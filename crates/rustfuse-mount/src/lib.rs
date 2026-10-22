//! Mount frontends (`docs/design.md` §2, Phase 6): serve a synced
//! directory read/write as a FUSE mount (Linux, or macOS with macFUSE and
//! the `macfuse` feature) or over NFSv3 (any OS with an NFS client,
//! including macOS with nothing installed).
//!
//! Both work on the synced directory itself, so whatever the mount writes
//! is captured, sent and conflict-resolved by the sync engine exactly like
//! an edit made in the directory, offline included.

mod backing;
#[cfg(fuse_mount)]
mod fuse;
#[cfg(not(fuse_mount))]
#[path = "no_fuse.rs"]
mod fuse;
mod nfs;

pub use backing::{Backing, Errno, ROOT, SetAttr, Stale, StaleSink};
pub use fuse::{FUSE, FuseMount, FuseOpts, mount_fuse};
pub use nfs::NfsServer;
