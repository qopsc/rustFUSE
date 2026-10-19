//! Persistent local state of one synced directory, in `.rustfuse/index.redb`
//! (`docs/design.md` §5.4).
//!
//! Holds two trees: the volume as the log describes it (through `applied`)
//! and what is on this machine's disk, which is the log plus local
//! changesets not yet accepted (the outbox). `pending` lists nodes where
//! the disk may still lag behind, so an interrupted apply resumes.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use redb::{Database, Durability, ReadableDatabase, ReadableTable, TableDefinition};
use rustfuse_types::{Changeset, ChunkRef, Hlc, Node, NodeId, ReplicaId};
use serde::{Deserialize, Serialize};

use crate::tree::Tree;
use crate::{Error, Result};

const STATE: TableDefinition<&str, &[u8]> = TableDefinition::new("state");
const LOG_NODES: TableDefinition<u128, &[u8]> = TableDefinition::new("log_nodes");
const LOG_REMOVED: TableDefinition<u128, &[u8]> = TableDefinition::new("log_removed");
const DISK_NODES: TableDefinition<u128, &[u8]> = TableDefinition::new("disk_nodes");
const DISK_REMOVED: TableDefinition<u128, &[u8]> = TableDefinition::new("disk_removed");
const PENDING: TableDefinition<u128, ()> = TableDefinition::new("pending");
const OUTBOX: TableDefinition<u64, &[u8]> = TableDefinition::new("outbox");
const STATS: TableDefinition<u128, &[u8]> = TableDefinition::new("stats");
const CHUNKS: TableDefinition<&[u8], &[u8]> = TableDefinition::new("chunks");

/// Settings of one synced directory. Credentials and the hub token are not
/// stored; they come from the environment.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Config {
    pub volume: String,
    pub bucket: String,
    pub endpoint: Option<String>,
    pub region: String,
    /// Bucket prefix under which volumes live.
    pub prefix: String,
    pub path_style: bool,
    /// Hub address: `host:port`, or `tls://host:port` for TLS.
    pub hub: Option<String>,
    /// CA certificates (PEM) to trust for the hub's TLS certificate
    /// instead of the system's roots.
    #[serde(default)]
    pub hub_ca: Option<PathBuf>,
    pub replica: ReplicaId,
    /// Unique seed for this replica's pack ids.
    pub seed: [u8; 16],
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

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Counters {
    /// Last log sequence applied.
    pub applied: u64,
    /// Next node counter.
    pub counter: u64,
    /// Last clock reading.
    pub clock: Hlc,
    /// Next outbox number.
    pub next_local: u64,
}

pub struct Loaded {
    pub config: Config,
    pub counters: Counters,
    pub log: Tree,
    pub disk: Tree,
    pub outbox: Vec<Changeset>,
    pub pending: Vec<NodeId>,
    pub stats: HashMap<NodeId, StatKey>,
    pub chunks: Vec<ChunkRef>,
}

/// Changes to persist atomically.
#[derive(Default)]
pub struct Commit<'a> {
    pub counters: Option<Counters>,
    /// Nodes to write from the log tree.
    pub log: Option<(&'a Tree, Vec<NodeId>)>,
    /// Nodes to write from the disk tree.
    pub disk: Option<(&'a Tree, Vec<NodeId>)>,
    pub outbox_put: Vec<&'a Changeset>,
    pub outbox_remove: Vec<u64>,
    pub pending_add: Vec<NodeId>,
    pub pending_clear: bool,
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
            let c = Counters {
                counter: 1,
                next_local: 1,
                ..Counters::default()
            };
            s.insert("counters", enc(&c).as_slice()).map_err(db_err)?;
            tx.open_table(LOG_NODES).map_err(db_err)?;
            tx.open_table(LOG_REMOVED).map_err(db_err)?;
            tx.open_table(DISK_NODES).map_err(db_err)?;
            tx.open_table(DISK_REMOVED).map_err(db_err)?;
            tx.open_table(PENDING).map_err(db_err)?;
            tx.open_table(OUTBOX).map_err(db_err)?;
            tx.open_table(STATS).map_err(db_err)?;
            tx.open_table(CHUNKS).map_err(db_err)?;
        }
        tx.commit().map_err(db_err)?;
        Ok(Index { db })
    }

    pub fn open(path: &Path) -> Result<Index> {
        let db = Database::open(path).map_err(|e| match e {
            redb::DatabaseError::DatabaseAlreadyOpen => Error::Index(
                "another rustfuse process (probably `rustfuse sync`) is using this directory".into(),
            ),
            other => db_err(other),
        })?;
        Ok(Index { db })
    }

    pub fn config(&self) -> Result<Config> {
        let tx = self.db.begin_read().map_err(db_err)?;
        let s = tx.open_table(STATE).map_err(|e| {
            Error::Index(format!(
                "{e} (directories from older rustfuse versions must be re-attached)"
            ))
        })?;
        let raw = s
            .get("config")
            .map_err(db_err)?
            .ok_or_else(|| Error::Index("missing config".into()))?;
        serde_json::from_slice(raw.value()).map_err(|e| {
            Error::Index(format!(
                "{e} (directories from older rustfuse versions must be re-attached)"
            ))
        })
    }

    pub fn load(&self) -> Result<Loaded> {
        let config = self.config()?;
        let tx = self.db.begin_read().map_err(db_err)?;
        let s = tx.open_table(STATE).map_err(db_err)?;
        let counters: Counters = dec(s
            .get("counters")
            .map_err(db_err)?
            .ok_or_else(|| Error::Index("missing counters".into()))?
            .value())?;

        let nodes = |def: TableDefinition<u128, &[u8]>| -> Result<Vec<(NodeId, Node)>> {
            let t = tx.open_table(def).map_err(db_err)?;
            let mut out = Vec::new();
            for item in t.iter().map_err(db_err)? {
                let (k, v) = item.map_err(db_err)?;
                out.push((NodeId(k.value()), dec(v.value())?));
            }
            Ok(out)
        };
        let log = Tree::from_parts(nodes(LOG_NODES)?, nodes(LOG_REMOVED)?);
        let disk = Tree::from_parts(nodes(DISK_NODES)?, nodes(DISK_REMOVED)?);

        let mut outbox = Vec::new();
        for item in tx.open_table(OUTBOX).map_err(db_err)?.iter().map_err(db_err)? {
            outbox.push(dec(item.map_err(db_err)?.1.value())?);
        }
        let mut pending = Vec::new();
        for item in tx.open_table(PENDING).map_err(db_err)?.iter().map_err(db_err)? {
            pending.push(NodeId(item.map_err(db_err)?.0.value()));
        }
        let mut stats = HashMap::new();
        for item in tx.open_table(STATS).map_err(db_err)?.iter().map_err(db_err)? {
            let (k, v) = item.map_err(db_err)?;
            stats.insert(NodeId(k.value()), dec(v.value())?);
        }
        let mut chunks = Vec::new();
        for item in tx.open_table(CHUNKS).map_err(db_err)?.iter().map_err(db_err)? {
            chunks.push(dec(item.map_err(db_err)?.1.value())?);
        }
        Ok(Loaded {
            config,
            counters,
            log,
            disk,
            outbox,
            pending,
            stats,
            chunks,
        })
    }

    pub fn commit(&self, c: Commit<'_>) -> Result<()> {
        let mut tx = self.db.begin_write().map_err(db_err)?;
        tx.set_durability(Durability::Immediate).map_err(db_err)?;
        {
            if let Some(counters) = &c.counters {
                let mut s = tx.open_table(STATE).map_err(db_err)?;
                s.insert("counters", enc(counters).as_slice()).map_err(db_err)?;
            }
            let write_tree = |tree: &Tree,
                              ids: &[NodeId],
                              nodes: TableDefinition<u128, &[u8]>,
                              removed: TableDefinition<u128, &[u8]>|
             -> Result<()> {
                let mut n = tx.open_table(nodes).map_err(db_err)?;
                let mut r = tx.open_table(removed).map_err(db_err)?;
                for id in ids {
                    match tree.node(*id) {
                        Some(node) => n.insert(id.0, enc(node).as_slice()).map_err(db_err)?,
                        None => n.remove(id.0).map_err(db_err)?,
                    };
                    match tree.removed(*id) {
                        Some(node) => r.insert(id.0, enc(node).as_slice()).map_err(db_err)?,
                        None => r.remove(id.0).map_err(db_err)?,
                    };
                }
                Ok(())
            };
            if let Some((tree, ids)) = &c.log {
                write_tree(tree, ids, LOG_NODES, LOG_REMOVED)?;
            }
            if let Some((tree, ids)) = &c.disk {
                write_tree(tree, ids, DISK_NODES, DISK_REMOVED)?;
            }
            let mut outbox = tx.open_table(OUTBOX).map_err(db_err)?;
            for cs in &c.outbox_put {
                outbox.insert(cs.local, enc(cs).as_slice()).map_err(db_err)?;
            }
            for local in &c.outbox_remove {
                outbox.remove(local).map_err(db_err)?;
            }
            if c.pending_clear {
                // Dropping the table is far cheaper than deleting each row.
                tx.delete_table(PENDING).map_err(db_err)?;
            }
            let mut pending = tx.open_table(PENDING).map_err(db_err)?;
            for id in &c.pending_add {
                pending.insert(id.0, ()).map_err(db_err)?;
            }
            let mut stats = tx.open_table(STATS).map_err(db_err)?;
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
