//! Object storage abstraction for rustFUSE.
//!
//! The filesystem only needs a handful of operations from the object store
//! (`docs/design.md` §8.4). Keeping them behind [`ObjectStore`] lets the VFS
//! run against an in-memory store in tests and against any S3-compatible
//! service (rustFS, MinIO, Ceph RGW, AWS) in production.

use std::ops::Range;
use std::time::SystemTime;

use async_trait::async_trait;
use bytes::Bytes;

pub mod conformance;
mod memory;
mod prefix;
mod s3;

pub use memory::MemoryStore;
pub use prefix::PrefixStore;
pub use s3::{S3Config, S3Store};

pub type Result<T, E = Error> = std::result::Result<T, E>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("object not found: {0}")]
    NotFound(String),
    #[error("object store error: {0}")]
    Backend(#[source] Box<dyn std::error::Error + Send + Sync>),
}

impl Error {
    pub(crate) fn backend(e: impl std::error::Error + Send + Sync + 'static) -> Self {
        Error::Backend(Box::new(e))
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ObjectMeta {
    pub key: String,
    pub size: u64,
    /// When the object was last written, if the store reports it.
    pub modified: Option<SystemTime>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ListPage {
    /// Objects in lexicographic key order.
    pub objects: Vec<ObjectMeta>,
    /// Pass as `start_after` to fetch the next page; `None` when done.
    pub next_start_after: Option<String>,
}

/// The operations rustFUSE needs from an object store.
///
/// Keys are relative to the store's configured prefix. Data blocks are
/// written once and never modified, so no conditional or partial writes are
/// required.
#[async_trait]
pub trait ObjectStore: Send + Sync + 'static {
    /// Reads the whole object.
    async fn get(&self, key: &str) -> Result<Bytes>;

    /// Reads bytes `range` of the object, clipped to its length: a range
    /// starting at or past the end yields an empty buffer. An empty `range`
    /// returns empty without contacting the store.
    async fn get_range(&self, key: &str, range: Range<u64>) -> Result<Bytes>;

    /// Writes the object, replacing any existing one.
    async fn put(&self, key: &str, data: Bytes) -> Result<()>;

    /// Returns the object's metadata, or `None` if it does not exist.
    async fn head(&self, key: &str) -> Result<Option<ObjectMeta>>;

    /// Deletes the object. Deleting a missing object succeeds.
    async fn delete(&self, key: &str) -> Result<()>;

    /// Lists up to `limit` objects whose keys start with `prefix` and sort
    /// after `start_after`.
    async fn list(&self, prefix: &str, start_after: Option<&str>, limit: usize) -> Result<ListPage>;
}
