//! The synced directory both frontends serve. Each operation goes straight
//! to the real file, so the sync engine sees mount writes exactly as it
//! sees any other local edit, and changes it applies from other machines
//! show up in the mount.
//!
//! The kernel (FUSE) and NFS clients name files by number. Numbers are
//! handed out on first lookup and remember the parent and name they were
//! found under; a rename through the mount moves the number along, and a
//! name that disappears underneath (another machine deleted or renamed it)
//! answers "no such file" until it is looked up again.

use std::collections::HashMap;
use std::ffi::OsStr;
use std::fs::{self, File, FileTimes, Metadata, OpenOptions};
use std::io::ErrorKind;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::{DirBuilderExt, FileExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

pub use rustix::io::Errno;

pub type Result<T, E = Errno> = std::result::Result<T, E>;

/// The root directory's number.
pub const ROOT: u64 = 1;

pub(crate) fn errno(e: std::io::Error) -> Errno {
    Errno::from_io_error(&e).unwrap_or(match e.kind() {
        ErrorKind::NotFound => Errno::NOENT,
        ErrorKind::PermissionDenied => Errno::ACCESS,
        ErrorKind::AlreadyExists => Errno::EXIST,
        ErrorKind::InvalidInput => Errno::INVAL,
        _ => Errno::IO,
    })
}

/// Attribute changes; `None` leaves a field as it is.
#[derive(Clone, Copy, Debug, Default)]
pub struct SetAttr {
    pub mode: Option<u32>,
    pub uid: Option<u32>,
    pub gid: Option<u32>,
    pub size: Option<u64>,
    pub atime: Option<SystemTime>,
    pub mtime: Option<SystemTime>,
}

/// Makes sure a file holds its content before it is opened (a lazy
/// directory fetches it on first use).
pub type Fetch = Arc<dyn Fn(&Path) -> std::io::Result<()> + Send + Sync>;

/// Something a kernel may hold in its cache that changed underneath the
/// mount (see [`Backing::changed`]).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Stale {
    /// The name `parent/name` may now point elsewhere, or nowhere.
    Entry { parent: u64, name: Vec<u8> },
    /// The file numbered `ino` at `parent/name` was removed.
    Gone { parent: u64, ino: u64, name: Vec<u8> },
    /// The attributes and content of `ino`.
    Inode(u64),
    /// Anything: changes were missed.
    All,
}

/// Where [`Stale`] notes go: the FUSE frontend, which tells the kernel.
pub type StaleSink = Box<dyn Fn(Stale) + Send + Sync>;

pub struct Backing {
    root: PathBuf,
    /// Names at the root that the mount does not show (the sync state).
    hidden: Vec<Vec<u8>>,
    nodes: Mutex<Nodes>,
    fetch: Option<Fetch>,
    /// Files held open through the mount, by real inode number.
    open: Mutex<HashMap<u64, usize>>,
    stale: Mutex<Option<StaleSink>>,
}

#[derive(Default)]
struct Nodes {
    /// Number -> (parent number, name).
    by_ino: HashMap<u64, (u64, Vec<u8>)>,
    by_name: HashMap<(u64, Vec<u8>), u64>,
    last: u64,
}

impl Nodes {
    fn ino(&mut self, parent: u64, name: &[u8]) -> u64 {
        if let Some(&i) = self.by_name.get(&(parent, name.to_vec())) {
            return i;
        }
        self.last = self.last.max(ROOT) + 1;
        self.by_name.insert((parent, name.to_vec()), self.last);
        self.by_ino.insert(self.last, (parent, name.to_vec()));
        self.last
    }

    /// Drops the number of whatever is at `parent/name`: anyone still
    /// holding it gets "stale" instead of whatever takes the name next.
    fn forget(&mut self, parent: u64, name: &[u8]) {
        if let Some(i) = self.by_name.remove(&(parent, name.to_vec())) {
            self.by_ino.remove(&i);
        }
    }

    fn moved(&mut self, from: (u64, &[u8]), to: (u64, &[u8])) {
        self.forget(to.0, to.1);
        if let Some(i) = self.by_name.remove(&(from.0, from.1.to_vec())) {
            self.by_name.insert((to.0, to.1.to_vec()), i);
            self.by_ino.insert(i, (to.0, to.1.to_vec()));
        }
    }
}

fn check_name(name: &[u8]) -> Result<()> {
    if name.is_empty() || name == b"." || name == b".." || name.contains(&b'/') || name.contains(&0) {
        return Err(Errno::INVAL);
    }
    if name.len() > 255 {
        return Err(Errno::NAMETOOLONG);
    }
    Ok(())
}

impl Backing {
    /// Serves `root`, hiding the given names in it.
    pub fn new(root: &Path, hidden: &[&str]) -> std::io::Result<Backing> {
        let root = root.canonicalize()?;
        if !fs::metadata(&root)?.is_dir() {
            return Err(std::io::Error::new(ErrorKind::NotADirectory, "not a directory"));
        }
        Ok(Backing {
            root,
            hidden: hidden.iter().map(|h| h.as_bytes().to_vec()).collect(),
            nodes: Mutex::new(Nodes::default()),
            fetch: None,
            open: Mutex::new(HashMap::new()),
            stale: Mutex::new(None),
        })
    }

    /// Sends what goes stale in a kernel's cache to `sink` (`None` stops).
    pub fn on_stale(&self, sink: Option<StaleSink>) {
        *self.stale.lock().unwrap() = sink;
    }

    /// Notes that something other than the mount changed these paths
    /// (relative to the root): the sync applying another machine's edits,
    /// its git pass, or any other program writing in the directory. With
    /// `None`, anything may have changed.
    ///
    /// Only names the kernel has been told about can be cached, so paths
    /// it has never looked up are skipped.
    pub fn changed(&self, paths: Option<&[PathBuf]>) {
        let sink = self.stale.lock().unwrap();
        let Some(sink) = sink.as_ref() else { return };
        let Some(paths) = paths else {
            return sink(Stale::All);
        };
        for rel in paths {
            for note in self.stale_notes(rel) {
                sink(note);
            }
        }
    }

    /// Everything a kernel may have cached: every name and number handed
    /// out.
    pub fn all_stale(&self) -> Vec<Stale> {
        let nodes = self.nodes.lock().unwrap();
        let mut notes = vec![Stale::Inode(ROOT)];
        for (&ino, (parent, name)) in &nodes.by_ino {
            notes.push(Stale::Entry {
                parent: *parent,
                name: name.clone(),
            });
            notes.push(Stale::Inode(ino));
        }
        notes
    }

    fn stale_notes(&self, rel: &Path) -> Vec<Stale> {
        let mut names = Vec::new();
        for c in rel.components() {
            match c {
                std::path::Component::Normal(n) => names.push(n.as_bytes()),
                _ => return Vec::new(),
            }
        }
        let Some((&name, dirs)) = names.split_last() else {
            return Vec::new();
        };
        let (parent, ino) = {
            let nodes = self.nodes.lock().unwrap();
            let mut parent = ROOT;
            for d in dirs {
                match nodes.by_name.get(&(parent, d.to_vec())) {
                    Some(&i) => parent = i,
                    None => return Vec::new(),
                }
            }
            (parent, nodes.by_name.get(&(parent, name.to_vec())).copied())
        };
        if self.hidden(parent, name) {
            return Vec::new();
        }
        let name = name.to_vec();
        // The directory's listing and times changed too.
        let mut notes = vec![Stale::Inode(parent)];
        match ino {
            Some(ino) if fs::symlink_metadata(self.root.join(rel)).is_err() => {
                notes.push(Stale::Gone { parent, ino, name });
            }
            Some(ino) => {
                notes.push(Stale::Entry { parent, name });
                notes.push(Stale::Inode(ino));
            }
            None => notes.push(Stale::Entry { parent, name }),
        }
        notes
    }

    /// Notes a file held open through the mount (see [`in_use`](Self::in_use)).
    pub fn opened(&self, f: &File) {
        if let Ok(m) = f.metadata() {
            *self.open.lock().unwrap().entry(m.ino()).or_default() += 1;
        }
    }

    pub fn closed(&self, f: &File) {
        if let Ok(m) = f.metadata() {
            let mut open = self.open.lock().unwrap();
            if let Some(n) = open.get_mut(&m.ino()) {
                *n -= 1;
                if *n == 0 {
                    open.remove(&m.ino());
                }
            }
        }
    }

    /// Whether the file with this real inode number is held open through
    /// the mount (NFS clients hold nothing open).
    pub fn in_use(&self, ino: u64) -> bool {
        self.open.lock().unwrap().contains_key(&ino)
    }

    /// Calls `fetch` on a file before opening or truncating it.
    pub fn with_fetch(mut self, fetch: Fetch) -> Backing {
        self.fetch = Some(fetch);
        self
    }

    fn ensure_content(&self, path: &Path) -> Result<()> {
        match &self.fetch {
            Some(f) => f(path).map_err(errno),
            None => Ok(()),
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn hidden(&self, parent: u64, name: &[u8]) -> bool {
        parent == ROOT && self.hidden.iter().any(|h| h == name)
    }

    /// The real path of a number.
    pub fn path(&self, ino: u64) -> Result<PathBuf> {
        let nodes = self.nodes.lock().unwrap();
        let mut names = Vec::new();
        let mut at = ino;
        while at != ROOT {
            let (parent, name) = nodes.by_ino.get(&at).ok_or(Errno::STALE)?;
            names.push(name.as_slice());
            at = *parent;
            if names.len() > 4096 {
                return Err(Errno::LOOP);
            }
        }
        let mut p = self.root.clone();
        for n in names.iter().rev() {
            p.push(OsStr::from_bytes(n));
        }
        Ok(p)
    }

    fn child(&self, parent: u64, name: &[u8]) -> Result<PathBuf> {
        check_name(name)?;
        Ok(self.path(parent)?.join(OsStr::from_bytes(name)))
    }

    /// A child path that may be created or changed through the mount.
    fn writable_child(&self, parent: u64, name: &[u8]) -> Result<PathBuf> {
        if self.hidden(parent, name) {
            return Err(Errno::PERM);
        }
        self.child(parent, name)
    }

    fn found(&self, parent: u64, name: &[u8], path: &Path) -> Result<(u64, Metadata)> {
        match fs::symlink_metadata(path) {
            Ok(m) => Ok((self.nodes.lock().unwrap().ino(parent, name), m)),
            Err(e) => {
                if e.kind() == ErrorKind::NotFound {
                    self.nodes.lock().unwrap().forget(parent, name);
                }
                Err(errno(e))
            }
        }
    }

    pub fn lookup(&self, parent: u64, name: &[u8]) -> Result<(u64, Metadata)> {
        match name {
            b"." => return Ok((parent, self.getattr(parent)?)),
            b".." => {
                let up = match parent {
                    ROOT => ROOT,
                    p => self.nodes.lock().unwrap().by_ino.get(&p).ok_or(Errno::STALE)?.0,
                };
                return Ok((up, self.getattr(up)?));
            }
            _ => {}
        }
        if self.hidden(parent, name) {
            return Err(Errno::NOENT);
        }
        let path = self.child(parent, name)?;
        self.found(parent, name, &path)
    }

    pub fn getattr(&self, ino: u64) -> Result<Metadata> {
        fs::symlink_metadata(self.path(ino)?).map_err(errno)
    }

    pub fn setattr(&self, ino: u64, s: SetAttr) -> Result<Metadata> {
        let path = self.path(ino)?;
        let m = fs::symlink_metadata(&path).map_err(errno)?;
        let link = m.file_type().is_symlink();
        if (s.uid.is_some_and(|u| u != m.uid())) || (s.gid.is_some_and(|g| g != m.gid())) {
            rustix::fs::chownat(
                rustix::fs::CWD,
                &path,
                s.uid.map(rustix::fs::Uid::from_raw),
                s.gid.map(rustix::fs::Gid::from_raw),
                rustix::fs::AtFlags::SYMLINK_NOFOLLOW,
            )?;
        }
        if let Some(mode) = s.mode
            && !link
        {
            fs::set_permissions(&path, fs::Permissions::from_mode(mode & 0o7777)).map_err(errno)?;
        }
        if let Some(size) = s.size {
            if m.is_dir() {
                return Err(Errno::ISDIR);
            }
            self.ensure_content(&path)?;
            OpenOptions::new()
                .write(true)
                .open(&path)
                .and_then(|f| f.set_len(size))
                .map_err(errno)?;
        }
        if (s.atime.is_some() || s.mtime.is_some()) && !link {
            let mut t = FileTimes::new();
            if let Some(a) = s.atime {
                t = t.set_accessed(a);
            }
            if let Some(m) = s.mtime {
                t = t.set_modified(m);
            }
            let f = File::open(&path)
                .or_else(|_| OpenOptions::new().write(true).open(&path))
                .map_err(errno)?;
            f.set_times(t).map_err(errno)?;
        }
        fs::symlink_metadata(&path).map_err(errno)
    }

    /// Opens a file for the frontends' reads and writes.
    pub fn open(&self, ino: u64, read: bool, write: bool, truncate: bool) -> Result<File> {
        let path = self.path(ino)?;
        self.ensure_content(&path)?;
        OpenOptions::new()
            .read(read || !write)
            .write(write)
            .truncate(truncate && write)
            .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits() as i32)
            .open(path)
            .map_err(errno)
    }

    pub fn read_at(file: &File, offset: u64, len: u32) -> Result<Vec<u8>> {
        let mut buf = vec![0u8; len as usize];
        let mut n = 0;
        while n < buf.len() {
            match file.read_at(&mut buf[n..], offset + n as u64) {
                Ok(0) => break,
                Ok(k) => n += k,
                Err(e) if e.kind() == ErrorKind::Interrupted => {}
                Err(e) => return Err(errno(e)),
            }
        }
        buf.truncate(n);
        Ok(buf)
    }

    pub fn write_at(file: &File, offset: u64, data: &[u8]) -> Result<()> {
        file.write_all_at(data, offset).map_err(errno)
    }

    /// Creates (or with `exclusive` unset, opens) a regular file.
    pub fn create(
        &self,
        parent: u64,
        name: &[u8],
        mode: u32,
        exclusive: bool,
    ) -> Result<(u64, Metadata, File)> {
        let path = self.writable_child(parent, name)?;
        let mut o = OpenOptions::new();
        o.read(true).write(true).mode(mode & 0o7777);
        if exclusive {
            o.create_new(true);
        } else {
            // It may already exist and need its content.
            if fs::symlink_metadata(&path).is_ok() {
                self.ensure_content(&path)?;
            }
            o.create(true);
        }
        let f = o.open(&path).map_err(errno)?;
        let (ino, m) = self.found(parent, name, &path)?;
        Ok((ino, m, f))
    }

    pub fn mkdir(&self, parent: u64, name: &[u8], mode: u32) -> Result<(u64, Metadata)> {
        let path = self.writable_child(parent, name)?;
        fs::DirBuilder::new()
            .mode(mode & 0o7777)
            .create(&path)
            .map_err(errno)?;
        self.found(parent, name, &path)
    }

    pub fn symlink(&self, parent: u64, name: &[u8], target: &[u8]) -> Result<(u64, Metadata)> {
        let path = self.writable_child(parent, name)?;
        std::os::unix::fs::symlink(OsStr::from_bytes(target), &path).map_err(errno)?;
        self.found(parent, name, &path)
    }

    pub fn readlink(&self, ino: u64) -> Result<Vec<u8>> {
        let t = fs::read_link(self.path(ino)?).map_err(errno)?;
        Ok(t.into_os_string().into_vec())
    }

    pub fn unlink(&self, parent: u64, name: &[u8]) -> Result<()> {
        let path = self.writable_child(parent, name)?;
        fs::remove_file(&path).map_err(errno)?;
        self.nodes.lock().unwrap().forget(parent, name);
        Ok(())
    }

    pub fn rmdir(&self, parent: u64, name: &[u8]) -> Result<()> {
        let path = self.writable_child(parent, name)?;
        fs::remove_dir(&path).map_err(errno)?;
        self.nodes.lock().unwrap().forget(parent, name);
        Ok(())
    }

    /// Renames; with `noreplace`, fails if the target exists.
    pub fn rename(
        &self,
        parent: u64,
        name: &[u8],
        to_parent: u64,
        to_name: &[u8],
        noreplace: bool,
    ) -> Result<()> {
        let from = self.writable_child(parent, name)?;
        let to = self.writable_child(to_parent, to_name)?;
        if noreplace && fs::symlink_metadata(&to).is_ok() {
            return Err(Errno::EXIST);
        }
        fs::rename(&from, &to).map_err(errno)?;
        self.nodes
            .lock()
            .unwrap()
            .moved((parent, name), (to_parent, to_name));
        Ok(())
    }

    /// A directory's entries (without `.` and `..`), sorted by name so
    /// listings page the same way every time.
    pub fn readdir(&self, ino: u64) -> Result<Vec<(Vec<u8>, u64, Metadata)>> {
        let path = self.path(ino)?;
        let mut out = Vec::new();
        for ent in fs::read_dir(&path).map_err(errno)? {
            let ent = ent.map_err(errno)?;
            let name = ent.file_name().into_vec();
            if self.hidden(ino, &name) {
                continue;
            }
            // Gone since the listing started: leave it out.
            let Ok(m) = ent.metadata() else { continue };
            out.push((name, m));
        }
        out.sort_by(|a, b| a.0.cmp(&b.0));
        let mut nodes = self.nodes.lock().unwrap();
        Ok(out
            .into_iter()
            .map(|(name, m)| {
                let i = nodes.ino(ino, &name);
                (name, i, m)
            })
            .collect())
    }

    pub fn statfs(&self) -> Result<rustix::fs::StatVfs> {
        rustix::fs::statvfs(&self.root)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers_follow_renames_and_hide_state() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join(".rustfuse")).unwrap();
        let b = Backing::new(dir.path(), &[".rustfuse"]).unwrap();
        assert_eq!(b.lookup(ROOT, b".rustfuse").unwrap_err(), Errno::NOENT);
        assert_eq!(b.mkdir(ROOT, b".rustfuse", 0o755).unwrap_err(), Errno::PERM);
        assert!(b.readdir(ROOT).unwrap().is_empty());

        let (d, _) = b.mkdir(ROOT, b"d", 0o755).unwrap();
        let (f, _, file) = b.create(d, b"a", 0o644, true).unwrap();
        assert_eq!(b.create(d, b"a", 0o644, true).unwrap_err(), Errno::EXIST);
        Backing::write_at(&file, 0, b"hello").unwrap();
        Backing::write_at(&file, 10, b"!").unwrap();
        assert_eq!(b.getattr(f).unwrap().len(), 11);
        let r = b.open(f, true, false, false).unwrap();
        assert_eq!(Backing::read_at(&r, 0, 5).unwrap(), b"hello");
        assert_eq!(Backing::read_at(&r, 10, 100).unwrap(), b"!");

        // The file keeps its number through a rename of its directory.
        b.rename(ROOT, b"d", ROOT, b"e", false).unwrap();
        assert_eq!(b.path(f).unwrap(), b.root().join("e/a"));
        assert_eq!(b.lookup(d, b"a").unwrap().0, f);
        // Replacing a file retires the old number.
        let (g, _, _) = b.create(d, b"b", 0o600, true).unwrap();
        b.rename(d, b"a", d, b"b", false).unwrap();
        assert_eq!(b.getattr(g).unwrap_err(), Errno::STALE);
        assert_eq!(b.lookup(d, b"b").unwrap().0, f);
        assert_eq!(b.rename(d, b"b", ROOT, b"e", true).unwrap_err(), Errno::EXIST);

        let m = b
            .setattr(
                f,
                SetAttr {
                    mode: Some(0o600),
                    size: Some(2),
                    mtime: Some(SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000)),
                    ..SetAttr::default()
                },
            )
            .unwrap();
        assert_eq!((m.mode() & 0o777, m.len(), m.mtime()), (0o600, 2, 1_000_000));

        let (l, _) = b.symlink(d, b"l", b"b").unwrap();
        assert_eq!(b.readlink(l).unwrap(), b"b");
        let names: Vec<_> = b.readdir(d).unwrap().into_iter().map(|e| e.0).collect();
        assert_eq!(names, vec![b"b".to_vec(), b"l".to_vec()]);

        // Something else removes the file: the name is gone, then stale.
        fs::remove_file(b.root().join("e/b")).unwrap();
        assert_eq!(b.lookup(d, b"b").unwrap_err(), Errno::NOENT);
        assert_eq!(b.getattr(f).unwrap_err(), Errno::STALE);
        b.unlink(d, b"l").unwrap();
        b.rmdir(ROOT, b"e").unwrap();
        assert_eq!(b.lookup(ROOT, b"..").unwrap().0, ROOT);
    }
}
