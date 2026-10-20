//! In-memory model of a volume: nodes by id plus a name index per directory,
//! and the conflict rules that make every machine converge
//! (`docs/design.md` §7.2).
//!
//! Every machine applies the same ordered log with [`Tree::apply`], which is
//! deterministic, so all replicas end up with the same tree. The rules, per
//! part of a node:
//!
//! - **Location** (parent + name) and **permissions**: the later version
//!   wins.
//! - **Content**: a change based on the current version applies. Concurrent
//!   changes with the same bytes merge silently; otherwise the later one
//!   stays and the other is kept next to it as a conflict copy.
//! - **Remove**: applies only if nobody changed the node concurrently and
//!   the directory is empty, so edits beat deletes and a directory that
//!   gained a file elsewhere survives.
//! - **Two nodes at one path**: directories merge, files with the same bytes
//!   merge, anything else keeps the later one and renames the other.
//! - A move that would put a directory inside itself is ignored.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};

use rustfuse_types::{Entry, Fields, Node, NodeId, Op, Snapshot, Version, Versions};

#[derive(Clone, Debug, Default)]
pub struct Tree {
    nodes: HashMap<NodeId, Node>,
    children: HashMap<NodeId, BTreeMap<Vec<u8>, NodeId>>,
    /// Removed nodes with their last state.
    removed: HashMap<NodeId, Node>,
}

/// How one node differs between two trees, with paths in each.
#[derive(Clone, Debug)]
pub struct NodeChange {
    pub node: NodeId,
    pub old: Option<Entry>,
    pub old_path: Option<PathBuf>,
    pub new: Option<Entry>,
    pub new_path: Option<PathBuf>,
}

impl Tree {
    pub fn from_parts(
        nodes: impl IntoIterator<Item = (NodeId, Node)>,
        removed: impl IntoIterator<Item = (NodeId, Node)>,
    ) -> Tree {
        let mut t = Tree {
            removed: removed.into_iter().collect(),
            ..Tree::default()
        };
        for (id, n) in nodes {
            t.link(id, &n.entry);
            t.nodes.insert(id, n);
        }
        t
    }

    pub fn from_snapshot(s: &Snapshot) -> Tree {
        Tree::from_parts(s.nodes.iter().cloned(), s.removed.iter().cloned())
    }

    pub fn snapshot(&self, seq: u64) -> Snapshot {
        let sorted = |m: &HashMap<NodeId, Node>| {
            let mut v: Vec<_> = m.iter().map(|(k, v)| (*k, v.clone())).collect();
            v.sort_by_key(|(k, _)| *k);
            v
        };
        Snapshot {
            format: rustfuse_types::FORMAT_VERSION,
            seq,
            nodes: sorted(&self.nodes),
            removed: sorted(&self.removed),
        }
    }

    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    pub fn get(&self, id: NodeId) -> Option<&Entry> {
        self.nodes.get(&id).map(|n| &n.entry)
    }

    pub fn node(&self, id: NodeId) -> Option<&Node> {
        self.nodes.get(&id)
    }

    pub fn removed(&self, id: NodeId) -> Option<&Node> {
        self.removed.get(&id)
    }

    pub fn iter(&self) -> impl Iterator<Item = (&NodeId, &Node)> {
        self.nodes.iter()
    }

    pub fn child(&self, parent: NodeId, name: &[u8]) -> Option<NodeId> {
        self.children.get(&parent)?.get(name).copied()
    }

    pub fn children(&self, parent: NodeId) -> impl Iterator<Item = (&Vec<u8>, &NodeId)> {
        self.children.get(&parent).into_iter().flat_map(|m| m.iter())
    }

    fn has_children(&self, id: NodeId) -> bool {
        self.children.get(&id).is_some_and(|m| !m.is_empty())
    }

    /// Path of a node relative to the volume root.
    pub fn path(&self, mut id: NodeId) -> Option<PathBuf> {
        let mut parts = Vec::new();
        while id != NodeId::ROOT {
            let e = &self.nodes.get(&id)?.entry;
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

    /// Whether `id` is `ancestor` or lies below it.
    fn is_within(&self, mut id: NodeId, ancestor: NodeId) -> bool {
        for _ in 0..4096 {
            if id == ancestor {
                return true;
            }
            match self.nodes.get(&id) {
                Some(n) => id = n.entry.parent,
                None => return false,
            }
        }
        true
    }

    fn link(&mut self, id: NodeId, e: &Entry) -> Option<NodeId> {
        let siblings = self.children.entry(e.parent).or_default();
        siblings.insert(e.name.clone(), id).filter(|&old| old != id)
    }

    fn unlink(&mut self, id: NodeId, e: &Entry) {
        if let Some(siblings) = self.children.get_mut(&e.parent)
            && siblings.get(&e.name) == Some(&id)
        {
            siblings.remove(&e.name);
            if siblings.is_empty() {
                self.children.remove(&e.parent);
            }
        }
    }

    /// Applies one changeset (all ops carry version `v`) and returns every
    /// node it touched, in first-touched order.
    pub fn apply(&mut self, v: Version, ops: &[Op]) -> Vec<NodeId> {
        let mut cx = Apply {
            tree: self,
            v,
            touched: Vec::new(),
            seen: HashSet::new(),
            displaced: Vec::new(),
        };
        for op in ops {
            if op.node() == NodeId::ROOT {
                continue;
            }
            match op {
                Op::Upsert {
                    node,
                    entry,
                    base,
                    changed,
                } => cx.upsert(*node, entry, *base, *changed),
                Op::Remove { node, base } => cx.remove(*node, *base),
            }
        }
        cx.settle();
        cx.touched
    }

    /// Compares this tree with `target` on the given nodes.
    pub fn diff(&self, target: &Tree, ids: impl IntoIterator<Item = NodeId>) -> Vec<NodeChange> {
        let mut out = Vec::new();
        let mut seen = HashSet::new();
        for id in ids {
            if id == NodeId::ROOT || !seen.insert(id) {
                continue;
            }
            let old = self.get(id);
            let new = target.get(id);
            let (old_path, new_path) = (self.path(id), target.path(id));
            if old == new && old_path == new_path {
                continue;
            }
            out.push(NodeChange {
                node: id,
                old: old.cloned(),
                old_path,
                new: new.cloned(),
                new_path,
            });
        }
        out
    }

    /// Replaces a node's content in place (same bytes, stored elsewhere).
    pub fn set_content(&mut self, id: NodeId, content: rustfuse_types::Content) {
        if let Some(n) = self.nodes.get_mut(&id) {
            n.entry.content = content;
        }
    }

    /// Makes the given nodes (and their removal records) match `source`.
    /// The caller guarantees the trees differ on no other node.
    pub fn copy_from(&mut self, source: &Tree, ids: &HashSet<NodeId>) {
        for id in ids {
            if let Some(n) = self.nodes.remove(id) {
                self.unlink(*id, &n.entry);
            }
            self.removed.remove(id);
        }
        for id in ids {
            if let Some(n) = source.nodes.get(id) {
                self.link(*id, &n.entry);
                self.nodes.insert(*id, n.clone());
            }
            if let Some(n) = source.removed.get(id) {
                self.removed.insert(*id, n.clone());
            }
        }
    }
}

struct Apply<'a> {
    tree: &'a mut Tree,
    v: Version,
    touched: Vec<NodeId>,
    seen: HashSet<NodeId>,
    /// Nodes pushed out of their name by another node.
    displaced: Vec<NodeId>,
}

impl Apply<'_> {
    fn touch(&mut self, id: NodeId) {
        if self.seen.insert(id) {
            self.touched.push(id);
        }
    }

    /// Inserts or replaces a node and links it at its name.
    fn put(&mut self, id: NodeId, node: Node) {
        self.touch(id);
        if let Some(old) = self.tree.nodes.remove(&id) {
            self.tree.unlink(id, &old.entry);
        }
        self.tree.removed.remove(&id);
        if let Some(other) = self.tree.link(id, &node.entry) {
            self.displaced.push(other);
        }
        self.tree.nodes.insert(id, node);
    }

    fn drop_node(&mut self, id: NodeId) {
        self.touch(id);
        if let Some(n) = self.tree.nodes.remove(&id) {
            self.tree.unlink(id, &n.entry);
            self.tree.removed.insert(id, n);
        }
    }

    /// Returns an existing directory to hold children: `id` itself, revived
    /// from its removal record if needed, or the root as a last resort.
    fn ensure_dir(&mut self, id: NodeId, depth: usize) -> NodeId {
        if id == NodeId::ROOT || self.tree.nodes.get(&id).is_some_and(|n| n.entry.content.is_dir()) {
            return id;
        }
        match self.tree.removed.get(&id).cloned() {
            Some(mut n) if n.entry.content.is_dir() && depth < 4096 => {
                n.entry.parent = self.ensure_dir(n.entry.parent, depth + 1);
                self.put(id, n);
                id
            }
            _ => NodeId::ROOT,
        }
    }

    fn upsert(&mut self, id: NodeId, entry: &Entry, base: Option<Versions>, changed: Fields) {
        let v = self.v;
        let Some(cur) = self.tree.nodes.get(&id).cloned() else {
            // New, or removed concurrently: the change brings it back.
            let prior = self.tree.removed.get(&id).map(|n| n.versions);
            if prior.is_some_and(|p| {
                (!changed.contains(Fields::LOC) || p.loc >= v)
                    && (!changed.contains(Fields::DATA) || p.data >= v)
                    && (!changed.contains(Fields::META) || p.meta >= v)
            }) {
                return; // already applied before the removal (a re-sent changeset)
            }
            let mut versions = base.or(prior).unwrap_or(Versions::all(v));
            set_changed(&mut versions, changed, v);
            let mut entry = entry.clone();
            entry.parent = self.ensure_dir(entry.parent, 0);
            if self.tree.is_within(entry.parent, id) {
                entry.parent = NodeId::ROOT;
            }
            self.put(id, Node { entry, versions });
            return;
        };

        let base = base.unwrap_or_default();
        let mut next = cur.clone();
        let mut copy = None;
        if changed.contains(Fields::LOC) && v > cur.versions.loc {
            let parent = self.ensure_dir(entry.parent, 0);
            if !self.tree.is_within(parent, id) {
                next.entry.parent = parent;
                next.entry.name = entry.name.clone();
                next.versions.loc = v;
            }
        }
        if changed.contains(Fields::META) && v > cur.versions.meta {
            next.entry.mode = entry.mode;
            next.versions.meta = v;
        }
        if changed.contains(Fields::DATA) && v != cur.versions.data {
            if base.data == cur.versions.data || cur.entry.content.same_data(&entry.content) {
                // Clean change, or concurrent with identical bytes.
                if v > cur.versions.data {
                    next.entry.content = entry.content.clone();
                    next.entry.mtime = entry.mtime;
                    next.versions.data = v;
                }
            } else if v > cur.versions.data {
                copy = Some((cur.entry.content.clone(), cur.entry.mtime, cur.versions.data));
                next.entry.content = entry.content.clone();
                next.entry.mtime = entry.mtime;
                next.versions.data = v;
            } else {
                copy = Some((entry.content.clone(), entry.mtime, v));
            }
        }
        if next != cur {
            self.put(id, next.clone());
        }
        if let Some((content, mtime, lost)) = copy {
            let copy_id = derived_id(id, lost);
            if self.tree.nodes.contains_key(&copy_id) || self.tree.removed.contains_key(&copy_id) {
                return;
            }
            let node = Node {
                entry: Entry {
                    parent: next.entry.parent,
                    name: conflict_name(&next.entry.name, lost),
                    mode: next.entry.mode,
                    mtime,
                    content,
                },
                versions: Versions::all(lost),
            };
            self.put(copy_id, node);
        }
    }

    fn remove(&mut self, id: NodeId, base: Versions) {
        let Some(cur) = self.tree.nodes.get(&id) else {
            return;
        };
        // A concurrent change to the node, or a child added to a directory
        // elsewhere, wins over the removal.
        if cur.versions != base || self.tree.has_children(id) {
            return;
        }
        self.drop_node(id);
    }

    /// Resolves names claimed by more than one node.
    fn settle(&mut self) {
        while let Some(y) = self.displaced.pop() {
            let Some(yn) = self.tree.nodes.get(&y).cloned() else {
                continue;
            };
            let holder = self.tree.child(yn.entry.parent, &yn.entry.name);
            let x = match holder {
                Some(x) if x == y => continue,
                Some(x) => x,
                None => {
                    self.put(y, yn);
                    continue;
                }
            };
            let xn = self.tree.nodes[&x].clone();
            // The later location wins; on a tie the node linked last keeps it.
            let (winner, loser) = if yn.versions.loc > xn.versions.loc {
                (y, x)
            } else {
                (x, y)
            };
            if winner == y {
                self.touch(x);
                self.tree.link(y, &yn.entry);
            }
            self.resolve_loser(winner, loser);
        }
    }

    fn resolve_loser(&mut self, winner: NodeId, loser: NodeId) {
        let wn = self.tree.nodes[&winner].clone();
        let ln = self.tree.nodes[&loser].clone();
        if wn.entry.content.is_dir() && ln.entry.content.is_dir() {
            // Same directory created on two machines: merge the contents.
            let kids: Vec<NodeId> = self.tree.children(loser).map(|(_, c)| *c).collect();
            for k in kids {
                let mut kn = self.tree.nodes[&k].clone();
                kn.entry.parent = winner;
                self.put(k, kn);
            }
            self.forget(loser, ln);
        } else if !wn.entry.content.is_dir()
            && !ln.entry.content.is_dir()
            && std::mem::discriminant(&wn.entry.content) == std::mem::discriminant(&ln.entry.content)
            && wn.entry.content.same_data(&ln.entry.content)
        {
            self.forget(loser, ln);
        } else {
            let mut n = ln;
            n.entry.name = conflict_name(&n.entry.name, n.versions.loc);
            if self.tree.child(n.entry.parent, &n.entry.name).is_some() {
                n.entry
                    .name
                    .extend_from_slice(format!("-{:x}", loser.0 as u64).as_bytes());
            }
            self.touch(loser);
            self.tree.nodes.insert(loser, n.clone());
            if let Some(other) = self.tree.link(loser, &n.entry) {
                self.displaced.push(other);
            }
        }
    }

    /// Removes a node that lost a name contest (its entry no longer owns the
    /// name, so it must not be unlinked by name).
    fn forget(&mut self, id: NodeId, n: Node) {
        self.touch(id);
        self.tree.nodes.remove(&id);
        self.tree.children.remove(&id);
        self.tree.removed.insert(id, n);
    }
}

fn set_changed(versions: &mut Versions, changed: Fields, v: Version) {
    if changed.contains(Fields::LOC) {
        versions.loc = v;
    }
    if changed.contains(Fields::DATA) {
        versions.data = v;
    }
    if changed.contains(Fields::META) {
        versions.meta = v;
    }
}

/// Stable id for the conflict copy of `node`'s content at version `v`, the
/// same on every machine.
fn derived_id(node: NodeId, v: Version) -> NodeId {
    let mut buf = Vec::with_capacity(40);
    buf.extend_from_slice(&node.0.to_le_bytes());
    buf.extend_from_slice(&v.time.0.to_le_bytes());
    buf.extend_from_slice(&v.replica.0.to_le_bytes());
    let h = rustfuse_chunk::hash(&buf);
    NodeId(u128::from_le_bytes(h.0[..16].try_into().unwrap()))
}

/// `name.conflict-<machine>-<UTC time>.ext`, e.g.
/// `notes.conflict-1a2b3c4d-20261019-151700.txt`.
pub fn conflict_name(name: &[u8], v: Version) -> Vec<u8> {
    let tag = format!(
        ".conflict-{:08x}-{}",
        v.replica.0 as u32,
        utc_stamp(v.time.0 / 1_000_000_000)
    );
    // Keep the extension so editors still recognise the file; a leading dot
    // (".gitignore") is part of the stem.
    let split = name.iter().rposition(|&b| b == b'.').filter(|&i| i > 0);
    let mut out = Vec::with_capacity(name.len() + tag.len());
    match split {
        Some(i) => {
            out.extend_from_slice(&name[..i]);
            out.extend_from_slice(tag.as_bytes());
            out.extend_from_slice(&name[i..]);
        }
        None => {
            out.extend_from_slice(name);
            out.extend_from_slice(tag.as_bytes());
        }
    }
    out
}

/// `YYYYMMDD-HHMMSS` for seconds since the Unix epoch, in UTC.
fn utc_stamp(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    // Civil-from-days (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}{m:02}{d:02}-{:02}{:02}{:02}",
        rem / 3600,
        rem / 60 % 60,
        rem % 60
    )
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
    use rustfuse_types::{Content, Hlc, ReplicaId, Timestamp};

    fn ver(t: u64, r: u64) -> Version {
        Version {
            time: Hlc(t),
            replica: ReplicaId(r),
        }
    }

    fn entry(parent: u128, name: &str, content: Content) -> Entry {
        Entry {
            parent: NodeId(parent),
            name: name.as_bytes().to_vec(),
            mode: 0o644,
            mtime: Timestamp(0),
            content,
        }
    }

    fn file(data: &str) -> Content {
        Content::Inline(data.as_bytes().to_vec())
    }

    fn create(id: u128, parent: u128, name: &str, content: Content) -> Op {
        Op::Upsert {
            node: NodeId(id),
            entry: entry(parent, name, content),
            base: None,
            changed: Fields::ALL,
        }
    }

    /// An op changing `changed` on node `id`, based on what `t` holds now.
    fn change(t: &Tree, id: u128, changed: Fields, f: impl FnOnce(&mut Entry)) -> Op {
        let n = t.node(NodeId(id)).unwrap();
        let mut e = n.entry.clone();
        f(&mut e);
        Op::Upsert {
            node: NodeId(id),
            entry: e,
            base: Some(n.versions),
            changed,
        }
    }

    fn remove(t: &Tree, id: u128) -> Op {
        Op::Remove {
            node: NodeId(id),
            base: t.node(NodeId(id)).unwrap().versions,
        }
    }

    fn content_at(t: &Tree, path: &str) -> Option<String> {
        let id = t.resolve(Path::new(path))?;
        match &t.get(id)?.content {
            Content::Inline(d) => Some(String::from_utf8(d.clone()).unwrap()),
            Content::Dir => Some("<dir>".into()),
            other => Some(format!("{other:?}")),
        }
    }

    fn listing(t: &Tree) -> Vec<String> {
        let mut out: Vec<String> = t
            .iter()
            .map(|(id, _)| {
                let p = t.path(*id).unwrap();
                format!("{}={}", p.display(), content_at(t, p.to_str().unwrap()).unwrap())
            })
            .collect();
        out.sort();
        out
    }

    /// Base tree: src/ with a.txt, plus docs/.
    fn base() -> Tree {
        let mut t = Tree::default();
        t.apply(
            ver(10, 1),
            &[
                create(1, 0, "src", Content::Dir),
                create(11, 1, "a.txt", file("one")),
                create(2, 0, "docs", Content::Dir),
            ],
        );
        t
    }

    #[test]
    fn paths_and_moves() {
        let mut t = base();
        assert_eq!(t.path(NodeId(11)).unwrap(), PathBuf::from("src/a.txt"));
        let op = change(&t, 1, Fields::LOC, |e| {
            e.parent = NodeId(2);
            e.name = b"code".to_vec();
        });
        t.apply(ver(20, 1), &[op]);
        assert_eq!(t.path(NodeId(11)).unwrap(), PathBuf::from("docs/code/a.txt"));
        assert_eq!(t.descendants(NodeId(2)), vec![NodeId(11), NodeId(1)]);
    }

    #[test]
    fn swap_names_in_one_changeset() {
        let mut t = Tree::default();
        t.apply(
            ver(1, 1),
            &[create(11, 0, "a", file("A")), create(12, 0, "b", file("B"))],
        );
        let o1 = change(&t, 11, Fields::LOC, |e| e.name = b"b".to_vec());
        let o2 = change(&t, 12, Fields::LOC, |e| e.name = b"a".to_vec());
        t.apply(ver(2, 1), &[o1, o2]);
        assert_eq!(listing(&t), ["a=B", "b=A"]);
    }

    #[test]
    fn concurrent_edits_keep_a_conflict_copy() {
        let t0 = base();
        let mine = change(&t0, 11, Fields::DATA, |e| e.content = file("mine"));
        let theirs = change(&t0, 11, Fields::DATA, |e| e.content = file("theirs"));
        // Same log on two machines: the later version wins the path.
        let mut t = t0.clone();
        t.apply(ver(30, 2), &[theirs]);
        t.apply(ver(20, 1), &[mine]);
        let l = listing(&t);
        assert!(l.contains(&"src/a.txt=theirs".to_string()), "{l:?}");
        let copy = l.iter().find(|s| s.contains(".conflict-00000001-")).unwrap();
        assert!(
            copy.starts_with("src/a.conflict-") && copy.ends_with(".txt=mine"),
            "{copy}"
        );
    }

    #[test]
    fn concurrent_edits_with_same_bytes_merge() {
        let t0 = base();
        let a = change(&t0, 11, Fields::DATA, |e| e.content = file("same"));
        let b = change(&t0, 11, Fields::DATA, |e| e.content = file("same"));
        let mut t = t0;
        t.apply(ver(20, 1), &[a]);
        t.apply(ver(21, 2), &[b]);
        assert_eq!(t.len(), 3);
        assert_eq!(content_at(&t, "src/a.txt").unwrap(), "same");
    }

    #[test]
    fn edit_beats_delete_in_either_order() {
        let t0 = base();
        let edit = change(&t0, 11, Fields::DATA, |e| e.content = file("kept"));
        let del = remove(&t0, 11);
        for order in [[&edit, &del], [&del, &edit]] {
            let mut t = t0.clone();
            t.apply(ver(20, 1), &[order[0].clone()]);
            t.apply(ver(21, 2), &[order[1].clone()]);
            assert_eq!(content_at(&t, "src/a.txt").unwrap(), "kept");
        }
    }

    #[test]
    fn rename_and_edit_both_apply() {
        let t0 = base();
        let mv = change(&t0, 11, Fields::LOC, |e| e.name = b"b.txt".to_vec());
        let edit = change(&t0, 11, Fields::DATA, |e| e.content = file("edited"));
        let mut t = t0;
        t.apply(ver(20, 1), &[mv]);
        t.apply(ver(21, 2), &[edit]);
        assert_eq!(listing(&t), ["docs=<dir>", "src/b.txt=edited", "src=<dir>"]);
    }

    #[test]
    fn delete_dir_vs_create_inside_keeps_dir() {
        let t0 = base();
        let del = [remove(&t0, 11), remove(&t0, 1)];
        let add = create(12, 1, "new.txt", file("new"));
        let mut t = t0.clone();
        t.apply(ver(20, 2), std::slice::from_ref(&add));
        t.apply(ver(21, 1), &del);
        assert_eq!(listing(&t), ["docs=<dir>", "src/new.txt=new", "src=<dir>"]);
        // Other order: the directory is already gone and comes back.
        let mut t = t0;
        t.apply(ver(21, 1), &del);
        t.apply(ver(20, 2), &[add]);
        assert_eq!(listing(&t), ["docs=<dir>", "src/new.txt=new", "src=<dir>"]);
    }

    #[test]
    fn same_directory_created_twice_merges() {
        let mut t = Tree::default();
        t.apply(
            ver(10, 1),
            &[
                create(101, 0, "objects", Content::Dir),
                create(111, 101, "x", file("X")),
            ],
        );
        t.apply(
            ver(11, 2),
            &[
                create(201, 0, "objects", Content::Dir),
                create(211, 201, "y", file("Y")),
                create(212, 201, "x", file("X")),
            ],
        );
        assert_eq!(listing(&t), ["objects/x=X", "objects/y=Y", "objects=<dir>"]);
    }

    #[test]
    fn two_new_files_at_one_path() {
        let mut t = Tree::default();
        t.apply(ver(10, 1), &[create(11, 0, "f", file("first"))]);
        t.apply(ver(11, 2), &[create(21, 0, "f", file("second"))]);
        let l = listing(&t);
        assert_eq!(l.len(), 2);
        assert!(l.contains(&"f=second".to_string()), "{l:?}");
        assert!(
            l.iter()
                .any(|s| s.starts_with("f.conflict-00000001-") && s.ends_with("=first"))
        );
    }

    #[test]
    fn crossing_moves_do_not_make_a_cycle() {
        let mut t = Tree::default();
        t.apply(
            ver(1, 1),
            &[create(1, 0, "a", Content::Dir), create(2, 0, "b", Content::Dir)],
        );
        let a_into_b = change(&t, 1, Fields::LOC, |e| e.parent = NodeId(2));
        let b_into_a = change(&t, 2, Fields::LOC, |e| e.parent = NodeId(1));
        t.apply(ver(5, 1), &[a_into_b]);
        t.apply(ver(6, 2), &[b_into_a]);
        assert_eq!(t.path(NodeId(1)).unwrap(), PathBuf::from("b/a"));
        assert_eq!(t.path(NodeId(2)).unwrap(), PathBuf::from("b"));
    }

    #[test]
    fn replaying_a_changeset_changes_nothing() {
        let t0 = base();
        let ops = [
            change(&t0, 11, Fields::DATA, |e| e.content = file("v2")),
            create(12, 1, "b.txt", file("b")),
        ];
        let mut t = t0;
        t.apply(ver(20, 1), &ops);
        let once = listing(&t);
        t.apply(ver(20, 1), &ops);
        assert_eq!(listing(&t), once);
    }

    #[test]
    fn diff_and_copy_from() {
        let a = base();
        let mut b = a.clone();
        let op = change(&b, 1, Fields::LOC, |e| e.name = b"lib".to_vec());
        let touched = b.apply(ver(20, 1), &[op]);
        let changes = a.diff(&b, touched.iter().copied());
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].old_path.as_deref(), Some(Path::new("src")));
        assert_eq!(changes[0].new_path.as_deref(), Some(Path::new("lib")));
        let mut c = a.clone();
        c.copy_from(&b, &touched.into_iter().collect());
        assert_eq!(listing(&c), listing(&b));
    }

    #[test]
    fn snapshot_round_trip_keeps_removed() {
        let mut t = base();
        let op = remove(&t, 11);
        t.apply(ver(20, 1), &[op]);
        let t2 = Tree::from_snapshot(&t.snapshot(5));
        assert_eq!(listing(&t2), listing(&t));
        assert!(t2.removed(NodeId(11)).is_some());
    }

    #[test]
    fn utc_stamps() {
        assert_eq!(utc_stamp(0), "19700101-000000");
        assert_eq!(utc_stamp(1_792_422_000), "20261019-150000");
        assert_eq!(
            conflict_name(b".gitignore", ver(0, 0xab)),
            b".gitignore.conflict-000000ab-19700101-000000".to_vec()
        );
    }
}
