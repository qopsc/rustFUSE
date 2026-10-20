//! Mount frontends (`docs/design.md` §2, Phase 6): serve a synced
//! directory read/write as a FUSE mount (Linux) or over NFSv3 (any OS with
//! an NFS client, including macOS without kernel extensions).
//!
//! Both work on the synced directory itself, so whatever the mount writes
//! is captured, sent and conflict-resolved by the sync engine exactly like
//! an edit made in the directory, offline included.

mod backing;
#[cfg(target_os = "linux")]
mod fuse;
mod nfs;

pub use backing::{Backing, Errno, ROOT, SetAttr};
#[cfg(target_os = "linux")]
pub use fuse::{FuseMount, mount_fuse};
pub use nfs::NfsServer;
