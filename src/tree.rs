//! Lazily loaded folder tree and its sort order (newest first, natural order on ties).

use std::cmp::Ordering;
use std::path::PathBuf;
use std::time::SystemTime;

pub type NodeId = usize;

/// Raw entry read from disk (produced off the UI thread).
pub struct Entry {
    pub path: PathBuf,
    pub name: String,
    pub is_dir: bool,
    /// Modification time, used for sorting only. `None` if unavailable.
    pub modified: Option<SystemTime>,
}

pub struct Node {
    pub path: PathBuf,
    pub name: String,
    pub is_dir: bool,
    pub hidden: bool,
    pub depth: usize,
    pub parent: Option<NodeId>,
    /// `None` until the contents have been read.
    pub children: Option<Vec<NodeId>>,
    pub loading: bool,
    pub expanded: bool,
}

/// Arena tree: nodes are addressed by index.
pub struct Tree {
    pub nodes: Vec<Node>,
}

impl Tree {
    pub const ROOT: NodeId = 0;

    pub fn new(root: PathBuf) -> Self {
        let name = root
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| root.to_string_lossy().into_owned());
        Self {
            nodes: vec![Node {
                path: root,
                name,
                is_dir: true,
                hidden: false,
                depth: 0,
                parent: None,
                children: None,
                loading: false,
                expanded: true,
            }],
        }
    }

    /// Stores the entries read (already sorted) as children of `id`.
    pub fn set_children(&mut self, id: NodeId, entries: Vec<Entry>) {
        let depth = self.nodes[id].depth + 1;
        let mut ids = Vec::with_capacity(entries.len());
        for e in entries {
            ids.push(self.nodes.len());
            self.nodes.push(Node {
                hidden: e.name.starts_with('.'),
                path: e.path,
                name: e.name,
                is_dir: e.is_dir,
                depth,
                parent: Some(id),
                children: None,
                loading: false,
                expanded: false,
            });
        }
        let node = &mut self.nodes[id];
        node.children = Some(ids);
        node.loading = false;
    }

    /// Children of `id` that are visible given the hidden-files setting.
    pub fn visible_children(&self, id: NodeId, show_hidden: bool) -> Vec<NodeId> {
        self.nodes[id]
            .children
            .iter()
            .flatten()
            .copied()
            .filter(|&c| show_hidden || !self.nodes[c].hidden)
            .collect()
    }

    /// Flattens the expanded tree into a list of rows, in display order.
    pub fn visible_rows(&self, show_hidden: bool) -> Vec<NodeId> {
        let mut rows = Vec::new();
        self.push_rows(Self::ROOT, show_hidden, &mut rows);
        rows
    }

    fn push_rows(&self, id: NodeId, show_hidden: bool, rows: &mut Vec<NodeId>) {
        rows.push(id);
        let node = &self.nodes[id];
        if node.is_dir && node.expanded {
            for c in self.visible_children(id, show_hidden) {
                self.push_rows(c, show_hidden, rows);
            }
        }
    }
}

/// Order: files first, then folders; newest first within each group.
/// Entries without a date come last in their group; ties fall back to natural
/// order.
pub fn sort_entries(entries: &mut [Entry]) {
    entries.sort_by(|a, b| {
        a.is_dir
            .cmp(&b.is_dir)
            .then_with(|| b.modified.cmp(&a.modified))
            .then_with(|| natural_cmp(&a.name, &b.name))
    });
}

/// Case-insensitive natural order: `img2` before `img10`.
pub fn natural_cmp(a: &str, b: &str) -> Ordering {
    let mut a = a.chars().peekable();
    let mut b = b.chars().peekable();
    loop {
        match (a.peek().copied(), b.peek().copied()) {
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(ca), Some(cb)) if ca.is_ascii_digit() && cb.is_ascii_digit() => {
                let na = take_digits(&mut a);
                let nb = take_digits(&mut b);
                let (ta, tb) = (na.trim_start_matches('0'), nb.trim_start_matches('0'));
                let ord = ta.len().cmp(&tb.len()).then_with(|| ta.cmp(tb));
                if ord != Ordering::Equal {
                    return ord;
                }
            }
            (Some(ca), Some(cb)) => {
                let ord = ca.to_lowercase().cmp(cb.to_lowercase());
                if ord != Ordering::Equal {
                    return ord;
                }
                a.next();
                b.next();
            }
        }
    }
}

fn take_digits(it: &mut std::iter::Peekable<std::str::Chars<'_>>) -> String {
    let mut s = String::new();
    while let Some(&c) = it.peek() {
        if !c.is_ascii_digit() {
            break;
        }
        s.push(c);
        it.next();
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, UNIX_EPOCH};

    #[test]
    fn numbers_compare_by_value() {
        assert_eq!(natural_cmp("img2.jpg", "img10.jpg"), Ordering::Less);
        assert_eq!(natural_cmp("img10.jpg", "img2.jpg"), Ordering::Greater);
    }

    #[test]
    fn case_insensitive() {
        assert_eq!(natural_cmp("Apple", "apple"), Ordering::Equal);
        assert_eq!(natural_cmp("b", "A"), Ordering::Greater);
    }

    #[test]
    fn leading_zeros_ignored_in_value() {
        assert_eq!(natural_cmp("a007", "a8"), Ordering::Less);
    }

    #[test]
    fn prefix_sorts_first() {
        assert_eq!(natural_cmp("img", "img1"), Ordering::Less);
    }

    fn entry(name: &str, is_dir: bool, secs: Option<u64>) -> Entry {
        Entry {
            path: PathBuf::from(name),
            name: name.into(),
            is_dir,
            modified: secs.map(|s| UNIX_EPOCH + Duration::from_secs(s)),
        }
    }

    fn sorted_names(mut v: Vec<Entry>) -> Vec<String> {
        sort_entries(&mut v);
        v.into_iter().map(|e| e.name).collect()
    }

    #[test]
    fn files_then_dirs_newest_first() {
        let names = sorted_names(vec![
            entry("old_dir", true, Some(10)),
            entry("old.png", false, Some(20)),
            entry("new_dir", true, Some(40)),
            entry("new.png", false, Some(30)),
        ]);
        assert_eq!(names, ["new.png", "old.png", "new_dir", "old_dir"]);
    }

    #[test]
    fn same_date_falls_back_to_natural_order() {
        let names = sorted_names(vec![
            entry("img10.png", false, Some(5)),
            entry("img2.png", false, Some(5)),
        ]);
        assert_eq!(names, ["img2.png", "img10.png"]);
    }

    #[test]
    fn missing_date_comes_last_in_its_group() {
        let names = sorted_names(vec![
            entry("a.png", false, None),
            entry("b.png", false, Some(1)),
            entry("c", true, Some(2)),
        ]);
        assert_eq!(names, ["b.png", "a.png", "c"]);
    }

    #[test]
    fn collapsed_children_are_not_rows() {
        let mut t = Tree::new(PathBuf::from("/r"));
        let e = |n: &str, d| entry(n, d, None);
        t.set_children(0, vec![e("d", true), e("x.png", false), e(".h.png", false)]);
        assert_eq!(t.visible_rows(false), vec![0, 1, 2]);
        assert_eq!(t.visible_rows(true), vec![0, 1, 2, 3]);
        t.nodes[1].children = Some(vec![]);
        t.nodes[1].expanded = true;
        assert_eq!(t.visible_rows(false), vec![0, 1, 2]);
    }
}
