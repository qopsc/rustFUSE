//! In-memory model of a volume: nodes by id plus a name index per directory.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use rustfuse_types::{Entry, NodeId, Op, Snapshot};

/// Name a node gets when two nodes end a changeset claiming the same path.
/// Only reachable with concurrent writers (Phase 2 handles these properly).
fn collision_name(name: &[u8], node: NodeId) -> Vec<u8> {
    let mut n = name.to_vec();
    n.extend_from_slice(format!(".collision-{:x}", node.0 as u64).as_bytes());
    n
}

#[derive(Clone, Debug, Default)]
pub struct Tree {
    nodes: HashMap<NodeId, Entry>,
    children: HashMap<NodeId, BTreeMap<Vec<u8>, NodeId>>,
}

/// How a changeset changed one node.
#[derive(Clone, Debug)]
pub struct NodeChange {
    pub node: NodeId,
    pub old: Option<Entry>,
    pub old_path: Option<PathBuf>,
    pub new: Option<Entry>,
    pub new_path: Option<PathBuf>,
}

impl Tree {
    pub fn from_entries(entries: impl IntoIterator<Item = (NodeId, Entry)>) -> Tree {
        let mut t = Tree::default();
        for (id, e) in entries {
            t.link(id, &e);
            t.nodes.insert(id, e);
        }
        t
    }

    pub fn from_snapshot(s: &Snapshot) -> Tree {
        Tree::from_entries(s.nodes.iter().cloned())
    }

    pub fn snapshot(&self, seq: u64) -> Snapshot {
        let mut nodes: Vec<_> = self.nodes.iter().map(|(k, v)| (*k, v.clone())).collect();
        nodes.sort_by_key(|(k, _)| *k);
        Snapshot {
            format: rustfuse_types::FORMAT_VERSION,
            seq,
            nodes,
        }
    }

    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    pub fn get(&self, id: NodeId) -> Option<&Entry> {
        self.nodes.get(&id)
    }

    pub fn iter(&self) -> impl Iterator<Item = (&NodeId, &Entry)> {
        self.nodes.iter()
    }

    pub fn child(&self, parent: NodeId, name: &[u8]) -> Option<NodeId> {
        self.children.get(&parent)?.get(name).copied()
    }

    pub fn children(&self, parent: NodeId) -> impl Iterator<Item = (&Vec<u8>, &NodeId)> {
        self.children.get(&parent).into_iter().flat_map(|m| m.iter())
    }

    /// Path of a node relative to the volume root.
    pub fn path(&self, mut id: NodeId) -> Option<PathBuf> {
        let mut parts = Vec::new();
        while id != NodeId::ROOT {
            let e = self.nodes.get(&id)?;
            parts.push(e.name.as_slice());
            id = e.parent;
            if parts.len() > 4096 {
                return None; // cycle guard
            }
        }
        let mut p = PathBuf::new();
        for part in parts.iter().rev() {
            p.push(os_str(part));
        }
        Some(p)
    }

    /// Resolves a relative path to a node (root for the empty path).
    pub fn resolve(&self, path: &Path) -> Option<NodeId> {
        let mut id = NodeId::ROOT;
        for c in path.components() {
            id = self.child(id, name_bytes(c.as_os_str()))?;
        }
        Some(id)
    }

    /// All nodes below `id`, deepest first.
    pub fn descendants(&self, id: NodeId) -> Vec<NodeId> {
        let mut out = Vec::new();
        let mut stack = vec![id];
        while let Some(n) = stack.pop() {
            for (_, c) in self.children(n) {
                out.push(*c);
                stack.push(*c);
            }
        }
        out.reverse();
        out
    }

    fn link(&mut self, id: NodeId, e: &Entry) {
        let siblings = self.children.entry(e.parent).or_default();
        siblings.insert(e.name.clone(), id);
    }

    fn unlink(&mut self, id: NodeId, e: &Entry) {
        if let Some(siblings) = self.children.get_mut(&e.parent)
            && siblings.get(&e.name) == Some(&id)
        {
            siblings.remove(&e.name);
        }
    }

    /// Applies a batch of ops and reports each touched node's before/after
    /// state. Intermediate states may briefly put two nodes at one name (for
    /// example a swap); any still colliding at the end are renamed.
    pub fn apply(&mut self, ops: &[Op]) -> Vec<NodeChange> {
        let mut before: HashMap<NodeId, (Option<Entry>, Option<PathBuf>)> = HashMap::new();
        let mut order = Vec::new();
        let mut displaced: Vec<NodeId> = Vec::new();
        for op in ops {
            let id = match op {
                Op::Upsert { node, .. } | Op::Remove { node } => *node,
            };
            if id == NodeId::ROOT {
                continue;
            }
            before.entry(id).or_insert_with(|| {
                order.push(id);
                (self.nodes.get(&id).cloned(), self.path(id))
            });
            match op {
                Op::Upsert { node, entry } => {
                    if let Some(old) = self.nodes.get(node).cloned() {
                        self.unlink(*node, &old);
                    }
                    if let Some(other) = self.child(entry.parent, &entry.name)
                        && other != *node
                    {
                        displaced.push(other);
                    }
                    self.link(*node, entry);
                    self.nodes.insert(*node, entry.clone());
                }
                Op::Remove { node } => {
                    if let Some(old) = self.nodes.remove(node) {
                        self.unlink(*node, &old);
                    }
                }
            }
        }
        // Re-link nodes pushed out of their name that were not moved since.
        for id in displaced {
            let Some(e) = self.nodes.get(&id).cloned() else {
                continue;
            };
            if self.child(e.parent, &e.name) == Some(id) {
                continue;
            }
            let mut e2 = e;
            if self.child(e2.parent, &e2.name).is_some() {
                e2.name = collision_name(&e2.name, id);
                before.entry(id).or_insert_with(|| {
                    order.push(id);
                    (self.nodes.get(&id).cloned(), self.path(id))
                });
            }
            self.link(id, &e2);
            self.nodes.insert(id, e2);
        }
        order
            .into_iter()
            .map(|id| {
                let (old, old_path) = before.remove(&id).unwrap();
                NodeChange {
                    node: id,
                    old,
                    old_path,
                    new: self.nodes.get(&id).cloned(),
                    new_path: self.path(id),
                }
            })
            .collect()
    }
}

#[cfg(unix)]
pub fn os_str(b: &[u8]) -> &std::ffi::OsStr {
    use std::os::unix::ffi::OsStrExt;
    std::ffi::OsStr::from_bytes(b)
}

#[cfg(unix)]
pub fn name_bytes(s: &std::ffi::OsStr) -> &[u8] {
    use std::os::unix::ffi::OsStrExt;
    s.as_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustfuse_types::{Content, Timestamp};

    fn entry(parent: NodeId, name: &str, content: Content) -> Entry {
        Entry {
            parent,
            name: name.as_bytes().to_vec(),
            mode: 0o644,
            mtime: Timestamp(0),
            content,
        }
    }

    fn up(id: u128, parent: u128, name: &str) -> Op {
        Op::Upsert {
            node: NodeId(id),
            entry: entry(
                NodeId(parent),
                name,
                if id < 10 {
                    Content::Dir
                } else {
                    Content::Inline(vec![])
                },
            ),
        }
    }

    #[test]
    fn paths_and_moves() {
        let mut t = Tree::default();
        t.apply(&[up(1, 0, "src"), up(11, 1, "main.rs"), up(2, 0, "docs")]);
        assert_eq!(t.path(NodeId(11)).unwrap(), PathBuf::from("src/main.rs"));
        assert_eq!(t.resolve(Path::new("src/main.rs")), Some(NodeId(11)));

        let changes = t.apply(&[up(1, 2, "code")]);
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].old_path.as_deref(), Some(Path::new("src")));
        assert_eq!(changes[0].new_path.as_deref(), Some(Path::new("docs/code")));
        assert_eq!(t.path(NodeId(11)).unwrap(), PathBuf::from("docs/code/main.rs"));
        assert_eq!(t.descendants(NodeId(2)), vec![NodeId(11), NodeId(1)]);
    }

    #[test]
    fn swap_names() {
        let mut t = Tree::default();
        t.apply(&[up(11, 0, "a"), up(12, 0, "b")]);
        t.apply(&[up(11, 0, "b"), up(12, 0, "a")]);
        assert_eq!(t.resolve(Path::new("a")), Some(NodeId(12)));
        assert_eq!(t.resolve(Path::new("b")), Some(NodeId(11)));
    }

    #[test]
    fn collision_renames_loser() {
        let mut t = Tree::default();
        t.apply(&[up(11, 0, "a"), up(12, 0, "b")]);
        t.apply(&[up(12, 0, "a")]);
        assert_eq!(t.resolve(Path::new("a")), Some(NodeId(12)));
        let p = t.path(NodeId(11)).unwrap();
        assert!(p.to_str().unwrap().starts_with("a.collision-"));
    }

    #[test]
    fn remove_and_snapshot_round_trip() {
        let mut t = Tree::default();
        t.apply(&[up(1, 0, "d"), up(11, 1, "f"), Op::Remove { node: NodeId(11) }]);
        assert_eq!(t.len(), 1);
        let t2 = Tree::from_snapshot(&t.snapshot(5));
        assert_eq!(t2.resolve(Path::new("d")), Some(NodeId(1)));
        assert_eq!(t2.resolve(Path::new("d/f")), None);
    }
}
