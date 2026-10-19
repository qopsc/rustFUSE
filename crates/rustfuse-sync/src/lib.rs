//! The sync engine: keeps a plain local directory and a volume in the
//! bucket in step (`docs/design.md` §4–§6).
//!
//! Phase 1 supports one writer per volume (`push`) and any number of
//! followers (`pull`). Concurrent writers and conflict resolution arrive
//! with the hub in Phase 2.

mod apply;
mod ignore;
mod index;
mod remote;
mod replica;
mod scan;
mod tree;

use std::path::PathBuf;

pub use ignore::Ignore;
pub use index::{Config, Role};
pub use remote::Volume;
pub use replica::{PushStats, Replica};
pub use scan::Change;
pub use tree::Tree;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Store(#[from] rustfuse_store::Error),
    #[error(transparent)]
    Chunk(#[from] rustfuse_chunk::Error),
    #[error("{path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("local index error: {0}")]
    Index(String),
    #[error("corrupt volume data: {0}")]
    Corrupt(String),
    #[error("a volume already exists at this location")]
    VolumeExists,
    #[error("no volume found at this location")]
    NoVolume,
    #[error("changeset {0} was already written by another machine")]
    Conflict(u64),
    #[error("this directory follows the volume; only the machine that ran `init` can push (until Phase 2)")]
    NotWriter,
    #[error("{0} is already a rustfuse directory")]
    AlreadyInitialized(PathBuf),
    #[error("{0} is not a rustfuse directory (run `rustfuse init` or `rustfuse attach`)")]
    NotInitialized(PathBuf),
    #[error("{0} must be empty to attach a volume")]
    NotEmpty(PathBuf),
    #[error("another machine is pushing to this volume (changeset {0}); Phase 1 supports one writer")]
    ForeignWriter(u64),
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

pub(crate) trait IoContext<T> {
    fn at(self, path: impl Into<PathBuf>) -> Result<T>;
}

impl<T> IoContext<T> for std::io::Result<T> {
    fn at(self, path: impl Into<PathBuf>) -> Result<T> {
        self.map_err(|source| Error::Io {
            path: path.into(),
            source,
        })
    }
}

/// Name of the per-directory state folder.
pub const STATE_DIR: &str = ".rustfuse";
