//! Brings the directory on disk in line with the tree after remote changes
//! (`docs/design.md` §6, "Apply").
//!
//! Works like a checkout: nodes that move or disappear are first parked in
//! a staging folder (so swaps and moves into fresh directories cannot
//! collide), removed nodes are deleted, then everything is placed top-down.
//! File content is written to a temporary file and renamed into place so
//! readers and watchers never see a half-written file.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use bytes::Bytes;
use rayon::prelude::*;
use rustfuse_store::ObjectStore;
use rustfuse_types::{ChunkHash, ChunkRef, Content, Entry, NodeId};

use crate::index::StatKey;
use crate::tree::{NodeChange, os_str};
use crate::{IoContext, Result, STATE_DIR};

/// Chunk data fetched per batch while writing files.
const FETCH_BATCH_BYTES: u64 = 256 << 20;

pub struct Applier<'a> {
    pub root: &'a Path,
    pub store: Arc<dyn ObjectStore>,
}

fn remove_any(path: &Path) -> std::io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(m) if m.is_dir() => fs::remove_dir_all(path),
        Ok(_) => fs::remove_file(path),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

fn exists(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok()
}

fn kind_matches(path: &Path, content: &Content) -> bool {
    match fs::symlink_metadata(path) {
        Ok(m) => match content {
            Content::Dir => m.is_dir(),
            Content::Symlink(_) => m.file_type().is_symlink(),
            _ => m.is_file(),
        },
        Err(_) => false,
    }
}

#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(mode & 0o777))
}

fn temp_path(dir: &Path, node: NodeId) -> PathBuf {
    dir.join(format!(".rustfuse-tmp-{:x}", node.0))
}

impl Applier<'_> {
    fn staging(&self) -> PathBuf {
        self.root.join(STATE_DIR).join("stage")
    }

    /// Applies the changes of one changeset. Returns the stat of every node
    /// written, so later scans know these files are unchanged.
    pub async fn apply(&self, changes: &[NodeChange]) -> Result<Vec<(NodeId, Option<StatKey>)>> {
        let stage = self.staging();
        fs::create_dir_all(&stage).at(&stage)?;
        let mut stats = Vec::new();

        // 1. Park nodes that move away or disappear, deepest first.
        let mut leaving: Vec<&NodeChange> = changes
            .iter()
            .filter(|c| {
                // Also park nodes whose directory changes identity at the
                // same path (directories merged by conflict resolution).
                c.old_path.is_some()
                    && (c.new.is_none()
                        || c.new_path != c.old_path
                        || c.old.as_ref().map(|e| e.parent) != c.new.as_ref().map(|e| e.parent))
            })
            .collect();
        leaving.sort_by_key(|c| std::cmp::Reverse(c.old_path.as_ref().unwrap().components().count()));
        let mut staged: HashSet<NodeId> = HashSet::new();
        for c in leaving {
            let from = self.root.join(c.old_path.as_ref().unwrap());
            if exists(&from) {
                let to = stage.join(format!("{:x}", c.node.0));
                remove_any(&to).at(&to)?;
                fs::rename(&from, &to).at(&from)?;
                staged.insert(c.node);
            }
        }

        // 2. Delete removed nodes.
        for c in changes.iter().filter(|c| c.new.is_none()) {
            stats.push((c.node, None));
            if staged.contains(&c.node) {
                let p = stage.join(format!("{:x}", c.node.0));
                remove_any(&p).at(&p)?;
            }
        }

        // 3. Directories top-down and parked nodes back into place, in
        //    order; then everything else in parallel.
        let mut placing: Vec<&NodeChange> = changes.iter().filter(|c| c.new.is_some()).collect();
        placing.sort_by_key(|c| c.new_path.as_ref().map_or(0, |p| p.components().count()));
        let mut dirs: Vec<(&NodeChange, PathBuf)> = Vec::new();
        let mut files: Vec<(&NodeChange, PathBuf)> = Vec::new();
        for c in placing {
            let entry = c.new.as_ref().unwrap();
            let Some(rel) = &c.new_path else { continue };
            let target = self.root.join(rel);
            if staged.contains(&c.node) {
                let from = stage.join(format!("{:x}", c.node.0));
                if exists(&target) {
                    remove_any(&target).at(&target)?;
                }
                fs::rename(&from, &target).at(&target)?;
            }
            if entry.content.is_dir() {
                if !kind_matches(&target, &entry.content) {
                    remove_any(&target).at(&target)?;
                    fs::create_dir(&target).at(&target)?;
                }
                dirs.push((c, target));
            } else {
                files.push((c, target));
            }
        }
        let placed: Vec<Result<Placed>> = files
            .into_par_iter()
            .map(|(c, target)| self.place(c, target))
            .collect();
        let mut to_fetch: Vec<(&NodeChange, PathBuf)> = Vec::new();
        for p in placed {
            match p? {
                Placed::Done(node, st) => stats.push((node, st)),
                Placed::Fetch(c, target) => to_fetch.push((c, target)),
            }
        }
        tracing::debug!(
            placed = stats.len(),
            fetch = to_fetch.len(),
            "apply: placed entries"
        );

        // 4. Fetch and write chunked files, bounding memory per batch.
        let mut batch: Vec<(&NodeChange, PathBuf)> = Vec::new();
        let mut batch_bytes = 0u64;
        for item in to_fetch {
            let size = match &item.0.new.as_ref().unwrap().content {
                Content::File { size, .. } => *size,
                _ => 0,
            };
            if !batch.is_empty() && batch_bytes + size > FETCH_BATCH_BYTES {
                stats.extend(self.write_batch(std::mem::take(&mut batch)).await?);
                batch_bytes = 0;
            }
            batch_bytes += size;
            batch.push(item);
        }
        if !batch.is_empty() {
            stats.extend(self.write_batch(batch).await?);
        }

        // 5. Directory permissions last, deepest first, so a read-only
        //    directory does not block writing its children.
        for (c, target) in dirs.iter().rev() {
            set_mode(target, c.new.as_ref().unwrap().mode).at(target)?;
            stats.push((c.node, stat_of(target)));
        }
        Ok(stats)
    }

    /// Writes one non-directory entry whose content is at hand (symlink,
    /// inline data, or only metadata changed), or says it needs fetching.
    fn place<'c>(&self, c: &'c NodeChange, target: PathBuf) -> Result<Placed<'c>> {
        let entry = c.new.as_ref().unwrap();
        let content_changed = match &c.old {
            Some(old) => !old.content.same_data(&entry.content),
            None => true,
        } || !kind_matches(&target, &entry.content);
        match &entry.content {
            Content::Dir => unreachable!("directories are placed in order"),
            Content::Symlink(dest) => {
                if content_changed {
                    remove_any(&target).at(&target)?;
                    std::os::unix::fs::symlink(os_str(dest), &target).at(&target)?;
                }
            }
            Content::Inline(data) => {
                if content_changed {
                    self.write_file(&target, c.node, entry, |f| f.write_all(data))?;
                } else {
                    finish_meta(&target, entry)?;
                }
            }
            Content::File { .. } => {
                if content_changed {
                    return Ok(Placed::Fetch(c, target));
                }
                finish_meta(&target, entry)?;
            }
        }
        Ok(Placed::Done(c.node, stat_of(&target)))
    }

    async fn write_batch(
        &self,
        batch: Vec<(&NodeChange, PathBuf)>,
    ) -> Result<Vec<(NodeId, Option<StatKey>)>> {
        let refs: Vec<ChunkRef> = batch
            .iter()
            .flat_map(|(c, _)| match &c.new.as_ref().unwrap().content {
                Content::File { chunks, .. } => chunks.clone(),
                _ => Vec::new(),
            })
            .collect();
        let t0 = std::time::Instant::now();
        let data: HashMap<ChunkHash, Bytes> = rustfuse_chunk::fetch(self.store.clone(), &refs).await?;
        tracing::debug!(
            chunks = refs.len(),
            ms = t0.elapsed().as_millis() as u64,
            "apply: fetched"
        );
        batch
            .into_par_iter()
            .filter_map(|(c, target)| {
                let entry = c.new.as_ref().unwrap();
                let Content::File { chunks, .. } = &entry.content else {
                    return None;
                };
                let written = self.write_file(&target, c.node, entry, |f| {
                    for r in chunks {
                        f.write_all(&data[&r.hash])?;
                    }
                    Ok(())
                });
                Some(written.map(|()| (c.node, stat_of(&target))))
            })
            .collect()
    }

    fn write_file(
        &self,
        target: &Path,
        node: NodeId,
        entry: &Entry,
        fill: impl FnOnce(&mut fs::File) -> std::io::Result<()>,
    ) -> Result<()> {
        let dir = target.parent().unwrap_or(self.root);
        let tmp = temp_path(dir, node);
        let mut f = fs::File::create(&tmp).at(&tmp)?;
        fill(&mut f).at(&tmp)?;
        f.set_modified(entry.mtime.into()).at(&tmp)?;
        drop(f);
        set_mode(&tmp, entry.mode).at(&tmp)?;
        if fs::symlink_metadata(target).is_ok_and(|m| m.is_dir()) {
            fs::remove_dir_all(target).at(target)?;
        }
        fs::rename(&tmp, target).at(target)?;
        Ok(())
    }
}

enum Placed<'c> {
    Done(NodeId, Option<StatKey>),
    Fetch(&'c NodeChange, PathBuf),
}

fn finish_meta(target: &Path, entry: &Entry) -> Result<()> {
    // futimens works on a read-only descriptor, so read-only files are fine.
    let f = fs::File::open(target).at(target)?;
    f.set_modified(entry.mtime.into()).at(target)?;
    set_mode(target, entry.mode).at(target)?;
    Ok(())
}

fn stat_of(path: &Path) -> Option<StatKey> {
    fs::symlink_metadata(path).ok().map(|m| StatKey::of(&m))
}
