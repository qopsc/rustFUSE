//! Finds what changed on disk since the index was last updated
//! (`docs/design.md` §6, "Capture").
//!
//! Compares directory listings and `lstat` results with the tree. Renames
//! and moves are recognised by inode identity, so a moved file keeps its
//! node id and its content is not re-uploaded.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use rustfuse_types::{Content, NodeId, ReplicaId, Timestamp};

use crate::ignore::Ignore;
use crate::index::StatKey;
use crate::tree::{Tree, name_bytes};
use crate::{IoContext, Result};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    File,
    Dir,
    Symlink,
}

impl Kind {
    fn of_content(c: &Content) -> Kind {
        match c {
            Content::Dir => Kind::Dir,
            Content::Inline(_) | Content::File { .. } => Kind::File,
            Content::Symlink(_) => Kind::Symlink,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Found {
    pub kind: Kind,
    pub stat: StatKey,
    pub mtime: Timestamp,
}

#[derive(Clone, Debug)]
pub enum Change {
    Upsert {
        node: NodeId,
        parent: NodeId,
        name: Vec<u8>,
        /// Path relative to the root.
        path: PathBuf,
        found: Found,
        /// The content must be (re)read from disk; otherwise the tree's
        /// current content is still valid.
        read: bool,
    },
    Remove {
        node: NodeId,
        path: PathBuf,
    },
}

impl Change {
    pub fn path(&self) -> &Path {
        match self {
            Change::Upsert { path, .. } | Change::Remove { path, .. } => path,
        }
    }
}

struct Candidate {
    parent: NodeId,
    name: Vec<u8>,
    path: PathBuf,
    found: Found,
}

/// One directory to compare: its node, relative path, and whether to
/// descend into subdirectories.
pub struct ScanRoot {
    pub node: NodeId,
    pub path: PathBuf,
    pub recursive: bool,
}

pub struct Scanner<'a> {
    pub root: &'a Path,
    pub tree: &'a Tree,
    pub stats: &'a HashMap<NodeId, StatKey>,
    pub ignore: &'a Ignore,
    pub replica: ReplicaId,
    pub counter: &'a mut u64,
}

fn list(abs: &Path, rel: &Path, ignore: &Ignore) -> Result<Vec<(Vec<u8>, Found)>> {
    let mut out = Vec::new();
    let rd = match std::fs::read_dir(abs) {
        Ok(rd) => rd,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(out),
        Err(e) => return Err(e).at(abs),
    };
    for ent in rd {
        let ent = ent.at(abs)?;
        let meta = match ent.metadata() {
            Ok(m) => m,
            // Vanished between listing and stat: it is simply gone.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e).at(ent.path()),
        };
        let ft = meta.file_type();
        let kind = if ft.is_dir() {
            Kind::Dir
        } else if ft.is_file() {
            Kind::File
        } else if ft.is_symlink() {
            Kind::Symlink
        } else {
            continue; // sockets, fifos and devices are not synced
        };
        let name = ent.file_name();
        if ignore.is_ignored(&rel.join(&name), kind == Kind::Dir) {
            continue;
        }
        let mtime = meta.modified().map(Timestamp::from).unwrap_or_default();
        out.push((
            name_bytes(&name).to_vec(),
            Found {
                kind,
                stat: StatKey::of(&meta),
                mtime,
            },
        ));
    }
    Ok(out)
}

impl Scanner<'_> {
    /// Compares the given directories with the tree and returns the changes,
    /// upserts parent-first followed by removals child-first.
    pub fn scan(&mut self, roots: Vec<ScanRoot>) -> Result<Vec<Change>> {
        let mut work: Vec<ScanRoot> = roots;
        let mut visited: HashSet<NodeId> = HashSet::new();
        let mut candidates: Vec<Candidate> = Vec::new();
        let mut removed: Vec<NodeId> = Vec::new();
        let mut removed_set: HashSet<NodeId> = HashSet::new();
        let mut changes: Vec<Change> = Vec::new();

        let tree = self.tree;
        let mark_removed = |node: NodeId, removed: &mut Vec<NodeId>, removed_set: &mut HashSet<NodeId>| {
            for n in std::iter::once(node).chain(tree.descendants(node)) {
                if removed_set.insert(n) {
                    removed.push(n);
                }
            }
        };

        loop {
            while let Some(ScanRoot {
                node,
                path,
                recursive,
            }) = work.pop()
            {
                if !visited.insert(node) {
                    continue;
                }
                let on_disk = list(&self.root.join(&path), &path, self.ignore)?;
                let mut seen: HashSet<&[u8]> = HashSet::new();
                for (name, found) in &on_disk {
                    seen.insert(name);
                    let child_path = path.join(crate::tree::os_str(name));
                    let existing = self.tree.child(node, name).filter(|c| !removed_set.contains(c));
                    match existing.map(|c| (c, self.tree.get(c).unwrap())) {
                        Some((child, entry)) if Kind::of_content(&entry.content) == found.kind => {
                            if found.kind == Kind::Dir {
                                if entry.mode != found.stat.mode & 0o777 {
                                    changes.push(Change::Upsert {
                                        node: child,
                                        parent: node,
                                        name: name.clone(),
                                        path: child_path.clone(),
                                        found: found.clone(),
                                        read: false,
                                    });
                                }
                                if recursive {
                                    work.push(ScanRoot {
                                        node: child,
                                        path: child_path,
                                        recursive: true,
                                    });
                                }
                            } else if !self.stats.get(&child).is_some_and(|s| s.unchanged(&found.stat)) {
                                changes.push(Change::Upsert {
                                    node: child,
                                    parent: node,
                                    name: name.clone(),
                                    path: child_path,
                                    found: found.clone(),
                                    read: true,
                                });
                            }
                        }
                        other => {
                            if let Some((child, _)) = other {
                                mark_removed(child, &mut removed, &mut removed_set);
                            }
                            candidates.push(Candidate {
                                parent: node,
                                name: name.clone(),
                                path: child_path,
                                found: found.clone(),
                            });
                        }
                    }
                }
                for (name, child) in self.tree.children(node) {
                    if !seen.contains(name.as_slice()) {
                        mark_removed(*child, &mut removed, &mut removed_set);
                    }
                }
            }

            if candidates.is_empty() {
                break;
            }

            // Match new names against removed nodes by inode: those are moves.
            let mut by_inode: HashMap<(u64, u64), NodeId> = HashMap::new();
            for n in &removed {
                if let Some(s) = self.stats.get(n)
                    && removed_set.contains(n)
                {
                    by_inode.insert((s.dev, s.ino), *n);
                }
            }
            for c in std::mem::take(&mut candidates) {
                let moved = by_inode
                    .remove(&(c.found.stat.dev, c.found.stat.ino))
                    .filter(|n| Kind::of_content(&self.tree.get(*n).unwrap().content) == c.found.kind);
                match moved {
                    Some(node) => {
                        removed_set.remove(&node);
                        let read = c.found.kind != Kind::Dir
                            && !self
                                .stats
                                .get(&node)
                                .is_some_and(|s| s.unchanged_after_move(&c.found.stat));
                        if c.found.kind == Kind::Dir {
                            // Its children are no longer removed; compare
                            // them at the new location instead.
                            for d in self.tree.descendants(node) {
                                removed_set.remove(&d);
                            }
                            work.push(ScanRoot {
                                node,
                                path: c.path.clone(),
                                recursive: true,
                            });
                        }
                        changes.push(Change::Upsert {
                            node,
                            parent: c.parent,
                            name: c.name,
                            path: c.path,
                            found: c.found,
                            read,
                        });
                    }
                    None => {
                        let node = NodeId::new(self.replica, *self.counter);
                        *self.counter += 1;
                        if c.found.kind == Kind::Dir {
                            let abs = self.root.join(&c.path);
                            for (name, found) in list(&abs, &c.path, self.ignore)? {
                                candidates.push(Candidate {
                                    parent: node,
                                    path: c.path.join(crate::tree::os_str(&name)),
                                    name,
                                    found,
                                });
                            }
                        }
                        changes.push(Change::Upsert {
                            node,
                            parent: c.parent,
                            name: c.name,
                            path: c.path,
                            read: c.found.kind != Kind::Dir,
                            found: c.found,
                        });
                    }
                }
            }
        }

        for n in removed.into_iter().filter(|n| removed_set.contains(n)) {
            changes.push(Change::Remove {
                node: n,
                path: self.tree.path(n).unwrap_or_default(),
            });
        }
        // Parents before children for upserts; children before parents for
        // removals.
        let depth = |c: &Change| c.path().components().count();
        changes.sort_by(|a, b| match (a, b) {
            (Change::Upsert { .. }, Change::Remove { .. }) => std::cmp::Ordering::Less,
            (Change::Remove { .. }, Change::Upsert { .. }) => std::cmp::Ordering::Greater,
            (Change::Upsert { .. }, Change::Upsert { .. }) => depth(a).cmp(&depth(b)),
            (Change::Remove { .. }, Change::Remove { .. }) => depth(b).cmp(&depth(a)),
        });
        Ok(changes)
    }
}
