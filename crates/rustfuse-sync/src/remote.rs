//! Reading a volume straight from the bucket, without a synced directory:
//! the Mac app's bucket browser (`rustfuse ls`, `rustfuse get`).
//!
//! The tree is rebuilt the way `attach` starts: the latest snapshot plus
//! every changeset after it. Nothing is written to the bucket or the hub.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use rustfuse_store::ObjectStore;
use rustfuse_types::{ChunkRef, Content, NodeId};
use serde::Serialize;

use crate::tree::{Tree, os_str};
use crate::{Error, IoContext, Result, STATE_DIR, Volume};

/// Chunk data fetched at a time while downloading a file.
const WINDOW_BYTES: u64 = 64 << 20;

/// A volume's tree as the bucket holds it.
pub struct RemoteTree {
    tree: Tree,
    seq: u64,
    store: Arc<dyn ObjectStore>,
}

/// One entry of a remote directory.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct RemoteEntry {
    pub name: String,
    pub kind: crate::provider::Kind,
    pub size: u64,
    pub mode: u32,
    pub mtime_ns: i64,
    /// Symlink target.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
}

/// What [`RemoteTree::download`] wrote.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Downloaded {
    pub files: u64,
    pub dirs: u64,
    pub bytes: u64,
}

impl RemoteTree {
    /// Reads the volume's latest state from `store` (scoped to the volume).
    pub async fn load(store: Arc<dyn ObjectStore>) -> Result<RemoteTree> {
        let volume = Volume::new(store.clone());
        volume.check().await?;
        let (mut tree, mut seq) = match volume.latest_snapshot().await? {
            Some(snap) => (Tree::from_snapshot(&snap), snap.seq),
            None => (Tree::default(), 0),
        };
        for s in volume.changesets_after(seq).await? {
            let cs = volume.get_changeset(s).await?;
            tree.apply(cs.version, &cs.ops);
            seq = s;
        }
        Ok(RemoteTree { tree, seq, store })
    }

    /// The last changeset the tree includes.
    pub fn seq(&self) -> u64 {
        self.seq
    }

    /// The node at `path` (relative; empty for the root).
    fn find(&self, path: &Path) -> Result<NodeId> {
        let mut rel = PathBuf::new();
        for c in path.components() {
            match c {
                Component::Normal(n) => rel.push(n),
                Component::CurDir | Component::RootDir => {}
                _ => return Err(not_found(path)),
            }
        }
        if rel.starts_with(STATE_DIR) {
            return Err(not_found(path));
        }
        self.tree.resolve(&rel).ok_or_else(|| not_found(path))
    }

    /// The entries of the directory at `path`, sorted by name.
    pub fn list(&self, path: &Path) -> Result<Vec<RemoteEntry>> {
        let id = self.find(path)?;
        if id != NodeId::ROOT && !self.tree.get(id).is_some_and(|e| e.content.is_dir()) {
            return Err(Error::Io {
                path: path.to_owned(),
                source: std::io::Error::other("not a directory"),
            });
        }
        let mut out: Vec<RemoteEntry> = self
            .tree
            .children(id)
            .filter(|(name, _)| !(id == NodeId::ROOT && name.as_slice() == STATE_DIR.as_bytes()))
            .filter_map(|(name, child)| {
                let e = self.tree.get(*child)?;
                let (kind, size, target) = match &e.content {
                    Content::Dir => (crate::provider::Kind::Dir, 0, None),
                    Content::Inline(d) => (crate::provider::Kind::File, d.len() as u64, None),
                    Content::File { size, .. } => (crate::provider::Kind::File, *size, None),
                    Content::Symlink(t) => (
                        crate::provider::Kind::Symlink,
                        t.len() as u64,
                        Some(String::from_utf8_lossy(t).into_owned()),
                    ),
                };
                Some(RemoteEntry {
                    name: String::from_utf8_lossy(name).into_owned(),
                    kind,
                    size,
                    mode: e.mode,
                    mtime_ns: e.mtime.0,
                    target,
                })
            })
            .collect();
        out.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(out)
    }

    /// Copies the file, symlink or directory (with everything in it) at
    /// `path` to `dest`, which must not exist yet.
    pub async fn download(&self, path: &Path, dest: &Path) -> Result<Downloaded> {
        let id = self.find(path)?;
        if fs::symlink_metadata(dest).is_ok() {
            return Err(Error::Io {
                path: dest.to_owned(),
                source: std::io::Error::from(std::io::ErrorKind::AlreadyExists),
            });
        }
        let mut out = Downloaded::default();
        // Parents before children; directory modes last, deepest first, so
        // a read-only directory does not block writing into it.
        let mut stack = vec![(id, dest.to_owned())];
        let mut dirs = Vec::new();
        while let Some((id, target)) = stack.pop() {
            let content = match self.tree.get(id) {
                Some(e) => e.content.clone(),
                None if id == NodeId::ROOT => Content::Dir,
                None => continue,
            };
            match content {
                Content::Dir => {
                    fs::create_dir(&target).at(&target)?;
                    out.dirs += 1;
                    for (name, child) in self.tree.children(id) {
                        if id == NodeId::ROOT && name.as_slice() == STATE_DIR.as_bytes() {
                            continue;
                        }
                        stack.push((*child, target.join(os_str(name))));
                    }
                    dirs.push((id, target));
                }
                Content::Symlink(t) => {
                    std::os::unix::fs::symlink(os_str(&t), &target).at(&target)?;
                }
                Content::Inline(d) => {
                    fs::write(&target, &d).at(&target)?;
                    self.finish_file(id, &target)?;
                    out.files += 1;
                    out.bytes += d.len() as u64;
                }
                Content::File { size, chunks } => {
                    self.write_chunks(&chunks, &target).await?;
                    self.finish_file(id, &target)?;
                    out.files += 1;
                    out.bytes += size;
                }
            }
        }
        for (id, target) in dirs.into_iter().rev() {
            if let Some(e) = self.tree.get(id) {
                fs::set_permissions(&target, fs::Permissions::from_mode(e.mode)).at(&target)?;
            }
        }
        Ok(out)
    }

    async fn write_chunks(&self, chunks: &[ChunkRef], target: &Path) -> Result<()> {
        use std::io::Write;
        let mut f = fs::File::create(target).at(target)?;
        let mut start = 0;
        while start < chunks.len() {
            let mut end = start;
            let mut bytes = 0u64;
            while end < chunks.len() && (end == start || bytes + u64::from(chunks[end].len) <= WINDOW_BYTES) {
                bytes += u64::from(chunks[end].len);
                end += 1;
            }
            let window = &chunks[start..end];
            let data = rustfuse_chunk::fetch(self.store.clone(), window).await?;
            for r in window {
                let d = data
                    .get(&r.hash)
                    .ok_or_else(|| Error::Corrupt(format!("chunk {:?} missing", r.hash)))?;
                f.write_all(d).at(target)?;
            }
            start = end;
        }
        f.sync_all().at(target)
    }

    fn finish_file(&self, id: NodeId, target: &Path) -> Result<()> {
        let Some(e) = self.tree.get(id) else {
            return Ok(());
        };
        let f = fs::File::options().write(true).open(target).at(target)?;
        let t = std::time::UNIX_EPOCH + std::time::Duration::from_nanos(e.mtime.0.max(0) as u64);
        f.set_modified(t).at(target)?;
        f.set_permissions(fs::Permissions::from_mode(e.mode)).at(target)
    }
}

fn not_found(path: &Path) -> Error {
    Error::Io {
        path: path.to_owned(),
        source: std::io::Error::from(std::io::ErrorKind::NotFound),
    }
}
