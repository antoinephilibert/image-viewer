//! Lazily loaded folder tree and natural sort order.

use std::cmp::Ordering;
use std::path::PathBuf;

pub type NodeId = usize;

/// Raw entry read from disk (produced off the UI thread).
pub struct Entry {
    pub path: PathBuf,
    pub name: String,
    pub is_dir: bool,
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

/// Order: folders first, then case-insensitive natural order.
pub fn sort_entries(entries: &mut [Entry]) {
    entries.sort_by(|a, b| {
        b.is_dir
            .cmp(&a.is_dir)
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

    #[test]
    fn dirs_come_first() {
        let e = |name: &str, is_dir| Entry {
            path: PathBuf::from(name),
            name: name.into(),
            is_dir,
        };
        let mut v = vec![
            e("b.png", false),
            e("z", true),
            e("a.png", false),
            e("A", true),
        ];
        sort_entries(&mut v);
        let names: Vec<_> = v.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["A", "z", "a.png", "b.png"]);
    }

    #[test]
    fn collapsed_children_are_not_rows() {
        let mut t = Tree::new(PathBuf::from("/r"));
        let e = |n: &str, d| Entry {
            path: PathBuf::from(n),
            name: n.into(),
            is_dir: d,
        };
        t.set_children(0, vec![e("d", true), e("x.png", false), e(".h.png", false)]);
        assert_eq!(t.visible_rows(false), vec![0, 1, 2]);
        assert_eq!(t.visible_rows(true), vec![0, 1, 2, 3]);
        t.nodes[1].children = Some(vec![]);
        t.nodes[1].expanded = true;
        assert_eq!(t.visible_rows(false), vec![0, 1, 2]);
    }
}
