//! Persistent local state of one synced directory, in `.rustfuse/index.redb`
//! (`docs/design.md` §5.4).

use std::collections::HashMap;
use std::path::Path;

use redb::{Database, Durability, ReadableDatabase, ReadableTable, TableDefinition};
use rustfuse_types::{ChunkRef, Entry, NodeId, ReplicaId};
use serde::{Deserialize, Serialize};

use crate::tree::{NodeChange, Tree};
use crate::{Error, Result};

const STATE: TableDefinition<&str, &[u8]> = TableDefinition::new("state");
const NODES: TableDefinition<u128, &[u8]> = TableDefinition::new("nodes");
const STATS: TableDefinition<u128, &[u8]> = TableDefinition::new("stats");
const CHUNKS: TableDefinition<&[u8], &[u8]> = TableDefinition::new("chunks");

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Role {
    /// Created the volume and pushes changes.
    Writer,
    /// Applies changes from the volume.
    Follower,
}

/// Settings of one synced directory. Credentials are not stored; they come
/// from the environment (`RUSTFUSE_ACCESS_KEY` / `RUSTFUSE_SECRET_KEY`) or
/// the standard AWS provider chain.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Config {
    pub volume: String,
    pub bucket: String,
    pub endpoint: Option<String>,
    pub region: String,
    /// Bucket prefix under which volumes live.
    pub prefix: String,
    pub path_style: bool,
    pub replica: ReplicaId,
    /// Unique seed for this replica's pack ids.
    pub seed: [u8; 16],
    pub role: Role,
}

impl Config {
    /// Key prefix of this volume inside the bucket.
    pub fn volume_prefix(&self) -> String {
        let base = self.prefix.trim_matches('/');
        if base.is_empty() {
            format!("volumes/{}", self.volume)
        } else {
            format!("{base}/volumes/{}", self.volume)
        }
    }
}

/// What a scan remembers about a file on disk to detect changes cheaply.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatKey {
    pub dev: u64,
    pub ino: u64,
    pub size: u64,
    pub mtime: i64,
    pub ctime: i64,
    pub mode: u32,
}

impl StatKey {
    #[cfg(unix)]
    pub fn of(m: &std::fs::Metadata) -> StatKey {
        use std::os::unix::fs::MetadataExt;
        StatKey {
            dev: m.dev(),
            ino: m.ino(),
            size: m.size(),
            mtime: m.mtime() * 1_000_000_000 + m.mtime_nsec(),
            ctime: m.ctime() * 1_000_000_000 + m.ctime_nsec(),
            mode: m.mode(),
        }
    }

    /// Same content and permissions as far as `stat` can tell.
    pub fn unchanged(&self, other: &StatKey) -> bool {
        self == other
    }

    /// Same content after a rename (renames update ctime on most systems).
    pub fn unchanged_after_move(&self, other: &StatKey) -> bool {
        self.dev == other.dev
            && self.ino == other.ino
            && self.size == other.size
            && self.mtime == other.mtime
            && self.mode == other.mode
    }
}

pub struct State {
    pub config: Config,
    pub applied: u64,
    pub counter: u64,
}

/// Changes to persist atomically after a push or pull.
#[derive(Default)]
pub struct Commit<'a> {
    pub applied: u64,
    pub counter: u64,
    pub changes: &'a [NodeChange],
    pub stats: Vec<(NodeId, Option<StatKey>)>,
    pub chunks: Vec<ChunkRef>,
}

fn db_err(e: impl std::fmt::Display) -> Error {
    Error::Index(e.to_string())
}

fn enc<T: Serialize>(v: &T) -> Vec<u8> {
    postcard::to_allocvec(v).expect("serialization cannot fail")
}

fn dec<'a, T: Deserialize<'a>>(b: &'a [u8]) -> Result<T> {
    postcard::from_bytes(b).map_err(|e| Error::Index(format!("corrupt index: {e}")))
}

pub struct Index {
    db: Database,
}

impl Index {
    pub fn create(path: &Path, config: &Config) -> Result<Index> {
        let db = Database::create(path).map_err(db_err)?;
        let tx = db.begin_write().map_err(db_err)?;
        {
            let mut s = tx.open_table(STATE).map_err(db_err)?;
            s.insert("config", serde_json::to_vec(config).unwrap().as_slice())
                .map_err(db_err)?;
            s.insert("applied", 0u64.to_le_bytes().as_slice())
                .map_err(db_err)?;
            s.insert("counter", 1u64.to_le_bytes().as_slice())
                .map_err(db_err)?;
            tx.open_table(NODES).map_err(db_err)?;
            tx.open_table(STATS).map_err(db_err)?;
            tx.open_table(CHUNKS).map_err(db_err)?;
        }
        tx.commit().map_err(db_err)?;
        Ok(Index { db })
    }

    pub fn open(path: &Path) -> Result<Index> {
        Ok(Index {
            db: Database::open(path).map_err(db_err)?,
        })
    }

    pub fn state(&self) -> Result<State> {
        let tx = self.db.begin_read().map_err(db_err)?;
        let s = tx.open_table(STATE).map_err(db_err)?;
        let get = |k: &str| -> Result<Vec<u8>> {
            Ok(s.get(k)
                .map_err(db_err)?
                .ok_or_else(|| Error::Index(format!("missing {k}")))?
                .value()
                .to_vec())
        };
        let u64_of = |b: Vec<u8>| -> Result<u64> {
            Ok(u64::from_le_bytes(
                b.try_into().map_err(|_| Error::Index("bad counter".into()))?,
            ))
        };
        Ok(State {
            config: serde_json::from_slice(&get("config")?).map_err(db_err)?,
            applied: u64_of(get("applied")?)?,
            counter: u64_of(get("counter")?)?,
        })
    }

    pub fn tree(&self) -> Result<Tree> {
        let tx = self.db.begin_read().map_err(db_err)?;
        let t = tx.open_table(NODES).map_err(db_err)?;
        let mut entries = Vec::new();
        for item in t.iter().map_err(db_err)? {
            let (k, v) = item.map_err(db_err)?;
            entries.push((NodeId(k.value()), dec::<Entry>(v.value())?));
        }
        Ok(Tree::from_entries(entries))
    }

    pub fn stats(&self) -> Result<HashMap<NodeId, StatKey>> {
        let tx = self.db.begin_read().map_err(db_err)?;
        let t = tx.open_table(STATS).map_err(db_err)?;
        let mut out = HashMap::new();
        for item in t.iter().map_err(db_err)? {
            let (k, v) = item.map_err(db_err)?;
            out.insert(NodeId(k.value()), dec(v.value())?);
        }
        Ok(out)
    }

    pub fn chunks(&self) -> Result<Vec<ChunkRef>> {
        let tx = self.db.begin_read().map_err(db_err)?;
        let t = tx.open_table(CHUNKS).map_err(db_err)?;
        let mut out = Vec::new();
        for item in t.iter().map_err(db_err)? {
            out.push(dec(item.map_err(db_err)?.1.value())?);
        }
        Ok(out)
    }

    pub fn commit(&self, c: Commit<'_>) -> Result<()> {
        let mut tx = self.db.begin_write().map_err(db_err)?;
        tx.set_durability(Durability::Immediate).map_err(db_err)?;
        {
            let mut s = tx.open_table(STATE).map_err(db_err)?;
            s.insert("applied", c.applied.to_le_bytes().as_slice())
                .map_err(db_err)?;
            s.insert("counter", c.counter.to_le_bytes().as_slice())
                .map_err(db_err)?;
            let mut nodes = tx.open_table(NODES).map_err(db_err)?;
            let mut stats = tx.open_table(STATS).map_err(db_err)?;
            for ch in c.changes {
                match &ch.new {
                    Some(e) => {
                        nodes.insert(ch.node.0, enc(e).as_slice()).map_err(db_err)?;
                    }
                    None => {
                        nodes.remove(ch.node.0).map_err(db_err)?;
                        stats.remove(ch.node.0).map_err(db_err)?;
                    }
                }
            }
            for (id, st) in &c.stats {
                match st {
                    Some(st) => stats.insert(id.0, enc(st).as_slice()).map_err(db_err)?,
                    None => stats.remove(id.0).map_err(db_err)?,
                };
            }
            let mut chunks = tx.open_table(CHUNKS).map_err(db_err)?;
            for r in &c.chunks {
                chunks
                    .insert(r.hash.0.as_slice(), enc(r).as_slice())
                    .map_err(db_err)?;
            }
        }
        tx.commit().map_err(db_err)?;
        Ok(())
    }
}
