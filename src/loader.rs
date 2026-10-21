//! Pool de workers, file de priorités, annulation par génération et caches LRU.
//!
//! Aucun décodage ni lecture de dossier ne se fait sur le thread UI : l'UI
//! soumet des requêtes, les workers renvoient des réponses par channel puis
//! appellent `ctx.request_repaint()`.

use std::cmp::Ordering;
use std::collections::{BinaryHeap, HashMap, HashSet};
use std::hash::Hash;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Condvar, Mutex, RwLock};

use eframe::egui::Context;

use crate::formats::{self, Decoded};
use crate::tree::{Entry, NodeId, sort_entries};

/// Priorité d'une requête; le premier variant passe en premier.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Priority {
    Dir,
    Current,
    Neighbor1,
    Neighbor2,
}

pub enum Job {
    Dir { epoch: u64, id: NodeId, path: PathBuf },
    Full { path: PathBuf, max_side: usize },
}

impl Job {
    fn is_image(&self) -> bool {
        matches!(self, Job::Full { .. })
    }
}

pub enum Response {
    Dir {
        epoch: u64,
        id: NodeId,
        entries: Vec<Entry>,
    },
    Full {
        path: PathBuf,
        result: Result<Decoded, String>,
    },
}

struct Item {
    priority: Priority,
    seq: u64,
    generation: u64,
    job: Job,
}

impl PartialEq for Item {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}
impl Eq for Item {}
impl PartialOrd for Item {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for Item {
    /// `BinaryHeap` est un max-heap : le plus prioritaire (puis le plus ancien)
    /// doit être le plus grand.
    fn cmp(&self, other: &Self) -> Ordering {
        other
            .priority
            .cmp(&self.priority)
            .then_with(|| other.seq.cmp(&self.seq))
    }
}

/// File de priorités bloquante partagée par les workers.
#[derive(Default)]
struct Queue {
    heap: Mutex<BinaryHeap<Item>>,
    ready: Condvar,
}

impl Queue {
    fn push(&self, item: Item) {
        self.heap.lock().unwrap().push(item);
        self.ready.notify_one();
    }

    fn pop_blocking(&self) -> Item {
        let mut heap = self.heap.lock().unwrap();
        loop {
            if let Some(item) = heap.pop() {
                return item;
            }
            heap = self.ready.wait(heap).unwrap();
        }
    }

    /// Retire les requêtes d'image en attente (elles seront ré-émises).
    fn clear_images(&self) {
        self.heap.lock().unwrap().retain(|i| !i.job.is_image());
    }
}

struct Shared {
    queue: Queue,
    generation: AtomicU64,
    /// Chemins encore voulus : une requête ou un résultat hors de cet
    /// ensemble est abandonné.
    wanted: RwLock<HashSet<PathBuf>>,
    in_flight: Mutex<HashSet<PathBuf>>,
}

impl Shared {
    fn is_wanted(&self, path: &Path) -> bool {
        self.wanted.read().unwrap().contains(path)
    }
}

pub struct Loader {
    shared: Arc<Shared>,
    rx: Receiver<Response>,
    seq: u64,
}

impl Loader {
    pub fn new(ctx: &Context) -> Self {
        let shared = Arc::new(Shared {
            queue: Queue::default(),
            generation: AtomicU64::new(0),
            wanted: RwLock::new(HashSet::new()),
            in_flight: Mutex::new(HashSet::new()),
        });
        let (tx, rx) = channel();
        let workers = std::thread::available_parallelism()
            .map_or(2, |n| n.get().saturating_sub(1))
            .clamp(2, 6);
        for _ in 0..workers {
            let (shared, tx, ctx) = (shared.clone(), tx.clone(), ctx.clone());
            std::thread::spawn(move || worker(&shared, &tx, &ctx));
        }
        Self { shared, rx, seq: 0 }
    }

    pub fn try_recv(&self) -> Option<Response> {
        self.rx.try_recv().ok()
    }

    pub fn request_dir(&mut self, epoch: u64, id: NodeId, path: PathBuf) {
        self.push(Priority::Dir, 0, Job::Dir { epoch, id, path });
    }

    /// Redéfinit les images voulues (nouvelle génération). Les requêtes en
    /// attente sont ré-émises avec leur nouvelle priorité; celles déjà en
    /// cours de décodage et toujours voulues continuent sans doublon.
    pub fn retarget(
        &mut self,
        targets: &[(PathBuf, Priority)],
        max_side: usize,
        is_cached: impl Fn(&Path) -> bool,
    ) {
        let generation = self.shared.generation.fetch_add(1, AtomicOrdering::SeqCst) + 1;
        *self.shared.wanted.write().unwrap() = targets.iter().map(|(p, _)| p.clone()).collect();
        self.shared.queue.clear_images();
        let in_flight = self.shared.in_flight.lock().unwrap().clone();
        for (path, prio) in targets {
            if is_cached(path) || in_flight.contains(path) {
                continue;
            }
            let job = Job::Full {
                path: path.clone(),
                max_side,
            };
            self.push(*prio, generation, job);
        }
    }

    /// Une réponse d'image est utile si son chemin est encore voulu.
    pub fn is_wanted(&self, path: &Path) -> bool {
        self.shared.is_wanted(path)
    }

    fn push(&mut self, priority: Priority, generation: u64, job: Job) {
        self.seq += 1;
        self.shared.queue.push(Item {
            priority,
            seq: self.seq,
            generation,
            job,
        });
    }
}

fn worker(shared: &Shared, tx: &Sender<Response>, ctx: &Context) {
    loop {
        let item = shared.queue.pop_blocking();
        let response = match item.job {
            Job::Dir { epoch, id, path } => {
                let entries = read_dir(&path);
                Some(Response::Dir { epoch, id, entries })
            }
            Job::Full { path, max_side } => {
                // Abandon avant décodage : génération périmée ou image plus voulue.
                if item.generation != shared.generation.load(AtomicOrdering::SeqCst)
                    || !shared.is_wanted(&path)
                {
                    continue;
                }
                shared.in_flight.lock().unwrap().insert(path.clone());
                let result = formats::decode(&path, max_side);
                shared.in_flight.lock().unwrap().remove(&path);
                // Abandon après décodage.
                shared.is_wanted(&path).then_some(Response::Full { path, result })
            }
        };
        if let Some(r) = response {
            if tx.send(r).is_err() {
                return;
            }
            ctx.request_repaint();
        }
    }
}

/// Lit un dossier : ne garde que les dossiers et les images supportées.
fn read_dir(path: &Path) -> Vec<Entry> {
    let Ok(rd) = std::fs::read_dir(path) else {
        return vec![];
    };
    let mut entries: Vec<Entry> = rd
        .filter_map(Result::ok)
        .filter_map(|e| {
            let path = e.path();
            // `fs::metadata` suit les liens symboliques.
            let is_dir = std::fs::metadata(&path).ok()?.is_dir();
            if !is_dir && formats::kind_of(&path).is_none() {
                return None;
            }
            Some(Entry {
                name: e.file_name().to_string_lossy().into_owned(),
                path,
                is_dir,
            })
        })
        .collect();
    sort_entries(&mut entries);
    entries
}

/// Cache LRU borné par un poids (nombre d'éléments ou octets selon l'usage).
pub struct Lru<K, V> {
    map: HashMap<K, Slot<V>>,
    budget: usize,
    used: usize,
    tick: u64,
}

struct Slot<V> {
    value: V,
    weight: usize,
    last_used: u64,
}

impl<K: Hash + Eq + Clone, V> Lru<K, V> {
    pub fn new(budget: usize) -> Self {
        Self {
            map: HashMap::new(),
            budget,
            used: 0,
            tick: 0,
        }
    }

    pub fn contains<Q>(&self, key: &Q) -> bool
    where
        K: std::borrow::Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        self.map.contains_key(key)
    }

    pub fn get<Q>(&mut self, key: &Q) -> Option<&V>
    where
        K: std::borrow::Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        self.tick += 1;
        let tick = self.tick;
        self.map.get_mut(key).map(|s| {
            s.last_used = tick;
            &s.value
        })
    }

    /// Insère puis évince les moins récemment utilisés jusqu'à respecter le
    /// budget (l'élément qui vient d'être inséré est toujours conservé).
    pub fn put(&mut self, key: K, value: V, weight: usize) {
        self.tick += 1;
        if let Some(old) = self.map.remove(&key) {
            self.used -= old.weight;
        }
        self.used += weight;
        self.map.insert(
            key.clone(),
            Slot {
                value,
                weight,
                last_used: self.tick,
            },
        );
        while self.used > self.budget && self.map.len() > 1 {
            let oldest = self
                .map
                .iter()
                .filter(|(k, _)| **k != key)
                .min_by_key(|(_, s)| s.last_used)
                .map(|(k, _)| k.clone());
            let Some(k) = oldest else { break };
            if let Some(s) = self.map.remove(&k) {
                self.used -= s.weight;
            }
        }
    }

    pub fn clear(&mut self) {
        self.map.clear();
        self.used = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(priority: Priority, seq: u64) -> Item {
        Item {
            priority,
            seq,
            generation: 0,
            job: Job::Full {
                path: PathBuf::from(format!("{seq}")),
                max_side: 1,
            },
        }
    }

    #[test]
    fn heap_orders_by_priority_then_age() {
        let mut h = BinaryHeap::new();
        h.push(item(Priority::Neighbor2, 1));
        h.push(item(Priority::Neighbor2, 0));
        h.push(item(Priority::Current, 3));
        h.push(item(Priority::Neighbor1, 5));
        h.push(item(Priority::Neighbor1, 4));
        let order: Vec<u64> = std::iter::from_fn(|| h.pop()).map(|i| i.seq).collect();
        assert_eq!(order, [3, 4, 5, 0, 1]);
    }

    #[test]
    fn clear_images_keeps_dir_jobs() {
        let q = Queue::default();
        q.push(item(Priority::Current, 1));
        q.push(Item {
            priority: Priority::Dir,
            seq: 2,
            generation: 0,
            job: Job::Dir {
                epoch: 0,
                id: 0,
                path: PathBuf::new(),
            },
        });
        q.clear_images();
        let heap = q.heap.lock().unwrap();
        assert_eq!(heap.len(), 1);
        assert!(matches!(heap.peek().unwrap().job, Job::Dir { .. }));
    }

    #[test]
    fn lru_evicts_least_recently_used() {
        let mut c: Lru<&str, u32> = Lru::new(3);
        c.put("a", 1, 1);
        c.put("b", 2, 1);
        c.put("c", 3, 1);
        c.get("a");
        c.put("d", 4, 1);
        assert!(c.contains("a") && c.contains("c") && c.contains("d"));
        assert!(!c.contains("b"));
    }

    #[test]
    fn lru_respects_weight_budget_but_keeps_newest() {
        let mut c: Lru<&str, ()> = Lru::new(10);
        c.put("a", (), 6);
        c.put("b", (), 6);
        assert!(!c.contains("a") && c.contains("b"));
        c.put("huge", (), 50);
        assert!(c.contains("huge") && !c.contains("b"));
    }
}
