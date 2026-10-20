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
//! Filling in a file keeps its record and notes the file's `stat` next to
//! it (`<record>.filled`). [`Hydrator::evict`] turns such files back into
//! placeholders to keep fetched content within a budget, least recently
//! opened first, but only while the `stat` still matches: the file then
//! holds exactly the recorded content, which is the current version (a
//! newer one would have been renamed into place) and is in the bucket.
//!
//! A lazy directory must be used through `rustfuse mount` or `rustfuse
//! nfs`: programs reading it directly see placeholders as zeros.

use std::collections::{BTreeSet, HashMap};
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rustfuse_store::ObjectStore;
use rustfuse_types::{ChunkRef, Content, NodeId};
use serde::{Deserialize, Serialize};
use tracing::debug;

use crate::tree::Tree;
use crate::{Error, IoContext, Result, STATE_DIR};

/// Marks a placeholder; the value is its record's name.
pub const XATTR: &str = "user.rustfuse.lazy";
/// Chunk data fetched at a time while filling in a file.
const WINDOW_BYTES: u64 = 64 << 20;
/// Suffix of the note kept for a filled-in file.
const FILLED: &str = ".filled";
/// Files opened this recently are never turned back into placeholders.
const RECENT: Duration = Duration::from_secs(60);

/// A filled-in file as it was right after filling in.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Filled {
    /// Relative to the directory, as bytes.
    path: Vec<u8>,
    ino: u64,
    size: u64,
    mtime: i64,
    ctime: i64,
}

impl Filled {
    fn of(path: Vec<u8>, m: &fs::Metadata) -> Filled {
        Filled {
            path,
            ino: m.ino(),
            size: m.size(),
            mtime: m.mtime() * 1_000_000_000 + m.mtime_nsec(),
            ctime: m.ctime() * 1_000_000_000 + m.ctime_nsec(),
        }
    }
}

/// What [`Hydrator::evict`] did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Evicted {
    /// Bytes of fetched content held before.
    pub held: u64,
    pub files: usize,
    pub bytes: u64,
}

/// Files and folders the user asked to keep downloaded: eviction skips
/// them. Kept in `.rustfuse/pinned.json`, relative paths as bytes.
#[derive(Debug, Default)]
pub struct Pins {
    file: PathBuf,
    set: Mutex<BTreeSet<PathBuf>>,
}

impl Pins {
    /// The pins of the directory at `root`.
    pub fn load(root: &Path) -> Pins {
        let file = root.join(STATE_DIR).join("pinned.json");
        let set = fs::read(&file)
            .ok()
            .and_then(|raw| serde_json::from_slice::<Vec<Vec<u8>>>(&raw).ok())
            .unwrap_or_default()
            .into_iter()
            .map(|b| PathBuf::from(std::ffi::OsStr::from_bytes(&b)))
            .collect();
        Pins {
            file,
            set: Mutex::new(set),
        }
    }

    /// Whether `rel` (relative to the root) is kept downloaded, by its own
    /// pin or a folder's above it.
    pub fn covers(&self, rel: &Path) -> bool {
        self.set.lock().unwrap().iter().any(|p| rel.starts_with(p))
    }

    /// Pins or unpins `rel`. Pins below it go either way: they are covered
    /// by the new pin, or released with it.
    pub fn set(&self, rel: &Path, keep: bool) -> Result<()> {
        let mut set = self.set.lock().unwrap();
        set.retain(|p| !p.starts_with(rel));
        if keep {
            set.insert(rel.to_owned());
        }
        let list: Vec<&[u8]> = set.iter().map(|p| p.as_os_str().as_bytes()).collect();
        let tmp = self.file.with_extension("json.tmp");
        fs::write(&tmp, serde_json::to_vec(&list).expect("serializable")).at(&tmp)?;
        fs::rename(&tmp, &self.file).at(&self.file)
    }
}

/// Space a synced directory takes on this machine.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    /// Files (and symlinks) in the directory.
    pub files: u64,
    /// Their combined size, downloaded or not.
    pub total: u64,
    /// Bytes of content on this machine.
    pub local: u64,
    /// Of `local`, git's own data (always kept).
    pub git: u64,
    /// Of `local`, files kept downloaded by a pin.
    pub pinned: u64,
    /// Of `local`, fetched content that can be dropped again.
    pub cached: u64,
}

/// Measures the directory at `root` (blocking; walks every file).
pub fn usage(root: &Path, pins: Option<&Pins>) -> Result<Usage> {
    let mut u = Usage::default();
    let mut stack = vec![PathBuf::new()];
    while let Some(rel) = stack.pop() {
        let dir = root.join(&rel);
        let rd = match fs::read_dir(&dir) {
            Ok(rd) => rd,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e).at(&dir),
        };
        for ent in rd.flatten() {
            let child = rel.join(ent.file_name());
            if rel.as_os_str().is_empty() && ent.file_name() == STATE_DIR {
                continue;
            }
            let Ok(m) = ent.metadata() else { continue };
            if m.is_dir() {
                stack.push(child);
                continue;
            }
            u.files += 1;
            u.total += m.len();
            if m.is_file() && is_placeholder(&root.join(&child)) {
                continue;
            }
            u.local += m.len();
            if always_full(&child) {
                u.git += m.len();
            } else if pins.is_some_and(|p| p.covers(&child)) {
                u.pinned += m.len();
            }
        }
    }
    // Fetched files that still hold what was fetched.
    if is_lazy(root) {
        let dir = records(root);
        if let Ok(rd) = fs::read_dir(&dir) {
            for ent in rd.flatten() {
                if !ent.file_name().as_bytes().ends_with(FILLED.as_bytes()) {
                    continue;
                }
                let Some(f) = fs::read(ent.path())
                    .ok()
                    .and_then(|raw| serde_json::from_slice::<Filled>(&raw).ok())
                else {
                    continue;
                };
                let rel = Path::new(std::ffi::OsStr::from_bytes(&f.path));
                let current =
                    fs::symlink_metadata(root.join(rel)).is_ok_and(|m| Filled::of(f.path.clone(), &m) == f);
                if current && !pins.is_some_and(|p| p.covers(rel)) {
                    u.cached += f.size;
                }
            }
        }
    }
    Ok(u)
}

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

/// Whether the file at `path` is a placeholder (content not fetched yet).
pub(crate) fn is_placeholder(path: &Path) -> bool {
    let mut buf = [0u8; 128];
    rustix::fs::lgetxattr(path, XATTR, &mut buf[..]).is_ok()
}

/// The content a placeholder stands for, or `None` if `file` holds its
/// content.
pub(crate) fn placeholder_content(root: &Path, file: &File) -> Result<Option<Content>> {
    let Some(k) = marker(file) else {
        return Ok(None);
    };
    let record = records(root).join(&k);
    let raw = fs::read(&record).at(&record)?;
    let content: Content =
        postcard::from_bytes(&raw).map_err(|e| Error::Corrupt(format!("{}: {e}", record.display())))?;
    // Written to without the mount (which fills in first): the marker is
    // stale and the file holds what was written.
    let len = file.metadata().at(&record)?.len();
    match content {
        Content::File { size, .. } if size == len => Ok(Some(content)),
        _ => Ok(None),
    }
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
        let k = name.strip_suffix(FILLED).unwrap_or(&name);
        let keep = k
            .split_once('-')
            .and_then(|(id, _)| u128::from_str_radix(id, 16).ok())
            .map(NodeId)
            .is_some_and(|id| tree.get(id).is_some_and(|e| key(id, &e.content) == k));
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
    /// When each file was last asked for, by inode.
    used: Mutex<HashMap<u64, Instant>>,
    pins: Arc<Pins>,
}

impl Hydrator {
    /// For the lazy directory at `root`, whose volume is in `store`.
    pub fn new(root: &Path, store: Arc<dyn ObjectStore>) -> Result<Hydrator> {
        let root = root.canonicalize().at(root)?;
        Ok(Hydrator {
            pins: Arc::new(Pins::load(&root)),
            root,
            store,
            busy: Mutex::new(HashMap::new()),
            used: Mutex::new(HashMap::new()),
        })
    }

    /// What the user asked to keep downloaded; shared with the sync loop.
    pub fn pins(&self) -> Arc<Pins> {
        self.pins.clone()
    }

    /// Fills in every placeholder at or below `rel` (relative to the
    /// root), as for a folder just pinned. Stops once `rel` is no longer
    /// pinned (freed meanwhile). Returns how many were fetched.
    pub async fn hydrate_all(&self, rel: &Path) -> Result<usize> {
        let mut n = 0;
        let mut stack = vec![self.root.join(rel)];
        while let Some(path) = stack.pop() {
            if !self.pins.covers(rel) {
                break;
            }
            let Ok(m) = fs::symlink_metadata(&path) else {
                continue;
            };
            if m.is_dir() {
                for ent in fs::read_dir(&path).at(&path)?.flatten() {
                    if ent.file_name() != STATE_DIR {
                        stack.push(ent.path());
                    }
                }
            } else if m.is_file() && self.hydrate(&path).await? {
                n += 1;
            }
        }
        Ok(n)
    }

    fn lock_for(&self, path: &Path) -> Arc<tokio::sync::Mutex<()>> {
        self.busy
            .lock()
            .unwrap()
            .entry(path.to_owned())
            .or_default()
            .clone()
    }

    fn unlock(&self, path: &Path, lock: &Arc<tokio::sync::Mutex<()>>) {
        let mut busy = self.busy.lock().unwrap();
        if Arc::strong_count(lock) == 2 {
            busy.remove(path);
        }
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
            Ok(f) if f.metadata().is_ok_and(|m| m.is_file()) => {
                if let Ok(m) = f.metadata() {
                    self.used.lock().unwrap().insert(m.ino(), Instant::now());
                }
                if marker(&f).is_none() {
                    return Ok(false);
                }
            }
            _ => return Ok(false),
        }
        let lock = self.lock_for(path);
        let guard = lock.lock().await;
        let result = self.fill(path, open).await;
        drop(guard);
        self.unlock(path, &lock);
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
        let m = f.metadata().at(path)?;
        self.used.lock().unwrap().insert(m.ino(), Instant::now());
        let rel = path.strip_prefix(&self.root).unwrap_or(path);
        let note = records(&self.root).join(format!("{k}{FILLED}"));
        let json =
            serde_json::to_vec(&Filled::of(rel.as_os_str().as_bytes().to_vec(), &m)).expect("serializable");
        fs::write(&note, json).at(&note)?;
        debug!(
            path = %path.display(),
            bytes = size,
            ms = t0.elapsed().as_millis() as u64,
            "fetched file content"
        );
        Ok(true)
    }
}

impl Hydrator {
    /// Turns filled-in files back into placeholders, least recently opened
    /// first, until at most `budget` bytes of fetched content remain
    /// (down to 90% of it, so this does not run on every new fetch).
    /// Skips files opened in the last minute and those `in_use` names (by
    /// inode). Blocking.
    pub fn evict(&self, budget: u64, in_use: impl Fn(u64) -> bool) -> Result<Evicted> {
        let held: Vec<_> = self
            .held()?
            .into_iter()
            .filter(|(_, f, _)| !self.pins.covers(Path::new(std::ffi::OsStr::from_bytes(&f.path))))
            .collect();
        let mut out = Evicted {
            held: held.iter().map(|(_, f, _)| f.size).sum(),
            ..Evicted::default()
        };
        if out.held <= budget {
            return Ok(out);
        }
        let target = budget / 10 * 9;
        let used = self.used.lock().unwrap().clone();
        // Never opened since this process started: oldest first, by when
        // they were filled in.
        let mut held = held;
        held.sort_by_key(|(k, f, _)| match used.get(&f.ino) {
            Some(t) => (1, Some(*t), k.clone()),
            None => (0, None, k.clone()),
        });
        let mut remaining = out.held;
        for (k, f, path) in held {
            if remaining <= target {
                break;
            }
            if used.get(&f.ino).is_some_and(|t| t.elapsed() < RECENT) || in_use(f.ino) {
                continue;
            }
            if self.empty_locked(&k, &f, &path)? {
                remaining -= f.size;
                out.files += 1;
                out.bytes += f.size;
            }
        }
        Ok(out)
    }

    /// Turns every filled-in file at or below `rel` back into a
    /// placeholder ("free up space"), except those `in_use` names (by
    /// inode). Blocking.
    pub fn free(&self, rel: &Path, in_use: impl Fn(u64) -> bool) -> Result<Evicted> {
        let mut out = Evicted::default();
        for (k, f, path) in self.held()? {
            if !Path::new(std::ffi::OsStr::from_bytes(&f.path)).starts_with(rel) {
                continue;
            }
            out.held += f.size;
            if !in_use(f.ino) && self.empty_locked(&k, &f, &path)? {
                out.files += 1;
                out.bytes += f.size;
            }
        }
        Ok(out)
    }

    /// Filled-in files that still hold exactly what was fetched, with their
    /// record names; notes of the others are dropped.
    fn held(&self) -> Result<Vec<(String, Filled, PathBuf)>> {
        let dir = records(&self.root);
        let rd = match fs::read_dir(&dir) {
            Ok(rd) => rd,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e).at(&dir),
        };
        let mut held = Vec::new();
        for ent in rd {
            let note = ent.at(&dir)?.path();
            let Some(k) = note
                .file_name()
                .and_then(|n| n.to_str())
                .and_then(|n| n.strip_suffix(FILLED))
                .map(str::to_owned)
            else {
                continue;
            };
            let filled: Option<Filled> = fs::read(&note)
                .ok()
                .and_then(|raw| serde_json::from_slice(&raw).ok());
            let path = filled
                .as_ref()
                .map(|f| self.root.join(std::ffi::OsStr::from_bytes(&f.path)));
            let current = match (&filled, &path) {
                (Some(f), Some(p)) => {
                    fs::symlink_metadata(p).is_ok_and(|m| Filled::of(f.path.clone(), &m) == *f)
                }
                _ => false,
            };
            if current {
                held.push((k, filled.unwrap(), path.unwrap()));
            } else {
                // Changed, replaced or gone since: not ours to empty.
                let _ = fs::remove_file(&note);
            }
        }
        Ok(held)
    }

    fn empty_locked(&self, k: &str, f: &Filled, path: &Path) -> Result<bool> {
        let lock = self.lock_for(path);
        let guard = lock.blocking_lock();
        let done = self.empty(k, f, path);
        drop(guard);
        self.unlock(path, &lock);
        done
    }

    /// Turns one filled-in file back into a placeholder, if it still holds
    /// what was fetched.
    fn empty(&self, k: &str, f: &Filled, path: &Path) -> Result<bool> {
        let file = match OpenOptions::new()
            .write(true)
            .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits() as i32)
            .open(path)
        {
            Ok(file) => file,
            // Read-only or gone: leave it.
            Err(_) => return Ok(false),
        };
        let m = file.metadata().at(path)?;
        if Filled::of(f.path.clone(), &m) != *f || !records(&self.root).join(k).exists() {
            return Ok(false);
        }
        let mtime = m.modified().at(path)?;
        // Marked first: from here on, capture sees the recorded content.
        rustix::fs::fsetxattr(&file, XATTR, k.as_bytes(), rustix::fs::XattrFlags::empty())
            .map_err(std::io::Error::from)
            .at(path)?;
        file.set_len(0).at(path)?;
        file.set_len(f.size).at(path)?;
        file.set_modified(mtime).at(path)?;
        let note = records(&self.root).join(format!("{k}{FILLED}"));
        let _ = fs::remove_file(note);
        debug!(path = %path.display(), bytes = f.size, "dropped fetched content");
        Ok(true)
    }
}
