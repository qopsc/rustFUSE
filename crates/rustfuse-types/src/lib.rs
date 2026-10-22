//! Core types shared by every rustFUSE crate: identifiers, the operation
//! model, and content references (`docs/design.md` §5).

use std::fmt;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// Version of the on-bucket encoding of changesets and snapshots.
pub const FORMAT_VERSION: u32 = 2;

/// Files up to this size are stored inside the operation, not in packs.
/// Kept small: inline content lives in every tree, index and snapshot, so
/// a large threshold costs memory on trees with many small files.
pub const INLINE_MAX: u64 = 256;

/// Identifies one machine's replica of a volume.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ReplicaId(pub u64);

/// Stable identity of a file or directory, independent of its path:
/// the creating replica in the high 64 bits, a per-replica counter in the
/// low 64 bits.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct NodeId(pub u128);

impl NodeId {
    /// The volume's root directory.
    pub const ROOT: NodeId = NodeId(0);

    pub fn new(replica: ReplicaId, counter: u64) -> Self {
        NodeId((u128::from(replica.0) << 64) | u128::from(counter))
    }
}

impl fmt::Debug for NodeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "NodeId({:016x}:{:x})", (self.0 >> 64) as u64, self.0 as u64)
    }
}

/// BLAKE3 hash of a chunk's uncompressed bytes.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ChunkHash(pub [u8; 32]);

impl fmt::Debug for ChunkHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ChunkHash({})", &hex::encode(self.0)[..16])
    }
}

/// Identity of an immutable pack object in the bucket.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct PackId(pub u128);

impl PackId {
    /// Object key of this pack, relative to the volume prefix.
    pub fn object_key(&self) -> String {
        // Leading byte as a directory spreads keys across store partitions.
        let hex = format!("{:032x}", self.0);
        format!("pack/{}/{}", &hex[..2], hex)
    }
}

impl fmt::Debug for PackId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "PackId({:032x})", self.0)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Codec {
    Raw,
    Zstd,
    /// Encrypted with the volume's key (`rustfuse-crypt`), after
    /// compression for `SealedZstd`.
    SealedRaw,
    SealedZstd,
}

/// Where one chunk of file content is stored.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChunkRef {
    pub hash: ChunkHash,
    /// Uncompressed length.
    pub len: u32,
    pub pack: PackId,
    pub offset: u64,
    pub stored_len: u32,
    pub codec: Codec,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Content {
    Dir,
    /// Small file stored in the operation itself.
    Inline(Vec<u8>),
    File {
        size: u64,
        chunks: Vec<ChunkRef>,
    },
    Symlink(Vec<u8>),
}

impl Content {
    pub fn is_dir(&self) -> bool {
        matches!(self, Content::Dir)
    }

    /// Whether two contents hold the same bytes (ignoring where they live).
    pub fn same_data(&self, other: &Content) -> bool {
        match (self, other) {
            (Content::File { size: a, chunks: ca }, Content::File { size: b, chunks: cb }) => {
                a == b && ca.len() == cb.len() && ca.iter().zip(cb).all(|(x, y)| x.hash == y.hash)
            }
            _ => self == other,
        }
    }
}

/// Nanoseconds since the Unix epoch.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Timestamp(pub i64);

impl Timestamp {
    pub fn now() -> Self {
        SystemTime::now().into()
    }
}

impl From<SystemTime> for Timestamp {
    fn from(t: SystemTime) -> Self {
        match t.duration_since(UNIX_EPOCH) {
            Ok(d) => Timestamp(d.as_nanos() as i64),
            Err(e) => Timestamp(-(e.duration().as_nanos() as i64)),
        }
    }
}

impl From<Timestamp> for SystemTime {
    fn from(t: Timestamp) -> Self {
        if t.0 >= 0 {
            UNIX_EPOCH + Duration::from_nanos(t.0 as u64)
        } else {
            UNIX_EPOCH - Duration::from_nanos(t.0.unsigned_abs())
        }
    }
}

/// State of one node: where it lives and what it contains.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    pub parent: NodeId,
    /// File name bytes (not necessarily UTF-8 on Linux).
    pub name: Vec<u8>,
    /// Permission bits (`0o777`).
    pub mode: u32,
    pub mtime: Timestamp,
    pub content: Content,
}

/// Hybrid logical clock reading: nanoseconds since the Unix epoch, nudged
/// forward so that every reading is later than any it has seen.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Hlc(pub u64);

/// Issues [`Hlc`] readings for one replica.
#[derive(Clone, Debug, Default)]
pub struct Clock {
    last: u64,
}

impl Clock {
    pub fn new(last: Hlc) -> Self {
        Clock { last: last.0 }
    }

    pub fn now(&mut self) -> Hlc {
        let wall = Timestamp::now().0.max(0) as u64;
        self.last = wall.max(self.last + 1);
        Hlc(self.last)
    }

    /// Moves the clock past a reading seen from another replica.
    pub fn observe(&mut self, t: Hlc) {
        self.last = self.last.max(t.0);
    }

    pub fn last(&self) -> Hlc {
        Hlc(self.last)
    }
}

/// Identifies one change: when and where it was made. Ordered by time, so
/// "later" in conflict rules is the greater version (`docs/design.md` §7).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Version {
    pub time: Hlc,
    pub replica: ReplicaId,
}

/// Last change to each independently mergeable part of a node, so a rename
/// on one machine and an edit on another both apply.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Versions {
    /// Parent and name.
    pub loc: Version,
    /// Content and mtime.
    pub data: Version,
    /// Permission bits.
    pub meta: Version,
}

impl Versions {
    pub fn all(v: Version) -> Self {
        Versions {
            loc: v,
            data: v,
            meta: v,
        }
    }
}

/// Which parts of a node an upsert changes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Fields(pub u8);

impl Fields {
    pub const LOC: Fields = Fields(1);
    pub const DATA: Fields = Fields(2);
    pub const META: Fields = Fields(4);
    pub const ALL: Fields = Fields(7);

    pub fn contains(self, f: Fields) -> bool {
        self.0 & f.0 == f.0
    }

    pub fn is_empty(self) -> bool {
        self.0 == 0
    }
}

impl std::ops::BitOr for Fields {
    type Output = Fields;
    fn bitor(self, rhs: Fields) -> Fields {
        Fields(self.0 | rhs.0)
    }
}

impl std::ops::BitOrAssign for Fields {
    fn bitor_assign(&mut self, rhs: Fields) {
        self.0 |= rhs.0;
    }
}

/// A node as the volume knows it: its entry plus the version of each part.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Node {
    pub entry: Entry,
    pub versions: Versions,
}

/// A change to one node. Ops say which versions their author started from,
/// so a receiver can tell a clean change from a concurrent one.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Op {
    /// Create, modify, move or rename a node. `entry` is the full new
    /// state; only the parts in `changed` count as changed.
    Upsert {
        node: NodeId,
        entry: Entry,
        /// Versions the author saw; `None` for a new node.
        base: Option<Versions>,
        changed: Fields,
    },
    Remove {
        node: NodeId,
        base: Versions,
    },
    /// All of an encrypted volume's ops in one changeset, sealed with its
    /// key (`rustfuse-crypt`); the only op such a changeset holds. Opened
    /// before anything reads them, so it never reaches a tree.
    Sealed(Vec<u8>),
}

impl Op {
    /// The node the op changes (none for a sealed op).
    pub fn node(&self) -> NodeId {
        match self {
            Op::Upsert { node, .. } | Op::Remove { node, .. } => *node,
            Op::Sealed(_) => NodeId::ROOT,
        }
    }
}

/// One batch of operations from one replica. Every op in it carries the
/// changeset's version. The hub assigns `seq` and stores it as `log/{seq}`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Changeset {
    pub format: u32,
    /// Position in the volume's log (0 until the hub accepts it).
    pub seq: u64,
    pub version: Version,
    /// The author's own counter, to match accepted changesets to its outbox.
    pub local: u64,
    pub ops: Vec<Op>,
}

/// Full state of a volume after applying changeset `seq`, stored as
/// `snap/{seq}`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Snapshot {
    pub format: u32,
    pub seq: u64,
    pub nodes: Vec<(NodeId, Node)>,
    /// Removed nodes, kept so late concurrent changes resolve the same way
    /// on every machine.
    pub removed: Vec<(NodeId, Node)>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn node_ids_embed_replica() {
        let id = NodeId::new(ReplicaId(0xabc), 7);
        assert_eq!((id.0 >> 64) as u64, 0xabc);
        assert_eq!(id.0 as u64, 7);
        assert_ne!(id, NodeId::ROOT);
    }

    #[test]
    fn pack_keys_are_sharded() {
        let key = PackId(0x1234 << 112).object_key();
        assert_eq!(key, "pack/12/12340000000000000000000000000000");
    }

    #[test]
    fn timestamps_round_trip() {
        for t in [
            UNIX_EPOCH + Duration::new(1_700_000_000, 5),
            UNIX_EPOCH - Duration::new(3, 7),
        ] {
            assert_eq!(SystemTime::from(Timestamp::from(t)), t);
        }
    }

    #[test]
    fn clock_is_monotonic_and_observes() {
        let mut c = Clock::default();
        let a = c.now();
        let b = c.now();
        assert!(b > a);
        c.observe(Hlc(b.0 + 1_000_000_000_000));
        assert!(c.now().0 > b.0 + 1_000_000_000_000);
        let v1 = Version {
            time: Hlc(5),
            replica: ReplicaId(9),
        };
        let v2 = Version {
            time: Hlc(6),
            replica: ReplicaId(1),
        };
        assert!(v2 > v1, "versions order by time first");
    }

    #[test]
    fn same_data_ignores_location() {
        let c = |pack| ChunkRef {
            hash: ChunkHash([1; 32]),
            len: 10,
            pack: PackId(pack),
            offset: 0,
            stored_len: 10,
            codec: Codec::Raw,
        };
        let a = Content::File {
            size: 10,
            chunks: vec![c(1)],
        };
        let b = Content::File {
            size: 10,
            chunks: vec![c(2)],
        };
        assert!(a.same_data(&b));
        assert!(!a.same_data(&Content::Inline(vec![0; 10])));
    }
}
