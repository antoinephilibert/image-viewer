//! Worker pool, priority queue, cancellation by generation and LRU caches.
//!
//! No decoding or folder reading happens on the UI thread: the UI submits
//! requests, the workers send responses back over a channel and then call
//! `ctx.request_repaint()`.

use std::cmp::Ordering;
use std::collections::{BinaryHeap, HashMap, HashSet};
use std::hash::Hash;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Condvar, Mutex, RwLock};

use eframe::egui::{ColorImage, Context};

use crate::formats::{self, Decoded};
use crate::thumbs;
use crate::tree::{Entry, NodeId, sort_entries};

/// Request priority; the first variant is served first.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Priority {
    Dir,
    Current,
    Neighbor1,
    Neighbor2,
    /// Visible thumbnail.
    Thumb,
    /// Thumbnail in the margin around the visible area.
    Rest,
}

pub enum Job {
    Dir {
        epoch: u64,
        id: NodeId,
        path: PathBuf,
    },
    /// `svg_side`: target longest side if the file is an SVG (ignored otherwise).
    Full {
        path: PathBuf,
        max_side: usize,
        svg_side: usize,
    },
    /// Re-rasterises an SVG at a new size (after a zoom change).
    Svg {
        path: PathBuf,
        side: usize,
    },
    Thumb {
        path: PathBuf,
    },
}

impl Job {
    fn is_full(&self) -> bool {
        matches!(self, Job::Full { .. } | Job::Svg { .. })
    }

    fn is_thumb(&self) -> bool {
        matches!(self, Job::Thumb { .. })
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
    Svg {
        path: PathBuf,
        result: Result<Decoded, String>,
    },
    Thumb {
        path: PathBuf,
        result: Result<ColorImage, String>,
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
    /// `BinaryHeap` is a max-heap: the highest priority (then the oldest)
    /// must compare as the greatest.
    fn cmp(&self, other: &Self) -> Ordering {
        other
            .priority
            .cmp(&self.priority)
            .then_with(|| other.seq.cmp(&self.seq))
    }
}

/// Blocking priority queue shared by the workers.
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

    /// Removes pending full-size requests (they will be re-issued).
    fn clear_full(&self) {
        self.heap.lock().unwrap().retain(|i| !i.job.is_full());
    }

    /// Removes pending thumbnail requests.
    fn clear_thumbs(&self) {
        self.heap.lock().unwrap().retain(|i| !i.job.is_thumb());
    }
}

struct Shared {
    queue: Queue,
    generation: AtomicU64,
    /// Paths still wanted: a request or result outside this set is
    /// dropped.
    wanted: RwLock<HashSet<PathBuf>>,
    in_flight: Mutex<HashSet<PathBuf>>,
    /// Same mechanism for thumbnails, with its own generation.
    thumb_generation: AtomicU64,
    wanted_thumbs: RwLock<HashSet<PathBuf>>,
    thumbs_in_flight: Mutex<HashSet<PathBuf>>,
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
            thumb_generation: AtomicU64::new(0),
            wanted_thumbs: RwLock::new(HashSet::new()),
            thumbs_in_flight: Mutex::new(HashSet::new()),
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

    /// Redefines the wanted images (new generation). Pending requests are
    /// re-issued with their new priority; those already being decoded and
    /// still wanted carry on without duplicates.
    pub fn retarget(
        &mut self,
        targets: &[(PathBuf, Priority)],
        max_side: usize,
        svg_side: usize,
        is_cached: impl Fn(&Path) -> bool,
    ) {
        let generation = self.shared.generation.fetch_add(1, AtomicOrdering::SeqCst) + 1;
        *self.shared.wanted.write().unwrap() = targets.iter().map(|(p, _)| p.clone()).collect();
        self.shared.queue.clear_full();
        let in_flight = self.shared.in_flight.lock().unwrap().clone();
        for (path, prio) in targets {
            if is_cached(path) || in_flight.contains(path) {
                continue;
            }
            let job = Job::Full {
                path: path.clone(),
                max_side,
                svg_side,
            };
            self.push(*prio, generation, job);
        }
    }

    /// Redefines the wanted thumbnails (visible, then margin): pending requests
    /// are cancelled and re-issued, those in progress and still wanted continue.
    pub fn retarget_thumbs(
        &mut self,
        targets: &[(PathBuf, Priority)],
        is_cached: impl Fn(&Path) -> bool,
    ) {
        let generation = self
            .shared
            .thumb_generation
            .fetch_add(1, AtomicOrdering::SeqCst)
            + 1;
        *self.shared.wanted_thumbs.write().unwrap() =
            targets.iter().map(|(p, _)| p.clone()).collect();
        self.shared.queue.clear_thumbs();
        let in_flight = self.shared.thumbs_in_flight.lock().unwrap().clone();
        for (path, prio) in targets {
            if is_cached(path) || in_flight.contains(path) {
                continue;
            }
            self.push(*prio, generation, Job::Thumb { path: path.clone() });
        }
    }

    pub fn is_thumb_wanted(&self, path: &Path) -> bool {
        self.shared.wanted_thumbs.read().unwrap().contains(path)
    }

    /// Requests a new rasterisation of an SVG (current image, top priority).
    pub fn request_svg(&mut self, path: PathBuf, side: usize) {
        let generation = self.shared.generation.load(AtomicOrdering::SeqCst);
        self.push(Priority::Current, generation, Job::Svg { path, side });
    }

    /// An image response is useful if its path is still wanted.
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
            Job::Thumb { path } => {
                let wanted = |p: &Path| shared.wanted_thumbs.read().unwrap().contains(p);
                if item.generation != shared.thumb_generation.load(AtomicOrdering::SeqCst)
                    || !wanted(&path)
                {
                    continue;
                }
                shared.thumbs_in_flight.lock().unwrap().insert(path.clone());
                let result = thumbs::load(&path);
                shared.thumbs_in_flight.lock().unwrap().remove(&path);
                wanted(&path).then_some(Response::Thumb { path, result })
            }
            Job::Dir { epoch, id, path } => {
                let entries = read_dir(&path);
                Some(Response::Dir { epoch, id, entries })
            }
            Job::Svg { path, side } => {
                if item.generation != shared.generation.load(AtomicOrdering::SeqCst)
                    || !shared.is_wanted(&path)
                {
                    continue;
                }
                let result = formats::rasterize_svg(&path, side);
                shared
                    .is_wanted(&path)
                    .then_some(Response::Svg { path, result })
            }
            Job::Full {
                path,
                max_side,
                svg_side,
            } => {
                // Abandon before decoding: stale generation or image no longer wanted.
                if item.generation != shared.generation.load(AtomicOrdering::SeqCst)
                    || !shared.is_wanted(&path)
                {
                    continue;
                }
                shared.in_flight.lock().unwrap().insert(path.clone());
                let result = if formats::kind_of(&path) == Some(formats::Kind::Svg) {
                    formats::rasterize_svg(&path, svg_side.min(max_side))
                } else {
                    formats::decode(&path, max_side)
                };
                shared.in_flight.lock().unwrap().remove(&path);
                // Abandon after decoding.
                shared
                    .is_wanted(&path)
                    .then_some(Response::Full { path, result })
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

/// Reads a folder, keeping only folders and supported images.
fn read_dir(path: &Path) -> Vec<Entry> {
    let Ok(rd) = std::fs::read_dir(path) else {
        return vec![];
    };
    let mut entries: Vec<Entry> = rd
        .filter_map(Result::ok)
        .filter_map(|e| {
            let path = e.path();
            // `fs::metadata` follows symlinks.
            let meta = std::fs::metadata(&path).ok()?;
            let is_dir = meta.is_dir();
            if !is_dir && formats::kind_of(&path).is_none() {
                return None;
            }
            Some(Entry {
                name: e.file_name().to_string_lossy().into_owned(),
                path,
                is_dir,
                modified: meta.modified().ok(),
            })
        })
        .collect();
    sort_entries(&mut entries);
    entries
}

/// LRU cache bounded by a weight (item count or bytes, depending on use).
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

    /// Inserts, then evicts the least recently used entries until the budget
    /// is met (the entry just inserted is always kept).
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
                svg_side: 1,
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
        q.clear_full();
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
