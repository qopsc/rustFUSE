//! FUSE frontend (Linux): passes each kernel request to the [`Backing`].

use std::collections::HashMap;
use std::ffi::OsStr;
use std::fs::{File, Metadata};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use fuser::{
    AccessFlags, BackgroundSession, Config, Errno, FileAttr, FileHandle, FileType, Filesystem, FopenFlags,
    Generation, INodeNo, LockOwner, MountOption, OpenAccMode, OpenFlags, RenameFlags, ReplyAttr, ReplyCreate,
    ReplyData, ReplyDirectory, ReplyEmpty, ReplyEntry, ReplyOpen, ReplyStatfs, ReplyWrite, ReplyXattr,
    Request, TimeOrNow, WriteFlags,
};

use crate::backing::{self, Backing, SetAttr};

/// How long the kernel may trust names and attributes before asking
/// again. Changes from other machines land in the directory underneath, so
/// this bounds how long the mount can show something stale.
const TTL: Duration = Duration::from_secs(1);

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

struct Fuse {
    b: Arc<Backing>,
    files: Mutex<HashMap<u64, Arc<File>>>,
    next_fh: AtomicU64,
}

impl Fuse {
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
            Ok((ino, m)) => reply.entry(&TTL, &attr(ino, &m), Generation(0)),
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
            Ok(m) => reply.attr(&TTL, &attr(ino.0, &m)),
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
        match self.b.setattr(ino.0, s) {
            Ok(m) => reply.attr(&TTL, &attr(ino.0, &m)),
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
        self.entry(reply, self.b.mkdir(parent.0, name.as_bytes(), mode & !umask));
    }

    fn unlink(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        Self::empty(reply, self.b.unlink(parent.0, name.as_bytes()));
    }

    fn rmdir(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        Self::empty(reply, self.b.rmdir(parent.0, name.as_bytes()));
    }

    fn symlink(&self, _req: &Request, parent: INodeNo, link_name: &OsStr, target: &Path, reply: ReplyEntry) {
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
        if flags.contains(RenameFlags::RENAME_EXCHANGE) {
            return reply.error(Errno::EINVAL);
        }
        Self::empty(
            reply,
            self.b.rename(
                parent.0,
                name.as_bytes(),
                newparent.0,
                newname.as_bytes(),
                flags.contains(RenameFlags::RENAME_NOREPLACE),
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
        match self.b.create(parent.0, name.as_bytes(), mode & !umask, exclusive) {
            Ok((ino, m, f)) => {
                if flags & rustix::fs::OFlags::TRUNC.bits() as i32 != 0
                    && let Err(e) = f.set_len(0)
                {
                    return reply.error(err(backing::errno(e)));
                }
                let fh = self.add_file(f);
                reply.created(&TTL, &attr(ino, &m), Generation(0), fh, FopenFlags::empty());
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
        _ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        data: &[u8],
        _write_flags: WriteFlags,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyWrite,
    ) {
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
}

impl FuseMount {
    pub fn unmount(mut self) -> std::io::Result<()> {
        match self.session.take() {
            Some(s) => s.umount_and_join(),
            None => Ok(()),
        }
    }
}

impl Drop for FuseMount {
    fn drop(&mut self) {
        if let Some(s) = self.session.take() {
            let _ = s.umount_and_join();
        }
    }
}

/// Mounts `backing` at `mountpoint`. `allow_other` lets other users in
/// (non-root users need `user_allow_other` in /etc/fuse.conf).
pub fn mount_fuse(backing: Arc<Backing>, mountpoint: &Path, allow_other: bool) -> std::io::Result<FuseMount> {
    let mut config = Config::default();
    config.mount_options.extend([
        MountOption::FSName("rustfuse".into()),
        MountOption::Subtype("rustfuse".into()),
        MountOption::DefaultPermissions,
    ]);
    if allow_other {
        config.acl = fuser::SessionACL::All;
    }
    config.n_threads = Some(8);
    let fs = Fuse {
        b: backing,
        files: Mutex::new(HashMap::new()),
        next_fh: AtomicU64::new(0),
    };
    let session = fuser::spawn_mount(fs, mountpoint, &config)?;
    Ok(FuseMount {
        session: Some(session),
    })
}
