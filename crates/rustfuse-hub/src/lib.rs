//! The hub: gives each volume a single, totally ordered log of changesets
//! and pushes new entries to connected machines as they arrive
//! (`docs/design.md` §9).
//!
//! The hub never sees file content and never resolves conflicts. It stores
//! each changeset in the bucket (`log/{seq}`) before acknowledging it, so a
//! hub can restart, or be replaced, from the bucket alone. Machines that are
//! far behind can read snapshots and the log from the bucket directly.
//!
//! Wire protocol: TCP, each frame a big-endian `u32` length followed by a
//! zstd-compressed postcard message ([`Request`] / [`Response`]). Clients
//! authenticate with a shared token. A connection that opens with
//! [`Request::Store`] instead reads and writes the volume's objects through
//! the hub ([`HubStore`]), so remote machines need neither S3 credentials
//! nor a route to the bucket. With a certificate, the hub speaks TLS
//! and machines reach it as `tls://host[:port]` ([`tls`]); without one,
//! run it on a trusted network or behind a tunnel.

mod client;
pub mod codec;
pub mod keys;
mod log;
pub mod metrics;
mod server;
mod store;
pub mod tls;

use bytes::Bytes;
use serde::{Deserialize, Serialize};

pub use client::{Event, HubClient, volumes};
pub use log::{Volume, list_volumes};
pub use server::{Hub, HubMetrics};
pub use store::HubStore;

use rustfuse_types::Changeset;

/// Default TCP port of the hub.
pub const DEFAULT_PORT: u16 = 7420;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Store(#[from] rustfuse_store::Error),
    #[error("corrupt volume data: {0}")]
    Corrupt(String),
    #[error("a volume already exists at this location")]
    VolumeExists,
    #[error("no volume found at this location")]
    NoVolume,
    #[error("changeset {0} already exists; is a second hub writing to this volume?")]
    Conflict(u64),
    #[error("hub connection: {0}")]
    Io(#[from] std::io::Error),
    #[error("hub protocol error: {0}")]
    Protocol(String),
    #[error("hub refused: {0}")]
    Refused(String),
    #[error("hub TLS: {0}")]
    Tls(String),
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Client to hub.
#[derive(Debug, Serialize, Deserialize)]
pub enum Request {
    /// First message: join a volume and receive every changeset after
    /// `after`, then new ones as they are accepted.
    Hello {
        volume: String,
        token: String,
        after: u64,
    },
    /// Append a changeset (its `seq` is ignored).
    Push(Changeset),
    /// First message of a storage connection: read and write the objects
    /// of `volume` through the hub. Answered with [`Response::StoreReady`];
    /// after that both sides exchange [`ObjectRequest`] and
    /// [`ObjectResponse`] frames without zstd (the data is already
    /// compressed), any number in flight.
    Store { volume: String, token: String },
    /// First and only message: list the volumes this token opens.
    /// Answered with [`Response::Volumes`].
    Volumes { token: String },
}

/// One object-store call over a storage connection; keys are relative to
/// the volume.
#[derive(Debug, Serialize, Deserialize)]
pub struct ObjectRequest {
    pub id: u64,
    pub op: ObjectOp,
}

#[derive(Debug, Serialize, Deserialize)]
pub enum ObjectOp {
    Get {
        key: String,
    },
    GetRange {
        key: String,
        start: u64,
        end: u64,
    },
    Put {
        key: String,
        data: Bytes,
    },
    Head {
        key: String,
    },
    Delete {
        key: String,
    },
    List {
        prefix: String,
        start_after: Option<String>,
        limit: u32,
    },
    /// Answered with [`ObjectReply::Written`].
    PutIfAbsent {
        key: String,
        data: Bytes,
    },
}

/// The answer to the [`ObjectRequest`] with the same `id`.
#[derive(Debug, Serialize, Deserialize)]
pub struct ObjectResponse {
    pub id: u64,
    pub result: Result<ObjectReply, ObjectError>,
}

#[derive(Debug, Serialize, Deserialize)]
pub enum ObjectReply {
    Data(Bytes),
    Done,
    /// Whether a [`ObjectOp::PutIfAbsent`] wrote.
    Written(bool),
    /// Size and modification time of the object, if it exists.
    Head(Option<(u64, Option<u64>)>),
    List {
        /// Key, size and modification time (milliseconds since the epoch).
        objects: Vec<(String, u64, Option<u64>)>,
        next_start_after: Option<String>,
    },
}

#[derive(Debug, Serialize, Deserialize)]
pub enum ObjectError {
    NotFound(String),
    Other(String),
}

/// Hub to client.
#[derive(Debug, Serialize, Deserialize)]
pub enum Response {
    /// Joined; the log currently ends at `head`.
    Welcome { head: u64 },
    /// A changeset of this client was accepted at `seq`.
    Pushed { local: u64, seq: u64 },
    /// A changeset in the log, delivered in sequence order.
    Changeset(Changeset),
    /// Fatal for this connection.
    Error(String),
    /// A storage connection is open.
    StoreReady,
    /// The volumes in the bucket that the token opens, by name.
    Volumes(Vec<String>),
}

/// Volume names become bucket key segments, so keep them plain.
pub fn valid_volume_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 128
        && name != "."
        && name != ".."
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}
