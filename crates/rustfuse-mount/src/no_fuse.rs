//! Stand-in for builds without FUSE: macOS without the `macfuse` feature.

use std::path::Path;
use std::sync::Arc;

use crate::Backing;

/// Whether this build can serve FUSE mounts.
pub const FUSE: bool = false;

/// How to mount (see the FUSE build).
#[derive(Clone, Debug, Default)]
pub struct FuseOpts {
    pub allow_other: bool,
    pub fskit: bool,
    pub volname: Option<String>,
}

/// A FUSE mount; never made in this build.
pub struct FuseMount(());

impl FuseMount {
    pub fn unmount(self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Always fails: this build has no FUSE support.
pub fn mount_fuse(_: Arc<Backing>, _: &Path, _: &FuseOpts) -> std::io::Result<FuseMount> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "this build has no FUSE support: on macOS, install macFUSE and build with `--features macfuse`, \
         or use `rustfuse nfs`",
    ))
}
