//! Lazy directories (`rustfuse attach --lazy`, `docs/design.md` §2.1): file
//! content stays in the bucket until something opens the file through a
//! mount.
//!
//! The applier writes a *placeholder* instead of each chunked file: a
//! sparse file of the right size, mode and modification time, marked with
//! the [`XATTR`] extended attribute. The attribute names a small record in
//! `.rustfuse/lazy/` holding the file's content (its chunk list). `stat`
//! (and so `git status`) sees the real size and time without anything
//! being downloaded. Git's own data (`.git/`) is always written in full:
//! git and the git pass read it directly.
//!
//! The mount calls [`Hydrator::hydrate`] before opening a file. It fetches
//! the chunks, writes them into the placeholder through an open descriptor
//! (so a newer version the applier renames into place meanwhile is never
//! overwritten), restores the modification time and drops the attribute.
//! When capturing, a file still marked as a placeholder counts as holding
//! its recorded content, so a placeholder is never synced as zeros.
//!
//! A lazy directory must be used through `rustfuse mount` or `rustfuse
//! nfs`: programs reading it directly see placeholders as zeros.

use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{FileExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use rustfuse_store::ObjectStore;
use rustfuse_types::{ChunkRef, Content, NodeId};
use tracing::debug;

use crate::tree::Tree;
use crate::{Error, IoContext, Result, STATE_DIR};

/// Marks a placeholder; the value is its record's name.
pub const XATTR: &str = "user.rustfuse.lazy";
/// Chunk data fetched at a time while filling in a file.
const WINDOW_BYTES: u64 = 64 << 20;

fn records(root: &Path) -> PathBuf {
    root.join(STATE_DIR).join("lazy")
}

/// Whether a lazy directory is at `root`.
pub fn is_lazy(root: &Path) -> bool {
    records(root).is_dir()
}

/// Marks `root` as a lazy directory.
pub(crate) fn enable(root: &Path) -> Result<()> {
    let dir = records(root);
    fs::create_dir_all(&dir).at(&dir)
}

/// Paths that are always written in full: git's own data, which git and
/// the git pass read without going through a mount.
pub(crate) fn always_full(rel: &Path) -> bool {
    rel.components().any(|c| c.as_os_str() == ".git")
}

/// Record name for one version of a node's content.
fn key(node: NodeId, content: &Content) -> String {
    let encoded = postcard::to_stdvec(content).expect("serializable");
    let h = rustfuse_chunk::hash(&encoded);
    let hex: String = h.0[..12].iter().map(|b| format!("{b:02x}")).collect();
    format!("{:032x}-{hex}", node.0)
}

/// Writes a placeholder for `content` at `tmp` (to be renamed into place).
pub(crate) fn write_placeholder(root: &Path, tmp: &Path, node: NodeId, content: &Content) -> Result<()> {
    let Content::File { size, .. } = content else {
        return Err(Error::Corrupt("placeholder for a non-file".into()));
    };
    let k = key(node, content);
    let record = records(root).join(&k);
    if !record.exists() {
        let part = records(root).join(format!("{k}.tmp"));
        fs::write(&part, postcard::to_stdvec(content).expect("serializable")).at(&part)?;
        fs::rename(&part, &record).at(&record)?;
    }
    let f = File::create(tmp).at(tmp)?;
    f.set_len(*size).at(tmp)?;
    rustix::fs::fsetxattr(&f, XATTR, k.as_bytes(), rustix::fs::XattrFlags::empty())
        .map_err(std::io::Error::from)
        .at(tmp)?;
    Ok(())
}

/// The record a placeholder points at, if `file` is one.
fn marker(file: &File) -> Option<String> {
    let mut buf = [0u8; 128];
    let n = rustix::fs::fgetxattr(file, XATTR, &mut buf[..]).ok()?;
    String::from_utf8(buf[..n].to_vec()).ok()
}

/// The content a placeholder stands for, or `None` if `file` holds its
/// content.
pub(crate) fn placeholder_content(root: &Path, file: &File) -> Result<Option<Content>> {
    let Some(k) = marker(file) else {
        return Ok(None);
    };
    let record = records(root).join(&k);
    let raw = fs::read(&record).at(&record)?;
    postcard::from_bytes(&raw)
        .map(Some)
        .map_err(|e| Error::Corrupt(format!("{}: {e}", record.display())))
}

/// Deletes records no placeholder can still point at: the node is gone or
/// its content changed since. `tree` is what is on disk.
pub(crate) fn prune(root: &Path, tree: &Tree) -> Result<usize> {
    let dir = records(root);
    let rd = match fs::read_dir(&dir) {
        Ok(rd) => rd,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(e).at(&dir),
    };
    let mut n = 0;
    for ent in rd {
        let ent = ent.at(&dir)?;
        let name = ent.file_name().to_string_lossy().into_owned();
        let keep = name
            .split_once('-')
            .and_then(|(id, _)| u128::from_str_radix(id, 16).ok())
            .map(NodeId)
            .is_some_and(|id| tree.get(id).is_some_and(|e| key(id, &e.content) == name));
        if !keep {
            let p = ent.path();
            fs::remove_file(&p).at(&p)?;
            n += 1;
        }
    }
    Ok(n)
}

/// Fills in placeholders on demand. Cheap to share between threads.
pub struct Hydrator {
    root: PathBuf,
    store: Arc<dyn ObjectStore>,
    busy: Mutex<HashMap<PathBuf, Arc<tokio::sync::Mutex<()>>>>,
}

impl Hydrator {
    /// For the lazy directory at `root`, whose volume is in `store`.
    pub fn new(root: &Path, store: Arc<dyn ObjectStore>) -> Result<Hydrator> {
        Ok(Hydrator {
            root: root.canonicalize().at(root)?,
            store,
            busy: Mutex::new(HashMap::new()),
        })
    }

    /// Makes sure the file at `path` holds its content, fetching it if it
    /// is a placeholder. Returns whether anything was fetched.
    pub async fn hydrate(&self, path: &Path) -> Result<bool> {
        let open = |write: bool| {
            OpenOptions::new()
                .read(true)
                .write(write)
                .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits() as i32)
                .open(path)
        };
        // Most files are not placeholders: answer those without locking.
        match open(false) {
            Ok(f) if f.metadata().is_ok_and(|m| m.is_file()) && marker(&f).is_some() => {}
            _ => return Ok(false),
        }
        let lock = self
            .busy
            .lock()
            .unwrap()
            .entry(path.to_owned())
            .or_default()
            .clone();
        let _guard = lock.lock().await;
        let result = self.fill(path, open).await;
        let mut busy = self.busy.lock().unwrap();
        if Arc::strong_count(&lock) == 2 {
            busy.remove(path);
        }
        result
    }

    async fn fill(&self, path: &Path, open: impl Fn(bool) -> std::io::Result<File>) -> Result<bool> {
        let meta = fs::symlink_metadata(path).at(path)?;
        // A read-only file is made writable while it is filled in.
        let mode = meta.permissions().mode();
        let f = match open(true) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
                fs::set_permissions(path, fs::Permissions::from_mode(mode | 0o200)).at(path)?;
                open(true).at(path)?
            }
            Err(e) => return Err(e).at(path),
        };
        let restore_mode = || -> Result<()> {
            if f.metadata().at(path)?.permissions().mode() != mode {
                f.set_permissions(fs::Permissions::from_mode(mode)).at(path)?;
            }
            Ok(())
        };
        // Filled in by someone else meanwhile.
        let Some(k) = marker(&f) else {
            restore_mode()?;
            return Ok(false);
        };
        let Some(Content::File { chunks, size }) = placeholder_content(&self.root, &f)? else {
            restore_mode()?;
            return Err(Error::Corrupt(format!(
                "{}: placeholder without file content",
                path.display()
            )));
        };
        let mtime = f.metadata().at(path)?.modified().at(path)?;
        let t0 = std::time::Instant::now();
        let mut offset = 0u64;
        let mut start = 0;
        while start < chunks.len() {
            let mut end = start;
            let mut bytes = 0u64;
            while end < chunks.len() && (end == start || bytes + u64::from(chunks[end].len) <= WINDOW_BYTES) {
                bytes += u64::from(chunks[end].len);
                end += 1;
            }
            let window: Vec<ChunkRef> = chunks[start..end].to_vec();
            let data = rustfuse_chunk::fetch(self.store.clone(), &window).await?;
            let mut buf = Vec::with_capacity(bytes as usize);
            for r in &window {
                let d = data
                    .get(&r.hash)
                    .ok_or_else(|| Error::Corrupt(format!("chunk {:?} missing", r.hash)))?;
                buf.write_all(d).at(path)?;
            }
            f.write_all_at(&buf, offset).at(path)?;
            offset += buf.len() as u64;
            start = end;
        }
        f.set_len(size).at(path)?;
        f.set_modified(mtime).at(path)?;
        rustix::fs::fremovexattr(&f, XATTR)
            .map_err(std::io::Error::from)
            .at(path)?;
        restore_mode()?;
        let record = records(&self.root).join(&k);
        let _ = fs::remove_file(record);
        debug!(
            path = %path.display(),
            bytes = size,
            ms = t0.elapsed().as_millis() as u64,
            "fetched file content"
        );
        Ok(true)
    }
}
