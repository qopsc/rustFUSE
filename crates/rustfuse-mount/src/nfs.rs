//! NFSv3 frontend: serves the [`Backing`] to the operating system's own
//! NFS client, which needs no kernel extension (the way to mount on macOS)
//! and can mount from other machines.
//!
//! There is no authentication: anyone who can reach the port can read and
//! write the volume. Listen on loopback unless the network is trusted.

use std::fs::Metadata;
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use nfsserve::nfs::{
    fattr3, fileid3, filename3, ftype3, nfspath3, nfsstat3, nfstime3, sattr3, set_atime, set_gid3, set_mode3,
    set_mtime, set_size3, set_uid3, specdata3,
};
use nfsserve::tcp::{NFSTcp, NFSTcpListener};
use nfsserve::vfs::{DirEntry, NFSFileSystem, ReadDirResult, VFSCapabilities};

use crate::backing::{self, Backing, Errno, ROOT, SetAttr};

fn stat(e: Errno) -> nfsstat3 {
    match e {
        Errno::PERM => nfsstat3::NFS3ERR_PERM,
        Errno::NOENT => nfsstat3::NFS3ERR_NOENT,
        Errno::NXIO => nfsstat3::NFS3ERR_NXIO,
        Errno::ACCESS => nfsstat3::NFS3ERR_ACCES,
        Errno::EXIST => nfsstat3::NFS3ERR_EXIST,
        Errno::XDEV => nfsstat3::NFS3ERR_XDEV,
        Errno::NOTDIR => nfsstat3::NFS3ERR_NOTDIR,
        Errno::ISDIR => nfsstat3::NFS3ERR_ISDIR,
        Errno::INVAL => nfsstat3::NFS3ERR_INVAL,
        Errno::FBIG => nfsstat3::NFS3ERR_FBIG,
        Errno::NOSPC => nfsstat3::NFS3ERR_NOSPC,
        Errno::ROFS => nfsstat3::NFS3ERR_ROFS,
        Errno::MLINK => nfsstat3::NFS3ERR_MLINK,
        Errno::NAMETOOLONG => nfsstat3::NFS3ERR_NAMETOOLONG,
        Errno::NOTEMPTY => nfsstat3::NFS3ERR_NOTEMPTY,
        Errno::DQUOT => nfsstat3::NFS3ERR_DQUOT,
        Errno::STALE => nfsstat3::NFS3ERR_STALE,
        Errno::NOTSUP => nfsstat3::NFS3ERR_NOTSUPP,
        _ => nfsstat3::NFS3ERR_IO,
    }
}

fn nfstime(secs: i64, nsecs: i64) -> nfstime3 {
    nfstime3 {
        seconds: secs.clamp(0, u32::MAX as i64) as u32,
        nseconds: nsecs as u32,
    }
}

fn time(t: nfstime3) -> SystemTime {
    UNIX_EPOCH + Duration::new(u64::from(t.seconds), t.nseconds)
}

fn fattr(ino: u64, m: &Metadata) -> fattr3 {
    let t = m.file_type();
    let ftype = if t.is_dir() {
        ftype3::NF3DIR
    } else if t.is_symlink() {
        ftype3::NF3LNK
    } else if t.is_fifo() {
        ftype3::NF3FIFO
    } else if t.is_socket() {
        ftype3::NF3SOCK
    } else if t.is_char_device() {
        ftype3::NF3CHR
    } else if t.is_block_device() {
        ftype3::NF3BLK
    } else {
        ftype3::NF3REG
    };
    fattr3 {
        ftype,
        mode: m.mode() & 0o7777,
        nlink: m.nlink() as u32,
        uid: m.uid(),
        gid: m.gid(),
        size: m.size(),
        used: m.blocks() * 512,
        rdev: specdata3::default(),
        fsid: 0,
        fileid: ino,
        atime: nfstime(m.atime(), m.atime_nsec()),
        mtime: nfstime(m.mtime(), m.mtime_nsec()),
        ctime: nfstime(m.ctime(), m.ctime_nsec()),
    }
}

fn set_attr(s: &sattr3) -> SetAttr {
    let now = SystemTime::now();
    SetAttr {
        mode: match s.mode {
            set_mode3::mode(m) => Some(m),
            set_mode3::Void => None,
        },
        uid: match s.uid {
            set_uid3::uid(u) => Some(u),
            set_uid3::Void => None,
        },
        gid: match s.gid {
            set_gid3::gid(g) => Some(g),
            set_gid3::Void => None,
        },
        size: match s.size {
            set_size3::size(n) => Some(n),
            set_size3::Void => None,
        },
        atime: match s.atime {
            set_atime::DONT_CHANGE => None,
            set_atime::SET_TO_SERVER_TIME => Some(now),
            set_atime::SET_TO_CLIENT_TIME(t) => Some(time(t)),
        },
        mtime: match s.mtime {
            set_mtime::DONT_CHANGE => None,
            set_mtime::SET_TO_SERVER_TIME => Some(now),
            set_mtime::SET_TO_CLIENT_TIME(t) => Some(time(t)),
        },
    }
}

struct Nfs {
    b: Arc<Backing>,
}

#[async_trait]
impl NFSFileSystem for Nfs {
    fn capabilities(&self) -> VFSCapabilities {
        VFSCapabilities::ReadWrite
    }

    fn root_dir(&self) -> fileid3 {
        ROOT
    }

    async fn lookup(&self, dirid: fileid3, filename: &filename3) -> Result<fileid3, nfsstat3> {
        self.b.lookup(dirid, &filename.0).map(|(i, _)| i).map_err(stat)
    }

    async fn getattr(&self, id: fileid3) -> Result<fattr3, nfsstat3> {
        self.b.getattr(id).map(|m| fattr(id, &m)).map_err(stat)
    }

    async fn setattr(&self, id: fileid3, setattr: sattr3) -> Result<fattr3, nfsstat3> {
        self.b
            .setattr(id, set_attr(&setattr))
            .map(|m| fattr(id, &m))
            .map_err(stat)
    }

    async fn read(&self, id: fileid3, offset: u64, count: u32) -> Result<(Vec<u8>, bool), nfsstat3> {
        let f = self.b.open(id, true, false, false).map_err(stat)?;
        let data = Backing::read_at(&f, offset, count).map_err(stat)?;
        let size = f.metadata().map_err(|e| stat(backing::errno(e)))?.len();
        let eof = offset + data.len() as u64 >= size;
        Ok((data, eof))
    }

    async fn write(&self, id: fileid3, offset: u64, data: &[u8]) -> Result<fattr3, nfsstat3> {
        let f = self.b.open(id, false, true, false).map_err(stat)?;
        Backing::write_at(&f, offset, data).map_err(stat)?;
        let m = f.metadata().map_err(|e| stat(backing::errno(e)))?;
        Ok(fattr(id, &m))
    }

    async fn create(
        &self,
        dirid: fileid3,
        filename: &filename3,
        attr: sattr3,
    ) -> Result<(fileid3, fattr3), nfsstat3> {
        let s = set_attr(&attr);
        let (id, _, _) = self
            .b
            .create(dirid, &filename.0, s.mode.unwrap_or(0o644), false)
            .map_err(stat)?;
        let m = self.b.setattr(id, SetAttr { mode: None, ..s }).map_err(stat)?;
        Ok((id, fattr(id, &m)))
    }

    async fn create_exclusive(&self, dirid: fileid3, filename: &filename3) -> Result<fileid3, nfsstat3> {
        self.b
            .create(dirid, &filename.0, 0o644, true)
            .map(|(i, _, _)| i)
            .map_err(stat)
    }

    async fn mkdir(&self, dirid: fileid3, dirname: &filename3) -> Result<(fileid3, fattr3), nfsstat3> {
        self.b
            .mkdir(dirid, &dirname.0, 0o755)
            .map(|(i, m)| (i, fattr(i, &m)))
            .map_err(stat)
    }

    async fn remove(&self, dirid: fileid3, filename: &filename3) -> Result<(), nfsstat3> {
        // NFS uses one call for files and directories.
        let (_, m) = self.b.lookup(dirid, &filename.0).map_err(stat)?;
        if m.is_dir() {
            self.b.rmdir(dirid, &filename.0)
        } else {
            self.b.unlink(dirid, &filename.0)
        }
        .map_err(stat)
    }

    async fn rename(
        &self,
        from_dirid: fileid3,
        from_filename: &filename3,
        to_dirid: fileid3,
        to_filename: &filename3,
    ) -> Result<(), nfsstat3> {
        self.b
            .rename(from_dirid, &from_filename.0, to_dirid, &to_filename.0, false)
            .map_err(stat)
    }

    async fn readdir(
        &self,
        dirid: fileid3,
        start_after: fileid3,
        max_entries: usize,
    ) -> Result<ReadDirResult, nfsstat3> {
        let all = self.b.readdir(dirid).map_err(stat)?;
        let start = match start_after {
            0 => 0,
            after => match all.iter().position(|(_, i, _)| *i == after) {
                Some(p) => p + 1,
                // The entry the client stopped at is gone; start over
                // rather than skip what is left.
                None => return Err(nfsstat3::NFS3ERR_BAD_COOKIE),
            },
        };
        let entries: Vec<DirEntry> = all[start..]
            .iter()
            .take(max_entries)
            .map(|(name, i, m)| DirEntry {
                fileid: *i,
                name: name.clone().into(),
                attr: fattr(*i, m),
            })
            .collect();
        let end = start + entries.len() >= all.len();
        Ok(ReadDirResult { entries, end })
    }

    async fn symlink(
        &self,
        dirid: fileid3,
        linkname: &filename3,
        symlink: &nfspath3,
        _attr: &sattr3,
    ) -> Result<(fileid3, fattr3), nfsstat3> {
        self.b
            .symlink(dirid, &linkname.0, &symlink.0)
            .map(|(i, m)| (i, fattr(i, &m)))
            .map_err(stat)
    }

    async fn readlink(&self, id: fileid3) -> Result<nfspath3, nfsstat3> {
        self.b.readlink(id).map(Into::into).map_err(stat)
    }
}

/// An NFS server for a [`Backing`], bound and ready to serve.
pub struct NfsServer {
    listener: NFSTcpListener<Nfs>,
}

impl NfsServer {
    /// Binds `addr` (`ip:port`; port 0 picks a free one).
    pub async fn bind(backing: Arc<Backing>, addr: &str) -> std::io::Result<NfsServer> {
        let listener = NFSTcpListener::bind(addr, Nfs { b: backing }).await?;
        Ok(NfsServer { listener })
    }

    pub fn port(&self) -> u16 {
        self.listener.get_listen_port()
    }

    pub fn ip(&self) -> std::net::IpAddr {
        self.listener.get_listen_ip()
    }

    /// Serves until an error stops the listener.
    pub async fn serve(self) -> std::io::Result<()> {
        self.listener.handle_forever().await
    }
}
