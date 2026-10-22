//! The sync engine: keeps a plain local directory and a volume in the
//! bucket in step (`docs/design.md` §4–§6).
//!
//! Any number of machines can change the same volume, online or offline;
//! the hub orders their changes and every machine resolves conflicts the
//! same way (`docs/design.md` §7).

mod apply;
pub mod fsmonitor;
pub mod gc;
mod git;
mod ignore;
mod index;
pub mod lazy;
pub mod metrics;
pub mod provider;
pub mod remote;
mod replica;
mod scan;
mod tree;

use std::path::PathBuf;

pub use git::original_name;
pub use ignore::Ignore;
pub use index::Config;
pub use replica::{CaptureStats, DiskWatch, HubSpec, Replica, SyncStats};
pub use rustfuse_hub::Volume;
pub use scan::Change;
pub use tree::{Tree, conflict_name};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Store(#[from] rustfuse_store::Error),
    #[error(transparent)]
    Chunk(#[from] rustfuse_chunk::Error),
    #[error(transparent)]
    Hub(rustfuse_hub::Error),
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
    #[error("timed out waiting for the hub")]
    Timeout,
    #[error("the hub closed the connection")]
    Disconnected,
    #[error("local changes in the way: {0:?}")]
    LocalChanges(Vec<PathBuf>),
    #[error("{0} is already a rustfuse directory")]
    AlreadyInitialized(PathBuf),
    #[error("{0} is not a rustfuse directory (run `rustfuse init` or `rustfuse attach`)")]
    NotInitialized(PathBuf),
    #[error("{0} must be empty to attach a volume")]
    NotEmpty(PathBuf),
}

impl From<rustfuse_hub::Error> for Error {
    fn from(e: rustfuse_hub::Error) -> Self {
        match e {
            rustfuse_hub::Error::Store(e) => Error::Store(e),
            rustfuse_hub::Error::Corrupt(m) => Error::Corrupt(m),
            rustfuse_hub::Error::VolumeExists => Error::VolumeExists,
            rustfuse_hub::Error::NoVolume => Error::NoVolume,
            other => Error::Hub(other),
        }
    }
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
