//! FUSE frontend (Linux, and macOS through macFUSE): passes each kernel
//! request to the [`Backing`].

use std::collections::HashMap;
use std::collections::HashSet;
use std::ffi::OsStr;
use std::fs::{File, Metadata};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use fuser::{
    AccessFlags, BackgroundSession, Config, Errno, FileAttr, FileHandle, FileType, Filesystem, FopenFlags,
    Generation, INodeNo, LockOwner, MountOption, Notifier, OpenAccMode, OpenFlags, RenameFlags, ReplyAttr,
    ReplyCreate, ReplyData, ReplyDirectory, ReplyEmpty, ReplyEntry, ReplyOpen, ReplyStatfs, ReplyWrite,
    ReplyXattr, Request, TimeOrNow, WriteFlags,
};

use crate::backing::{self, Backing, SetAttr, Stale};

/// How long the kernel may trust names and attributes before asking again,
/// unless told how long (see [`FuseOpts::cache_for`]). Changes made
/// underneath that nobody reports to the mount show after at most this.
const TTL: Duration = Duration::from_secs(1);

// `rename` flags: Linux `renameat2`, macOS `renamex_np`.
#[cfg(target_os = "linux")]
const NOREPLACE: u32 = 1;
#[cfg(target_os = "macos")]
const NOREPLACE: u32 = 4;
const EXCHANGE: u32 = 2;

fn err(e: backing::Errno) -> Errno {
    Errno::from_i32(e.raw_os_error())
}

fn kind(m: &Metadata) -> FileType {
    let t = m.file_type();
    if t.is_dir() {
        FileType::Directory
    } else if t.is_symlink() {
        FileType::Symlink
    } else if t.is_fifo() {
        FileType::NamedPipe
    } else if t.is_socket() {
        FileType::Socket
    } else if t.is_char_device() {
        FileType::CharDevice
    } else if t.is_block_device() {
        FileType::BlockDevice
    } else {
        FileType::RegularFile
    }
}

fn time(secs: i64, nsecs: i64) -> SystemTime {
    if secs >= 0 {
        UNIX_EPOCH + Duration::new(secs as u64, nsecs as u32)
    } else {
        UNIX_EPOCH - Duration::from_secs(secs.unsigned_abs()) + Duration::from_nanos(nsecs as u64)
    }
}

fn attr(ino: u64, m: &Metadata) -> FileAttr {
    FileAttr {
        ino: INodeNo(ino),
        size: m.size(),
        blocks: m.blocks(),
        atime: time(m.atime(), m.atime_nsec()),
        mtime: time(m.mtime(), m.mtime_nsec()),
        ctime: time(m.ctime(), m.ctime_nsec()),
        crtime: time(m.mtime(), m.mtime_nsec()),
        kind: kind(m),
        perm: (m.mode() & 0o7777) as u16,
        nlink: m.nlink() as u32,
        uid: m.uid(),
        gid: m.gid(),
        rdev: m.rdev() as u32,
        blksize: m.blksize() as u32,
        flags: 0,
    }
}

fn when(t: TimeOrNow) -> SystemTime {
    match t {
        TimeOrNow::SpecificTime(t) => t,
        TimeOrNow::Now => SystemTime::now(),
    }
}

/// Numbers changed through the mount lately, and when. The kernel already
/// holds what was written to them, so a change seen underneath that soon
/// after is most likely the same write coming back.
type Touched = Arc<Mutex<HashMap<u64, Instant>>>;

/// How long a change through the mount counts as recent.
const RECENT: Duration = Duration::from_secs(2);

struct Fuse {
    b: Arc<Backing>,
    files: Mutex<HashMap<u64, Arc<File>>>,
    next_fh: AtomicU64,
    touched: Touched,
    ttl: Duration,
}

impl Fuse {
    fn touch(&self, ino: u64) {
        let now = Instant::now();
        let mut t = self.touched.lock().unwrap();
        if t.len() > 4096 {
            t.retain(|_, at| now.duration_since(*at) < RECENT);
        }
        t.insert(ino, now);
    }

    fn add_file(&self, f: File) -> FileHandle {
        let fh = self.next_fh.fetch_add(1, Ordering::Relaxed) + 1;
        self.b.opened(&f);
        self.files.lock().unwrap().insert(fh, Arc::new(f));
        FileHandle(fh)
    }

    fn file(&self, fh: FileHandle) -> Result<Arc<File>, Errno> {
        self.files.lock().unwrap().get(&fh.0).cloned().ok_or(Errno::EBADF)
    }

    fn entry(&self, reply: ReplyEntry, r: backing::Result<(u64, Metadata)>) {
        match r {
            Ok((ino, m)) => reply.entry(&self.ttl, &attr(ino, &m), Generation(0)),
            Err(e) => reply.error(err(e)),
        }
    }

    fn empty(reply: ReplyEmpty, r: backing::Result<()>) {
        match r {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(err(e)),
        }
    }
}

impl Filesystem for Fuse {
    fn lookup(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        self.entry(reply, self.b.lookup(parent.0, name.as_bytes()));
    }

    fn getattr(&self, _req: &Request, ino: INodeNo, _fh: Option<FileHandle>, reply: ReplyAttr) {
        match self.b.getattr(ino.0) {
            Ok(m) => reply.attr(&self.ttl, &attr(ino.0, &m)),
            Err(e) => reply.error(err(e)),
        }
    }

    fn setattr(
        &self,
        _req: &Request,
        ino: INodeNo,
        mode: Option<u32>,
        uid: Option<u32>,
        gid: Option<u32>,
        size: Option<u64>,
        atime: Option<TimeOrNow>,
        mtime: Option<TimeOrNow>,
        _ctime: Option<SystemTime>,
        fh: Option<FileHandle>,
        _crtime: Option<SystemTime>,
        _chgtime: Option<SystemTime>,
        _bkuptime: Option<SystemTime>,
        _flags: Option<fuser::BsdFileFlags>,
        reply: ReplyAttr,
    ) {
        // Truncating an open file goes through its handle: the file may
        // be open for writing but no longer writable by mode.
        if let (Some(size), Some(fh)) = (size, fh)
            && let Ok(f) = self.file(fh)
            && let Err(e) = f.set_len(size)
        {
            return reply.error(err(backing::errno(e)));
        }
        let s = SetAttr {
            mode,
            uid,
            gid,
            size: if fh.is_some() { None } else { size },
            atime: atime.map(when),
            mtime: mtime.map(when),
        };
        self.touch(ino.0);
        match self.b.setattr(ino.0, s) {
            Ok(m) => reply.attr(&self.ttl, &attr(ino.0, &m)),
            Err(e) => reply.error(err(e)),
        }
    }

    fn readlink(&self, _req: &Request, ino: INodeNo, reply: ReplyData) {
        match self.b.readlink(ino.0) {
            Ok(t) => reply.data(&t),
            Err(e) => reply.error(err(e)),
        }
    }

    fn mknod(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        mode: u32,
        umask: u32,
        _rdev: u32,
        reply: ReplyEntry,
    ) {
        // Only regular files sync; no devices, pipes or sockets.
        if mode & 0o170000 != 0o100000 {
            return reply.error(Errno::EPERM);
        }
        let r = self
            .b
            .create(parent.0, name.as_bytes(), mode & !umask, true)
            .map(|(i, m, _)| (i, m));
        self.entry(reply, r);
    }

    fn mkdir(&self, _req: &Request, parent: INodeNo, name: &OsStr, mode: u32, umask: u32, reply: ReplyEntry) {
        self.touch(parent.0);
        self.entry(reply, self.b.mkdir(parent.0, name.as_bytes(), mode & !umask));
    }

    fn unlink(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        self.touch(parent.0);
        Self::empty(reply, self.b.unlink(parent.0, name.as_bytes()));
    }

    fn rmdir(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        self.touch(parent.0);
        Self::empty(reply, self.b.rmdir(parent.0, name.as_bytes()));
    }

    fn symlink(&self, _req: &Request, parent: INodeNo, link_name: &OsStr, target: &Path, reply: ReplyEntry) {
        self.touch(parent.0);
        self.entry(
            reply,
            self.b
                .symlink(parent.0, link_name.as_bytes(), target.as_os_str().as_bytes()),
        );
    }

    fn rename(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        newparent: INodeNo,
        newname: &OsStr,
        flags: RenameFlags,
        reply: ReplyEmpty,
    ) {
        if flags.bits() & EXCHANGE != 0 {
            return reply.error(Errno::EINVAL);
        }
        self.touch(parent.0);
        self.touch(newparent.0);
        Self::empty(
            reply,
            self.b.rename(
                parent.0,
                name.as_bytes(),
                newparent.0,
                newname.as_bytes(),
                flags.bits() & NOREPLACE != 0,
            ),
        );
    }

    fn link(&self, _req: &Request, _ino: INodeNo, _newparent: INodeNo, _newname: &OsStr, reply: ReplyEntry) {
        // A synced volume has no hard links; git falls back to renaming.
        reply.error(Errno::EPERM);
    }

    fn getxattr(&self, _req: &Request, _ino: INodeNo, _name: &OsStr, _size: u32, reply: ReplyXattr) {
        // Extended attributes do not sync, and the sync's own markers
        // stay out of sight.
        reply.error(Errno::ENOTSUP);
    }

    fn listxattr(&self, _req: &Request, _ino: INodeNo, size: u32, reply: ReplyXattr) {
        if size == 0 {
            reply.size(0);
        } else {
            reply.data(&[]);
        }
    }

    fn open(&self, _req: &Request, ino: INodeNo, flags: OpenFlags, reply: ReplyOpen) {
        let (read, write) = match flags.acc_mode() {
            OpenAccMode::O_RDONLY => (true, false),
            OpenAccMode::O_WRONLY => (false, true),
            OpenAccMode::O_RDWR => (true, true),
        };
        let truncate = flags.0 & rustix::fs::OFlags::TRUNC.bits() as i32 != 0;
        if write {
            // Also fetches a lazy file's content: no news to the kernel.
            self.touch(ino.0);
        }
        match self.b.open(ino.0, read, write, truncate) {
            Ok(f) => reply.opened(self.add_file(f), FopenFlags::empty()),
            Err(e) => reply.error(err(e)),
        }
    }

    fn create(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        mode: u32,
        umask: u32,
        flags: i32,
        reply: ReplyCreate,
    ) {
        let exclusive = flags & rustix::fs::OFlags::EXCL.bits() as i32 != 0;
        self.touch(parent.0);
        match self.b.create(parent.0, name.as_bytes(), mode & !umask, exclusive) {
            Ok((ino, m, f)) => {
                if flags & rustix::fs::OFlags::TRUNC.bits() as i32 != 0
                    && let Err(e) = f.set_len(0)
                {
                    return reply.error(err(backing::errno(e)));
                }
                self.touch(ino);
                let fh = self.add_file(f);
                reply.created(&self.ttl, &attr(ino, &m), Generation(0), fh, FopenFlags::empty());
            }
            Err(e) => reply.error(err(e)),
        }
    }

    fn read(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        size: u32,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyData,
    ) {
        match self
            .file(fh)
            .and_then(|f| Backing::read_at(&f, offset, size).map_err(err))
        {
            Ok(d) => reply.data(&d),
            Err(e) => reply.error(e),
        }
    }

    fn write(
        &self,
        _req: &Request,
        ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        data: &[u8],
        _write_flags: WriteFlags,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyWrite,
    ) {
        self.touch(ino.0);
        match self
            .file(fh)
            .and_then(|f| Backing::write_at(&f, offset, data).map_err(err))
        {
            Ok(()) => reply.written(data.len() as u32),
            Err(e) => reply.error(e),
        }
    }

    fn flush(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _fh: FileHandle,
        _lock_owner: LockOwner,
        reply: ReplyEmpty,
    ) {
        reply.ok();
    }

    fn release(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        if let Some(f) = self.files.lock().unwrap().remove(&fh.0) {
            self.b.closed(&f);
        }
        reply.ok();
    }

    fn fsync(&self, _req: &Request, _ino: INodeNo, fh: FileHandle, datasync: bool, reply: ReplyEmpty) {
        let r = self.file(fh).and_then(|f| {
            if datasync { f.sync_data() } else { f.sync_all() }.map_err(|e| err(backing::errno(e)))
        });
        match r {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(e),
        }
    }

    fn opendir(&self, _req: &Request, _ino: INodeNo, _flags: OpenFlags, reply: ReplyOpen) {
        reply.opened(FileHandle(0), FopenFlags::empty());
    }

    fn readdir(&self, _req: &Request, ino: INodeNo, _fh: FileHandle, offset: u64, mut reply: ReplyDirectory) {
        let entries = match self.b.readdir(ino.0) {
            Ok(e) => e,
            Err(e) => return reply.error(err(e)),
        };
        let up = self.b.lookup(ino.0, b"..").map_or(backing::ROOT, |(i, _)| i);
        let dots = [
            (b".".to_vec(), ino.0, FileType::Directory),
            (b"..".to_vec(), up, FileType::Directory),
        ];
        let all = dots
            .into_iter()
            .chain(entries.into_iter().map(|(n, i, m)| (n, i, kind(&m))));
        for (i, (name, ino, kind)) in all.enumerate().skip(offset as usize) {
            if reply.add(INodeNo(ino), i as u64 + 1, kind, OsStr::from_bytes(&name)) {
                break;
            }
        }
        reply.ok();
    }

    fn fsyncdir(&self, _req: &Request, _ino: INodeNo, _fh: FileHandle, _datasync: bool, reply: ReplyEmpty) {
        reply.ok();
    }

    fn statfs(&self, _req: &Request, _ino: INodeNo, reply: ReplyStatfs) {
        match self.b.statfs() {
            Ok(s) => reply.statfs(
                s.f_blocks,
                s.f_bfree,
                s.f_bavail,
                s.f_files,
                s.f_ffree,
                s.f_bsize as u32,
                s.f_namemax as u32,
                s.f_frsize as u32,
            ),
            Err(e) => reply.error(err(e)),
        }
    }

    fn access(&self, _req: &Request, _ino: INodeNo, _mask: AccessFlags, reply: ReplyEmpty) {
        // The kernel checks permissions itself (`default_permissions`).
        reply.ok();
    }
}

/// A FUSE mount; unmounted when dropped.
pub struct FuseMount {
    session: Option<BackgroundSession>,
    backing: Arc<Backing>,
}

impl FuseMount {
    pub fn unmount(mut self) -> std::io::Result<()> {
        self.backing.on_stale(None);
        match self.session.take() {
            Some(s) => s.umount_and_join(),
            None => Ok(()),
        }
    }
}

impl Drop for FuseMount {
    fn drop(&mut self) {
        self.backing.on_stale(None);
        if let Some(s) = self.session.take() {
            let _ = s.umount_and_join();
        }
    }
}

/// Tells the kernel what changed underneath the mount, so it drops what it
/// cached at once instead of trusting it until it expires. Runs on its
/// own thread: an invalidation can wait for a request the mount is still
/// answering.
///
/// Linux does not pass these on to inotify watchers of the mount (not even
/// removals, as of 6.18), so a watcher should watch the synced directory.
fn tell_kernel(rx: mpsc::Receiver<Stale>, kernel: Notifier, backing: Arc<Backing>, touched: Touched) {
    while let Ok(first) = rx.recv() {
        // One checkout or pull changes many paths at once.
        let mut batch = HashSet::from([first]);
        while let Ok(n) = rx.recv_timeout(Duration::from_millis(5)) {
            batch.insert(n);
            if batch.len() >= 100_000 {
                break;
            }
        }
        let notes = if batch.contains(&Stale::All) {
            backing.all_stale()
        } else {
            batch.into_iter().collect()
        };
        let recent = |ino: u64| {
            touched
                .lock()
                .unwrap()
                .get(&ino)
                .is_some_and(|at| at.elapsed() < RECENT)
        };
        for note in notes {
            let res = match note {
                Stale::Entry { parent, name } => {
                    kernel.inval_entry(INodeNo(parent), OsStr::from_bytes(&name))
                }
                Stale::Gone { parent, ino, name } => kernel
                    .delete(INodeNo(parent), INodeNo(ino), OsStr::from_bytes(&name))
                    .or_else(|_| kernel.inval_entry(INodeNo(parent), OsStr::from_bytes(&name))),
                // Just written through the mount: the kernel's copy of
                // the content is current, only the attributes may lag.
                Stale::Inode(ino) if recent(ino) => kernel.inval_inode(INodeNo(ino), -1, 0),
                Stale::Inode(ino) => kernel.inval_inode(INodeNo(ino), 0, 0),
                Stale::All => Ok(()),
            };
            if let Err(e) = res {
                tracing::debug!("cannot tell the kernel about a change: {e}");
            }
        }
    }
}

/// Whether this build can serve FUSE mounts.
pub const FUSE: bool = true;

/// How to mount.
#[derive(Clone, Debug, Default)]
pub struct FuseOpts {
    /// Let other users in (on Linux, non-root users need
    /// `user_allow_other` in /etc/fuse.conf).
    pub allow_other: bool,
    /// macOS: use macFUSE's FSKit backend (no kernel extension; macOS 15.4
    /// or later, mountpoint under /Volumes) instead of its kernel one.
    pub fskit: bool,
    /// macOS: the volume name Finder shows.
    pub volname: Option<String>,
    /// How long the kernel may trust names and attributes (1 second when
    /// unset). Longer makes stat-heavy work like `git status` faster; it
    /// is safe when every change underneath is reported through
    /// [`Backing::changed`], as the sync does for whatever it sees.
    pub cache_for: Option<Duration>,
}

/// Mounts `backing` at `mountpoint`.
pub fn mount_fuse(backing: Arc<Backing>, mountpoint: &Path, opts: &FuseOpts) -> std::io::Result<FuseMount> {
    let mut config = Config::default();
    config.mount_options.extend([
        MountOption::FSName("rustfuse".into()),
        MountOption::Subtype("rustfuse".into()),
        MountOption::DefaultPermissions,
    ]);
    if cfg!(target_os = "macos") {
        // No `._*` AppleDouble files for Finder metadata: they would sync
        // and show up in `git status`.
        config
            .mount_options
            .push(MountOption::CUSTOM("noappledouble".into()));
        if let Some(name) = &opts.volname {
            config
                .mount_options
                .push(MountOption::CUSTOM(format!("volname={name}")));
        }
        if opts.fskit {
            config
                .mount_options
                .push(MountOption::CUSTOM("backend=fskit".into()));
        }
    } else if opts.fskit {
        return Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "the FSKit backend is macOS-only",
        ));
    }
    if opts.allow_other {
        config.acl = fuser::SessionACL::All;
    }
    config.n_threads = Some(8);
    let touched = Touched::default();
    let fs = Fuse {
        b: backing.clone(),
        files: Mutex::new(HashMap::new()),
        next_fh: AtomicU64::new(0),
        touched: touched.clone(),
        ttl: opts.cache_for.unwrap_or(TTL),
    };
    let session = fuser::spawn_mount(fs, mountpoint, &config)?;
    let (tx, rx) = mpsc::channel();
    let (kernel, b) = (session.notifier(), backing.clone());
    std::thread::Builder::new()
        .name("rustfuse-inval".into())
        .spawn(move || tell_kernel(rx, kernel, b, touched))?;
    backing.on_stale(Some(Box::new(move |note| {
        let _ = tx.send(note);
    })));
    Ok(FuseMount {
        session: Some(session),
        backing,
    })
}
