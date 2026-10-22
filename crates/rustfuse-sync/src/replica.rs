//! One synced directory: ties the index, scanner, chunker, applier and the
//! hub together.
//!
//! Local changes are captured into changesets in an outbox; content goes to
//! the bucket first (or to a local spool while offline). The hub orders
//! everyone's changesets into one log. Every machine applies that log with
//! the same deterministic rules, and its disk shows the log plus its own
//! changesets still in flight.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{BufReader, Read};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bytes::Bytes;
use futures::{StreamExt, TryStreamExt};
use rayon::prelude::*;
use rustfuse_chunk::{PackBuilder, SealedPack};
use rustfuse_hub::{Event, HubClient, Volume};
use rustfuse_store::ObjectStore;
use rustfuse_types::{
    Changeset, ChunkRef, Clock, Content, Entry, FORMAT_VERSION, Fields, INLINE_MAX, NodeId, Op, PackId,
};
use tracing::{debug, info, warn};

use crate::apply::Applier;
use crate::git::{GitTidy, Plan};
use crate::ignore::Ignore;
use crate::index::{Commit, Config, Counters, Index, StatKey};
use crate::metrics::SyncMetrics;
use crate::scan::{Change, Kind, ScanRoot, Scanner};
use crate::tree::Tree;
use crate::{Error, IoContext, Result, STATE_DIR};

/// A snapshot is written every this many changesets so new machines do not
/// replay the whole log.
const SNAPSHOT_EVERY: u64 = 256;
const UPLOAD_CONCURRENCY: usize = 8;
/// Large captures (an initial import) are split so no single changeset is
/// huge.
const MAX_OPS_PER_CHANGESET: usize = 20_000;
/// Longest a burst of local changes is held back while the tree stays busy.
const MAX_DEBOUNCE: Duration = Duration::from_secs(1);
/// Rounds of "capture local edits, then apply" before giving up on a busy
/// directory for now.
const RECONCILE_ATTEMPTS: usize = 5;
/// How long a pack checked before sending stays trusted; well below the
/// cleanup delay (`gc::GcOptions::delay`).
const PACK_CHECK_TTL: Duration = Duration::from_secs(300);
const PACK_CHECK_CONCURRENCY: usize = 16;

/// Where the hub is and how to authenticate.
#[derive(Clone, Debug)]
pub struct HubSpec {
    /// `host:port`, without any scheme.
    pub addr: String,
    pub token: String,
    /// Connect over TLS with these settings.
    pub tls: Option<Arc<rustfuse_hub::tls::ClientConfig>>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CaptureStats {
    pub changesets: usize,
    pub ops: usize,
    pub files_read: usize,
    pub bytes_read: u64,
    pub packs: usize,
    pub bytes_uploaded: u64,
    /// Packs kept locally because the bucket was unreachable.
    pub packs_spooled: usize,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SyncStats {
    pub captured: Option<CaptureStats>,
    pub pushed: usize,
    pub received: usize,
}

pub struct Replica {
    root: PathBuf,
    config: Config,
    index: Index,
    /// The volume as of `counters.applied`.
    log: Tree,
    /// What is on disk: the log plus the outbox.
    tree: Tree,
    /// Local changesets not yet seen in the log, oldest first.
    outbox: Vec<Changeset>,
    /// Nodes the outbox changes relative to the log.
    outbox_nodes: HashSet<NodeId>,
    /// Nodes where the disk may lag behind the log plus outbox.
    pending: HashSet<NodeId>,
    stats: HashMap<NodeId, StatKey>,
    counters: Counters,
    clock: Clock,
    volume: Volume,
    builder: Arc<Mutex<PackBuilder>>,
    ignore: Ignore,
    /// Taken while the git pass runs on a blocking thread.
    git: Option<GitTidy>,
    /// Packs found in the bucket (and not marked for deletion), and when.
    checked_packs: HashMap<PackId, Instant>,
    metrics: Arc<SyncMetrics>,
    /// Content is fetched on first use through a mount (`crate::lazy`).
    lazy: bool,
    /// Nodes changed on disk this run, for the File Provider.
    changes: crate::provider::ChangeLog,
    /// What the user asked to keep downloaded (lazy directories).
    pins: Option<Arc<crate::lazy::Pins>>,
    /// Told of every change the watcher sees (see [`Replica::watch_disk`]).
    disk_watch: Option<DiskWatch>,
}

/// Called with the paths (relative to the root) of every change seen on
/// disk while syncing, or `None` when changes may have been missed.
pub type DiskWatch = Arc<dyn Fn(Option<&[PathBuf]>) + Send + Sync>;

/// Creates the state folder, hidden from git (and tools honouring
/// .gitignore) by a catch-all ignore file inside it.
fn create_state_dir(root: &Path) -> Result<()> {
    let dir = root.join(STATE_DIR);
    fs::create_dir_all(&dir).at(&dir)?;
    let gi = dir.join(".gitignore");
    fs::write(&gi, "*\n").at(&gi)?;
    Ok(())
}

fn index_path(root: &Path) -> PathBuf {
    root.join(STATE_DIR).join("index.redb")
}

fn content_chunks(ops: &[Op]) -> Vec<ChunkRef> {
    ops.iter()
        .flat_map(|op| match op {
            Op::Upsert {
                entry:
                    Entry {
                        content: Content::File { chunks, .. },
                        ..
                    },
                ..
            } => chunks.clone(),
            _ => Vec::new(),
        })
        .collect()
}

impl Replica {
    /// Reads the configuration of an existing synced directory.
    pub fn config_of(root: &Path) -> Result<Config> {
        let path = index_path(root);
        if !path.exists() {
            return Err(Error::NotInitialized(root.to_owned()));
        }
        Index::open(&path)?.config()
    }

    /// Creates a new volume from the contents of `root`. Its content is
    /// uploaded; the changesets wait in the outbox for the first sync.
    pub async fn init(
        root: &Path,
        config: Config,
        store: Arc<dyn ObjectStore>,
    ) -> Result<(Self, CaptureStats)> {
        if root.join(STATE_DIR).exists() {
            return Err(Error::AlreadyInitialized(root.to_owned()));
        }
        let volume = Volume::new(store.clone());
        volume.create().await?;
        create_state_dir(root)?;
        let index = Index::create(&index_path(root), &config)?;
        let mut r = Replica::load(root, index, store)?;
        let stats = r.capture(None).await?.unwrap_or_default();
        Ok((r, stats))
    }

    /// Materializes an existing volume into the empty directory `root`
    /// from the bucket's snapshot and log.
    pub async fn attach(root: &Path, config: Config, store: Arc<dyn ObjectStore>) -> Result<Self> {
        Replica::attach_with(root, config, store, false).await
    }

    /// Like [`attach`](Self::attach); with `lazy`, files are written as
    /// placeholders and fetched when a mount first opens them
    /// ([`crate::lazy`]).
    pub async fn attach_with(
        root: &Path,
        config: Config,
        store: Arc<dyn ObjectStore>,
        lazy: bool,
    ) -> Result<Self> {
        let volume = Volume::new(store.clone());
        volume.check().await?;
        if root.exists() && fs::read_dir(root).at(root)?.next().is_some() {
            return Err(Error::NotEmpty(root.to_owned()));
        }
        create_state_dir(root)?;
        if lazy {
            crate::lazy::enable(root)?;
        }
        let index = Index::create(&index_path(root), &config)?;
        let mut r = Replica::load(root, index, store)?;
        r.restore_from_bucket().await?;
        Ok(r)
    }

    pub fn open(root: &Path, store: Arc<dyn ObjectStore>) -> Result<Self> {
        let path = index_path(root);
        if !path.exists() {
            return Err(Error::NotInitialized(root.to_owned()));
        }
        Replica::load(root, Index::open(&path)?, store)
    }

    fn load(root: &Path, index: Index, store: Arc<dyn ObjectStore>) -> Result<Self> {
        let l = index.load()?;
        let mut builder = PackBuilder::new(l.config.seed);
        for r in &l.chunks {
            builder.insert_known(*r);
        }
        let mut outbox_nodes = HashSet::new();
        if !l.outbox.is_empty() {
            let mut t = l.log.clone();
            for cs in &l.outbox {
                outbox_nodes.extend(t.apply(cs.version, &cs.ops));
            }
        }
        // Watch events carry absolute, resolved paths.
        let root = root.canonicalize().at(root)?;
        let lazy = crate::lazy::is_lazy(&root);
        if lazy {
            let pruned = crate::lazy::prune(&root, &l.disk)?;
            if pruned > 0 {
                debug!(pruned, "removed placeholder records no longer needed");
            }
        }
        Ok(Replica {
            lazy,
            git: Some(GitTidy::load(&root)),
            ignore: Ignore::load(&root).map_err(|e| Error::Corrupt(format!("ignore file: {e}")))?,
            root,
            clock: Clock::new(l.counters.clock),
            counters: l.counters,
            config: l.config,
            index,
            log: l.log,
            tree: l.disk,
            outbox: l.outbox,
            outbox_nodes,
            pending: l.pending.into_iter().collect(),
            stats: l.stats,
            volume: Volume::new(store),
            builder: Arc::new(Mutex::new(builder)),
            checked_packs: HashMap::new(),
            metrics: Arc::default(),
            changes: crate::provider::ChangeLog::new(),
            pins: None,
            disk_watch: None,
        })
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    pub fn applied(&self) -> u64 {
        self.counters.applied
    }

    pub fn store(&self) -> &Arc<dyn ObjectStore> {
        self.volume.store()
    }

    /// What is on disk, as far as the index knows.
    pub fn tree(&self) -> &Tree {
        &self.tree
    }

    /// The synced directory (canonical).
    pub fn root_dir(&self) -> &Path {
        &self.root
    }

    /// Shares the pins of a lazy directory's [`Hydrator`](crate::lazy::Hydrator)
    /// with the File Provider answers.
    pub fn set_pins(&mut self, pins: Arc<crate::lazy::Pins>) {
        self.pins = Some(pins);
    }

    /// Passes what the watcher sees to `f`, whoever made the change: this
    /// replica applying other machines' edits, its git pass, or anyone
    /// else. A mount uses it to keep the kernel's cache current.
    pub fn watch_disk(&mut self, f: DiskWatch) {
        self.disk_watch = Some(f);
    }

    pub(crate) fn pins(&self) -> Option<&crate::lazy::Pins> {
        self.pins.as_deref()
    }

    pub(crate) fn change_log_mut(&mut self) -> &mut crate::provider::ChangeLog {
        &mut self.changes
    }

    pub(crate) fn change_log(&self) -> &crate::provider::ChangeLog {
        &self.changes
    }

    /// Local changesets not yet accepted by the hub.
    pub fn outbox_len(&self) -> usize {
        self.outbox.len()
    }

    /// Counters and gauges for `/metrics`, updated as this replica works.
    pub fn metrics(&self) -> Arc<SyncMetrics> {
        self.metrics.position(self.counters.applied, self.outbox.len());
        self.metrics.clone()
    }

    fn spool_dir(&self) -> PathBuf {
        self.root.join(STATE_DIR).join("spool")
    }

    fn save_counters(&mut self) -> Counters {
        self.counters.clock = self.clock.last();
        self.counters.clone()
    }

    // ---- receiving the log -------------------------------------------------

    async fn restore_from_bucket(&mut self) -> Result<()> {
        if let Some(snap) = self.volume.latest_snapshot().await? {
            info!(seq = snap.seq, nodes = snap.nodes.len(), "restoring snapshot");
            for (_, n) in snap.nodes.iter().chain(&snap.removed) {
                self.clock.observe(n.versions.loc.time.max(n.versions.data.time));
            }
            self.log = Tree::from_snapshot(&snap);
            self.counters.applied = snap.seq;
            let ids: Vec<NodeId> = snap
                .nodes
                .iter()
                .chain(&snap.removed)
                .map(|(id, _)| *id)
                .collect();
            self.pending.extend(&ids);
            let counters = self.save_counters();
            self.index.commit(Commit {
                counters: Some(counters),
                log: Some((&self.log, ids.clone())),
                pending_add: ids,
                ..Commit::default()
            })?;
        }
        let mut batch = Vec::new();
        for seq in self.volume.changesets_after(self.counters.applied).await? {
            batch.push(self.volume.get_changeset(seq).await?);
            if batch.len() == 256 {
                self.apply_log(std::mem::take(&mut batch)).await?;
            }
        }
        self.apply_log(batch).await?;
        self.reconcile_with_capture().await?;
        Ok(())
    }

    /// Applies changesets from the log (in order) and updates the disk.
    pub async fn receive(&mut self, batch: Vec<Changeset>) -> Result<()> {
        self.apply_log(batch).await?;
        self.reconcile_with_capture().await
    }

    /// Like [`receive`](Self::receive), but updates the disk only once the
    /// log is applied up to `head`: a machine catching up never fetches
    /// files that are gone again by then, whose content cleanup may already
    /// have removed ([`crate::gc`]).
    async fn receive_upto(&mut self, batch: Vec<Changeset>, head: u64) -> Result<()> {
        self.apply_log(batch).await?;
        if self.counters.applied >= head {
            self.reconcile_with_capture().await?;
        }
        Ok(())
    }

    async fn apply_log(&mut self, batch: Vec<Changeset>) -> Result<()> {
        let t0 = Instant::now();
        let n = batch.len();
        let before = self.counters.applied;
        let mut log_ids = Vec::new();
        let mut accepted = Vec::new();
        let mut chunks = Vec::new();
        for cs in batch {
            if cs.seq <= self.counters.applied {
                continue;
            }
            if cs.seq != self.counters.applied + 1 {
                return Err(Error::Corrupt(format!(
                    "log gap: expected changeset {}, got {}",
                    self.counters.applied + 1,
                    cs.seq
                )));
            }
            debug!(seq = cs.seq, ops = cs.ops.len(), "applying changeset");
            self.clock.observe(cs.version.time);
            log_ids.extend(self.log.apply(cs.version, &cs.ops));
            chunks.extend(content_chunks(&cs.ops));
            self.counters.applied = cs.seq;
            if cs.version.replica == self.config.replica {
                if let Some(pos) = self.outbox.iter().position(|o| o.local == cs.local) {
                    self.outbox.remove(pos);
                    accepted.push(cs.local);
                }
                if cs.seq == 1 || cs.seq.is_multiple_of(SNAPSHOT_EVERY) {
                    self.volume.put_snapshot(&self.log.snapshot(cs.seq)).await?;
                }
            }
        }
        let mut pending_add = log_ids.clone();
        if !accepted.is_empty() {
            pending_add.extend(self.outbox_nodes.iter().copied());
        }
        self.pending.extend(&pending_add);
        {
            let mut b = self.builder.lock().unwrap();
            for r in &chunks {
                b.insert_known(*r);
            }
        }
        let counters = self.save_counters();
        self.index.commit(Commit {
            counters: Some(counters),
            log: Some((&self.log, log_ids)),
            outbox_remove: accepted,
            pending_add,
            chunks,
            ..Commit::default()
        })?;
        debug!(
            changesets = n,
            ms = t0.elapsed().as_millis() as u64,
            "applied changesets to the log"
        );
        self.metrics.received_total.add(self.counters.applied - before);
        self.metrics.apply_seconds.observe(t0.elapsed());
        self.metrics.position(self.counters.applied, self.outbox.len());
        Ok(())
    }

    /// Brings the disk in line with the log plus outbox, capturing local
    /// edits that would otherwise be overwritten.
    async fn reconcile_with_capture(&mut self) -> Result<()> {
        for _ in 0..RECONCILE_ATTEMPTS {
            match self.reconcile().await {
                Err(Error::LocalChanges(paths)) => {
                    debug!(paths = paths.len(), "capturing local edits before applying");
                    self.capture(Some(paths)).await?;
                }
                other => return other,
            }
        }
        self.reconcile().await
    }

    async fn reconcile(&mut self) -> Result<()> {
        if self.pending.is_empty() {
            return Ok(());
        }
        // The disk may differ from the log wherever the outbox touched it
        // before; replaying the outbox on the new log decides what stays.
        let before = std::mem::take(&mut self.outbox_nodes);
        self.pending.extend(before);
        let replayed = if self.outbox.is_empty() {
            None
        } else {
            let mut t = self.log.clone();
            let mut touched = HashSet::new();
            for cs in &self.outbox {
                touched.extend(t.apply(cs.version, &cs.ops));
            }
            self.pending.extend(&touched);
            self.outbox_nodes = touched;
            Some(t)
        };
        let target = replayed.as_ref().unwrap_or(&self.log);
        let t0 = Instant::now();
        let changes = self.tree.diff(target, self.pending.iter().copied());
        let dirty = self.precheck(&changes);
        debug!(
            changes = changes.len(),
            ms = t0.elapsed().as_millis() as u64,
            "reconcile: diff + precheck"
        );
        if !dirty.is_empty() {
            return Err(Error::LocalChanges(dirty));
        }
        let applier = Applier {
            root: &self.root,
            store: self.volume.store().clone(),
            lazy: self.lazy,
        };
        let t0 = Instant::now();
        let stats = applier.apply(&changes).await?;
        debug!(ms = t0.elapsed().as_millis() as u64, "reconcile: apply to disk");
        let t0 = Instant::now();
        // Only nodes that really differ need rewriting in the index; after
        // a large catch-up most pending nodes already match.
        let differ: HashSet<NodeId> = self
            .pending
            .iter()
            .copied()
            .filter(|&id| {
                self.tree.node(id) != target.node(id) || self.tree.removed(id) != target.removed(id)
            })
            .collect();
        self.changes.record(differ.iter().copied());
        self.tree.copy_from(target, &differ);
        let ids: Vec<NodeId> = differ.into_iter().collect();
        for (id, st) in &stats {
            match st {
                Some(st) => self.stats.insert(*id, *st),
                None => self.stats.remove(id),
            };
        }
        self.pending.clear();
        self.index.commit(Commit {
            disk: Some((&self.tree, ids)),
            pending_clear: true,
            stats,
            ..Commit::default()
        })?;
        debug!(ms = t0.elapsed().as_millis() as u64, "reconcile: index commit");
        if !changes.is_empty() {
            info!(
                applied = self.counters.applied,
                changes = changes.len(),
                "updated local files"
            );
        }
        Ok(())
    }

    /// Paths where applying `changes` would overwrite something not yet
    /// captured: a modified or deleted file, or a new file in the way.
    fn precheck(&self, changes: &[crate::tree::NodeChange]) -> Vec<PathBuf> {
        let mut dirty = Vec::new();
        for c in changes {
            if let (Some(old), Some(rel)) = (&c.old, &c.old_path)
                && !old.content.is_dir()
            {
                let now = fs::symlink_metadata(self.root.join(rel))
                    .ok()
                    .map(|m| StatKey::of(&m));
                let known = self.stats.get(&c.node);
                if !matches!((known, now), (Some(k), Some(n)) if k.unchanged(&n)) {
                    dirty.push(rel.clone());
                }
            }
            if let Some(rel) = &c.new_path
                && c.old_path.as_ref() != Some(rel)
                && fs::symlink_metadata(self.root.join(rel)).is_ok()
                && self.tree.resolve(rel).is_none()
                && !self.ignore.is_ignored(rel, false)
            {
                dirty.push(rel.clone());
            }
        }
        dirty
    }

    // ---- capturing local changes -------------------------------------------

    /// Changes on disk not yet captured, without reading content.
    pub fn pending_changes(&self) -> Result<Vec<Change>> {
        let mut counter = self.counters.counter;
        Scanner {
            root: &self.root,
            tree: &self.tree,
            stats: &self.stats,
            ignore: &self.ignore,
            replica: self.config.replica,
            counter: &mut counter,
        }
        .scan(vec![ScanRoot {
            node: NodeId::ROOT,
            path: PathBuf::new(),
            recursive: true,
        }])
    }

    /// Scans for local changes (everything, or around the given paths),
    /// stores new content and adds changesets to the outbox. Returns `None`
    /// when nothing changed.
    pub async fn capture(&mut self, paths: Option<Vec<PathBuf>>) -> Result<Option<CaptureStats>> {
        let t0 = Instant::now();
        let stats = self.capture_changes(paths).await?;
        if let Some(s) = &stats {
            let m = &self.metrics;
            m.capture_seconds.observe(t0.elapsed());
            m.captures_total.inc();
            m.captured_ops_total.add(s.ops as u64);
            m.bytes_read_total.add(s.bytes_read);
            m.bytes_uploaded_total.add(s.bytes_uploaded);
            m.packs_uploaded_total.add(s.packs as u64);
            m.packs_spooled_total.add(s.packs_spooled as u64);
            m.position(self.counters.applied, self.outbox.len());
        }
        Ok(stats)
    }

    async fn capture_changes(&mut self, paths: Option<Vec<PathBuf>>) -> Result<Option<CaptureStats>> {
        let roots = match paths {
            None => vec![ScanRoot {
                node: NodeId::ROOT,
                path: PathBuf::new(),
                recursive: true,
            }],
            Some(paths) => self.scan_roots(paths),
        };
        let mut counter = self.counters.counter;
        let t0 = Instant::now();
        let changes = Scanner {
            root: &self.root,
            tree: &self.tree,
            stats: &self.stats,
            ignore: &self.ignore,
            replica: self.config.replica,
            counter: &mut counter,
        }
        .scan(roots)?;
        if changes.is_empty() {
            return Ok(None);
        }
        self.counters.counter = counter;

        let reads: Vec<(PathBuf, Kind)> = changes
            .iter()
            .filter_map(|c| match c {
                Change::Upsert {
                    path,
                    read: true,
                    found,
                    ..
                } => Some((self.root.join(path), found.kind)),
                _ => None,
            })
            .collect();
        let mut stats = CaptureStats {
            files_read: reads.len(),
            ..Default::default()
        };
        debug!(
            changes = changes.len(),
            ms = t0.elapsed().as_millis() as u64,
            "capture: scanned"
        );
        let t0 = Instant::now();
        let (contents, uploaded) = self.read_contents(reads).await?;
        debug!(ms = t0.elapsed().as_millis() as u64, "capture: read and uploaded");
        let t0 = Instant::now();
        stats.packs = uploaded.packs;
        stats.bytes_uploaded = uploaded.bytes;
        stats.packs_spooled = uploaded.spooled;

        let mut ops = Vec::with_capacity(changes.len());
        let mut new_stats = Vec::new();
        let mut contents = contents.into_iter();
        for c in changes {
            match c {
                Change::Upsert {
                    node,
                    parent,
                    name,
                    found,
                    read,
                    ..
                } => {
                    let prior = self.tree.node(node);
                    let content = if read {
                        match contents.next().unwrap() {
                            Some(c) => c,
                            None => continue, // vanished while reading; next scan sees it
                        }
                    } else if found.kind == Kind::Dir {
                        Content::Dir
                    } else {
                        prior.map(|n| n.entry.content.clone()).unwrap_or(Content::Dir)
                    };
                    if read {
                        stats.bytes_read += match &content {
                            Content::Inline(d) => d.len() as u64,
                            Content::File { size, .. } => *size,
                            _ => 0,
                        };
                    }
                    new_stats.push((node, Some(found.stat)));
                    let entry = Entry {
                        parent,
                        name,
                        mode: found.stat.mode & 0o777,
                        mtime: found.mtime,
                        content,
                    };
                    let (base, changed) = match prior {
                        None => (None, Fields::ALL),
                        Some(p) => (Some(p.versions), changed_fields(&p.entry, &entry)),
                    };
                    if !changed.is_empty() {
                        ops.push(Op::Upsert {
                            node,
                            entry,
                            base,
                            changed,
                        });
                    }
                }
                Change::Remove { node, .. } => {
                    new_stats.push((node, None));
                    if let Some(n) = self.tree.node(node) {
                        ops.push(Op::Remove {
                            node,
                            base: n.versions,
                        });
                    }
                }
            }
        }
        for (id, st) in &new_stats {
            match st {
                Some(st) => self.stats.insert(*id, *st),
                None => self.stats.remove(id),
            };
        }
        let chunks = content_chunks(&ops);
        let mut new_changesets = Vec::new();
        let mut disk_ids = Vec::new();
        for part in ops.chunks(MAX_OPS_PER_CHANGESET) {
            let cs = Changeset {
                format: FORMAT_VERSION,
                seq: 0,
                version: rustfuse_types::Version {
                    time: self.clock.now(),
                    replica: self.config.replica,
                },
                local: self.counters.next_local,
                ops: part.to_vec(),
            };
            self.counters.next_local += 1;
            let touched = self.tree.apply(cs.version, &cs.ops);
            self.changes.record(touched.iter().copied());
            self.outbox_nodes.extend(&touched);
            disk_ids.extend(touched);
            stats.ops += cs.ops.len();
            new_changesets.push(cs);
        }
        stats.changesets = new_changesets.len();
        let counters = self.save_counters();
        self.index.commit(Commit {
            counters: Some(counters),
            disk: Some((&self.tree, disk_ids)),
            outbox_put: new_changesets.iter().collect(),
            stats: new_stats,
            chunks,
            ..Commit::default()
        })?;
        self.outbox.extend(new_changesets);
        debug!(
            ms = t0.elapsed().as_millis() as u64,
            "capture: ops and index commit"
        );
        if stats.ops == 0 {
            return Ok(None);
        }
        Ok(Some(stats))
    }

    /// Turns changed paths into directories to rescan: each path's parent
    /// (where it appeared, vanished or changed) and the path itself if it is
    /// a known directory.
    fn scan_roots(&self, paths: Vec<PathBuf>) -> Vec<ScanRoot> {
        let mut seen = HashSet::new();
        let mut roots = Vec::new();
        let mut add = |p: &Path, roots: &mut Vec<ScanRoot>| {
            // Walk up to the nearest directory the tree knows.
            let mut dir = p.to_path_buf();
            loop {
                if let Some(node) = self.tree.resolve(&dir)
                    && (node == NodeId::ROOT || self.tree.get(node).is_some_and(|e| e.content.is_dir()))
                {
                    if seen.insert(node) {
                        roots.push(ScanRoot {
                            node,
                            path: dir,
                            recursive: false,
                        });
                    }
                    return;
                }
                if !dir.pop() {
                    return;
                }
            }
        };
        for p in &paths {
            add(p.parent().unwrap_or(Path::new("")), &mut roots);
            if self
                .tree
                .resolve(p)
                .is_some_and(|n| self.tree.get(n).is_some_and(|e| e.content.is_dir()))
            {
                add(p, &mut roots);
            }
        }
        roots
    }

    /// Reads and chunks files in parallel, uploading packs as they fill
    /// (or spooling them locally if the bucket is unreachable). Returns one
    /// content per input (`None` if the file vanished).
    async fn read_contents(&self, reads: Vec<(PathBuf, Kind)>) -> Result<(Vec<Option<Content>>, Uploaded)> {
        let (tx, rx) = tokio::sync::mpsc::channel::<SealedPack>(4);
        let store = self.volume.store().clone();
        let spool = self.spool_dir();
        let uploader = tokio::spawn(async move {
            let results: Vec<Result<Option<u64>>> = tokio_stream_from(rx)
                .map(|pack| {
                    let store = store.clone();
                    let spool = spool.clone();
                    async move {
                        let id = pack.id;
                        let data = pack.data.clone();
                        match rustfuse_chunk::upload_pack(store.as_ref(), pack).await {
                            Ok(()) => Ok(Some(data.len() as u64)),
                            Err(e) => {
                                debug!("pack upload failed, spooling: {e}");
                                spool_pack(&spool, id, &data)?;
                                Ok(None)
                            }
                        }
                    }
                })
                .buffer_unordered(UPLOAD_CONCURRENCY)
                .collect()
                .await;
            let mut up = Uploaded::default();
            for r in results {
                match r? {
                    Some(n) => {
                        up.packs += 1;
                        up.bytes += n;
                    }
                    None => up.spooled += 1,
                }
            }
            Ok::<_, Error>(up)
        });

        let builder = self.builder.clone();
        let lazy_root = self.lazy.then(|| self.root.clone());
        let reader = tokio::task::spawn_blocking(move || {
            let out: Vec<Result<Option<Content>>> = reads
                .par_iter()
                .map(|(path, kind)| read_one(path, *kind, &builder, &tx, lazy_root.as_deref()))
                .collect();
            if let Some(last) = builder.lock().unwrap().seal() {
                let _ = tx.blocking_send(last);
            }
            out
        });
        let contents = reader.await.expect("reader task panicked");
        let uploaded = uploader.await.expect("uploader task panicked")?;
        let contents = contents.into_iter().collect::<Result<Vec<_>>>()?;
        Ok((contents, uploaded))
    }

    /// Uploads packs spooled while offline. Returns how many.
    pub async fn flush_spool(&self) -> Result<usize> {
        let dir = self.spool_dir();
        let rd = match fs::read_dir(&dir) {
            Ok(rd) => rd,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
            Err(e) => return Err(e).at(&dir),
        };
        let mut n = 0;
        for ent in rd {
            let path = ent.at(&dir)?.path();
            let Some(id) = path
                .file_name()
                .and_then(|n| n.to_str())
                .and_then(|n| u128::from_str_radix(n, 16).ok())
            else {
                continue; // a partial write (".tmp")
            };
            let data = Bytes::from(fs::read(&path).at(&path)?);
            rustfuse_chunk::upload_pack(self.store().as_ref(), SealedPack { id: PackId(id), data }).await?;
            fs::remove_file(&path).at(&path)?;
            n += 1;
        }
        Ok(n)
    }

    // ---- talking to the hub --------------------------------------------------

    async fn connect(&self, hub: &HubSpec) -> Result<HubClient> {
        Ok(HubClient::connect_with(
            &hub.addr,
            hub.tls.as_ref(),
            &self.config.volume,
            &hub.token,
            self.counters.applied,
        )
        .await?)
    }

    /// Makes sure every pack the unsent changesets point into is still in
    /// the bucket and not about to be deleted by cleanup ([`crate::gc`]);
    /// content in packs that are gone is uploaded again from the files.
    async fn check_outbox_content(&mut self, sent: &HashSet<u64>) -> Result<()> {
        // Compacted packs forward to new ones, which are checked in turn.
        for _ in 0..8 {
            let missing = self.unavailable_packs(sent).await?;
            if missing.is_empty() {
                return Ok(());
            }
            if !self.follow_moves(&missing).await? {
                warn!(
                    packs = missing.len(),
                    "content of unsent changes was removed by cleanup; uploading it again"
                );
                return self.repair_outbox(&missing).await;
            }
        }
        Ok(())
    }

    /// Packs unsent changesets use that are gone or marked for deletion.
    async fn unavailable_packs(&mut self, sent: &HashSet<u64>) -> Result<HashSet<PackId>> {
        let now = Instant::now();
        let packs: HashSet<PackId> = self
            .outbox
            .iter()
            .filter(|cs| !sent.contains(&cs.local))
            .flat_map(|cs| content_chunks(&cs.ops))
            .map(|r| r.pack)
            .filter(|p| {
                self.checked_packs
                    .get(p)
                    .is_none_or(|t| now.duration_since(*t) > PACK_CHECK_TTL)
            })
            .collect();
        if packs.is_empty() {
            return Ok(HashSet::new());
        }
        let store = self.store().clone();
        let found: Vec<(PackId, bool)> = futures::stream::iter(packs)
            .map(|p| {
                let store = store.clone();
                async move {
                    let doomed = store.head(&crate::gc::tombstone_key(p)).await?.is_some();
                    let ok = !doomed && store.head(&p.object_key()).await?.is_some();
                    Ok::<_, Error>((p, ok))
                }
            })
            .buffer_unordered(PACK_CHECK_CONCURRENCY)
            .try_collect()
            .await?;
        let mut missing = HashSet::new();
        for (p, ok) in found {
            if ok {
                self.checked_packs.insert(p, now);
            } else {
                missing.insert(p);
            }
        }
        Ok(missing)
    }

    /// Points references into compacted `packs` at where their chunks moved
    /// (`rustfuse_chunk::Forward`), in unsent changesets, the disk tree and
    /// the known chunks. Returns whether anything moved.
    async fn follow_moves(&mut self, packs: &HashSet<PackId>) -> Result<bool> {
        let mut forwards = HashMap::new();
        for p in packs {
            if let Some(f) = rustfuse_chunk::load_forward(self.store().as_ref(), *p).await? {
                forwards.insert(*p, f);
            }
        }
        let remap = |content: &mut Content| -> bool {
            let Content::File { chunks, .. } = content else {
                return false;
            };
            let mut moved = false;
            for r in chunks.iter_mut() {
                if let Some(n) = forwards.get(&r.pack).and_then(|f| f.get(&r.offset))
                    && n.hash == r.hash
                {
                    *r = *n;
                    moved = true;
                }
            }
            moved
        };
        let mut changed = Vec::new();
        for cs in self.outbox.iter_mut() {
            let mut touched = false;
            for op in cs.ops.iter_mut() {
                if let Op::Upsert { entry, .. } = op {
                    touched |= remap(&mut entry.content);
                }
            }
            if touched {
                changed.push(cs.local);
            }
        }
        let mut disk_ids = Vec::new();
        let ids: Vec<NodeId> = self.tree.iter().map(|(id, _)| *id).collect();
        for id in ids {
            let mut content = self.tree.get(id).map(|e| e.content.clone());
            if let Some(c) = content.as_mut()
                && remap(c)
            {
                self.tree.set_content(id, content.unwrap());
                disk_ids.push(id);
            }
        }
        let moved: Vec<ChunkRef> = forwards.values().flat_map(|f| f.values().copied()).collect();
        // Chunks of those packs that did not move are gone: never reuse them.
        let forgotten = {
            let mut b = self.builder.lock().unwrap();
            let forgotten = b.forget_packs(&forwards.keys().copied().collect());
            for r in &moved {
                b.insert_known(*r);
            }
            forgotten
        };
        if changed.is_empty() && disk_ids.is_empty() {
            self.index.commit(Commit {
                chunks: moved,
                chunks_remove: forgotten,
                ..Commit::default()
            })?;
            return Ok(false);
        }
        debug!(
            changesets = changed.len(),
            files = disk_ids.len(),
            "followed compacted packs"
        );
        self.index.commit(Commit {
            disk: Some((&self.tree, disk_ids)),
            outbox_put: self
                .outbox
                .iter()
                .filter(|cs| changed.contains(&cs.local))
                .collect(),
            chunks: moved,
            chunks_remove: forgotten,
            ..Commit::default()
        })?;
        Ok(true)
    }

    /// Re-uploads the content of unsent changesets that lives in `missing`
    /// packs, reading it from the files, and points the changesets at the
    /// new copies.
    async fn repair_outbox(&mut self, missing: &HashSet<PackId>) -> Result<()> {
        let forgotten = self.builder.lock().unwrap().forget_packs(missing);
        self.index.commit(Commit {
            chunks_remove: forgotten,
            ..Commit::default()
        })?;
        // The disk may have moved on; capture first so the disk tree says
        // what the files hold.
        self.capture(None).await?;

        let in_missing = |c: &Content| match c {
            Content::File { chunks, .. } => chunks.iter().any(|r| missing.contains(&r.pack)),
            _ => false,
        };
        let mut nodes = HashSet::new();
        for cs in &self.outbox {
            for op in &cs.ops {
                if let Op::Upsert { node, entry, .. } = op
                    && in_missing(&entry.content)
                {
                    nodes.insert(*node);
                }
            }
        }
        for (id, n) in self.tree.iter() {
            if in_missing(&n.entry.content) {
                nodes.insert(*id);
            }
        }
        // Reading the files again stores every chunk no longer known.
        let mut reads = Vec::new();
        let mut read_ids = Vec::new();
        for id in &nodes {
            if let Some(path) = self.tree.path(*id)
                && matches!(self.tree.get(*id).map(|e| &e.content), Some(Content::File { .. }))
            {
                reads.push((self.root.join(path), Kind::File));
                read_ids.push(*id);
            }
        }
        let (contents, _) = self.read_contents(reads).await?;
        let mut fresh: HashMap<rustfuse_types::ChunkHash, ChunkRef> = HashMap::new();
        for c in contents.iter().flatten() {
            if let Content::File { chunks, .. } = c {
                fresh.extend(chunks.iter().map(|r| (r.hash, *r)));
            }
        }
        let fix = |content: &mut Content| -> bool {
            let Content::File { chunks, .. } = content else {
                return true;
            };
            let mut all = true;
            for r in chunks.iter_mut().filter(|r| missing.contains(&r.pack)) {
                match fresh.get(&r.hash) {
                    Some(f) => *r = *f,
                    None => all = false,
                }
            }
            all
        };

        // The disk tree first: its content is what the files hold now.
        let mut disk_ids = Vec::new();
        for id in nodes.iter().copied() {
            if let Some(mut n) = self.tree.node(id).cloned()
                && in_missing(&n.entry.content)
            {
                if !fix(&mut n.entry.content) {
                    warn!(?id, "could not recover content; the file will be re-read");
                }
                self.tree.set_content(id, n.entry.content);
                disk_ids.push(id);
            }
        }
        // Changesets: an older version of a file that no longer exists
        // anywhere becomes the current one (the end result is the same).
        let mut changed = Vec::new();
        for cs in self.outbox.iter_mut() {
            let mut touched = false;
            for op in cs.ops.iter_mut() {
                if let Op::Upsert { node, entry, .. } = op
                    && in_missing(&entry.content)
                {
                    touched = true;
                    if !fix(&mut entry.content) {
                        entry.content = match self.tree.get(*node).map(|e| &e.content) {
                            Some(c @ Content::File { .. }) if !in_missing(c) => c.clone(),
                            _ => Content::File {
                                size: 0,
                                chunks: Vec::new(),
                            },
                        };
                        warn!(node = ?node, "an unsent older version was lost to cleanup");
                    }
                }
            }
            if touched {
                changed.push(cs.local);
            }
        }
        let new_chunks: Vec<ChunkRef> = fresh.into_values().collect();
        self.index.commit(Commit {
            disk: Some((&self.tree, disk_ids)),
            outbox_put: self
                .outbox
                .iter()
                .filter(|cs| changed.contains(&cs.local))
                .collect(),
            chunks: new_chunks,
            ..Commit::default()
        })?;
        info!(
            changesets = changed.len(),
            files = read_ids.len(),
            "re-uploaded removed content"
        );
        Ok(())
    }

    /// Sends outbox changesets not already sent on this connection.
    async fn push_outbox(&mut self, conn: &mut HubClient, sent: &mut HashSet<u64>) -> Result<usize> {
        if !self.outbox.iter().any(|cs| !sent.contains(&cs.local)) {
            return Ok(0);
        }
        // Content must be in the bucket before anyone can see the changes.
        self.flush_spool().await?;
        self.check_outbox_content(sent).await?;
        let mut n = 0;
        for cs in &self.outbox {
            if sent.insert(cs.local) {
                conn.push(cs).await?;
                n += 1;
            }
        }
        self.metrics.pushed_total.add(n as u64);
        Ok(n)
    }

    /// One round: captures local changes, sends them, and applies the log
    /// until this machine is up to date and its changes are accepted.
    pub async fn sync_once(&mut self, hub: &HubSpec, timeout: Duration) -> Result<SyncStats> {
        let mut stats = SyncStats {
            captured: self.capture(None).await?,
            ..SyncStats::default()
        };
        self.reconcile_with_capture().await?;
        let mut conn = self.connect(hub).await?;
        let head = conn.head();
        self.metrics.hub_head.max(head as i64);
        let mut sent = HashSet::new();
        stats.pushed += self.push_outbox(&mut conn, &mut sent).await?;
        while self.counters.applied < head || !self.outbox.is_empty() {
            let first = match tokio::time::timeout(timeout, conn.next()).await {
                Err(_) => return Err(Error::Timeout),
                Ok(None) => return Err(Error::Disconnected),
                Ok(Some(ev)) => ev?,
            };
            let mut batch = Vec::new();
            let mut next = Some(Ok(first));
            while let Some(ev) = next {
                if let Event::Changeset(cs) = ev? {
                    batch.push(cs);
                }
                next = conn.try_next();
            }
            stats.received += batch.len();
            self.receive_upto(batch, head).await?;
            stats.pushed += self.push_outbox(&mut conn, &mut sent).await?;
        }
        if self.tidy_git().await? {
            // Fixes go out like any other change.
            stats.pushed += self.push_outbox(&mut conn, &mut sent).await?;
            while !self.outbox.is_empty() {
                let batch = match tokio::time::timeout(timeout, conn.next()).await {
                    Err(_) => return Err(Error::Timeout),
                    Ok(None) => return Err(Error::Disconnected),
                    Ok(Some(ev)) => match ev? {
                        Event::Changeset(cs) => vec![cs],
                        Event::Pushed { .. } => continue,
                    },
                };
                stats.received += batch.len();
                self.receive_upto(batch, head).await?;
                stats.pushed += self.push_outbox(&mut conn, &mut sent).await?;
            }
        }
        Ok(stats)
    }

    /// Runs the git-aware pass ([`crate::git`]) and captures what it
    /// changed. Returns whether it changed anything.
    pub async fn tidy_git(&mut self) -> Result<bool> {
        let plan = Plan::from_tree(&self.tree);
        if plan.is_empty() {
            return Ok(false);
        }
        let Some(mut git) = self.git.take() else {
            return Ok(false);
        };
        let (git, changed) = tokio::task::spawn_blocking(move || {
            let changed = git.run(plan);
            (git, changed)
        })
        .await
        .expect("git pass panicked");
        self.git = Some(git);
        if changed {
            self.capture(None).await?;
        }
        Ok(changed)
    }

    // ---- continuous mode -----------------------------------------------------

    /// Keeps the directory in sync until the task is cancelled: captures
    /// local changes as they happen, sends them when the hub is reachable,
    /// and applies everyone's changes as they arrive. Works offline and
    /// catches up on reconnect.
    pub async fn run(&mut self, hub: &HubSpec, debounce: Duration, full_scan_every: Duration) -> Result<()> {
        self.run_serving(hub, debounce, full_scan_every, None).await
    }

    /// [`Replica::run`], also answering File Provider requests from
    /// `calls` ([`crate::provider`]) between sync steps.
    pub async fn run_serving(
        &mut self,
        hub: &HubSpec,
        debounce: Duration,
        full_scan_every: Duration,
        mut calls: Option<tokio::sync::mpsc::Receiver<crate::provider::Call>>,
    ) -> Result<()> {
        use notify::{RecursiveMode, Watcher};
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let journal = crate::fsmonitor::Journal::new();
        let (j, root, disk_watch) = (journal.clone(), self.root.clone(), self.disk_watch.clone());
        let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
            j.record(&root, &res);
            if let Some(f) = &disk_watch {
                match &res {
                    Ok(ev) if ev.need_rescan() => f(None),
                    Ok(ev) if matches!(ev.kind, notify::EventKind::Access(_)) => {}
                    Ok(ev) => {
                        let rel: Vec<PathBuf> = ev
                            .paths
                            .iter()
                            .filter_map(|p| p.strip_prefix(&root).ok())
                            .filter(|p| !p.starts_with(STATE_DIR))
                            .map(Path::to_path_buf)
                            .collect();
                        if !rel.is_empty() {
                            f(Some(&rel));
                        }
                    }
                    Err(_) => f(None),
                }
            }
            let _ = tx.send(res);
        })
        .map_err(|e| Error::Corrupt(format!("cannot watch: {e}")))?;
        watcher
            .watch(&self.root, RecursiveMode::Recursive)
            .map_err(|e| Error::Corrupt(format!("cannot watch {}: {e}", self.root.display())))?;

        let _fsmonitor = AbortOnDrop(tokio::spawn(crate::fsmonitor::serve(self.root.clone(), journal)));

        report(self.capture(None).await?);
        self.reconcile_with_capture().await?;
        let mut full = tokio::time::interval(full_scan_every);
        full.tick().await;

        let mut conn: Option<HubClient> = None;
        // Where the log ended when we connected; see `receive_upto`.
        let mut catch_up = 0;
        let mut sent = HashSet::new();
        let mut backoff = Duration::from_millis(500);
        let mut next_attempt = Instant::now();
        let mut offline_reported = false;
        loop {
            if conn.is_none() && Instant::now() >= next_attempt {
                match self.connect(hub).await {
                    Ok(c) => {
                        info!(
                            head = c.head(),
                            applied = self.counters.applied,
                            "connected to hub"
                        );
                        catch_up = c.head();
                        self.metrics.hub_head.max(catch_up as i64);
                        self.metrics.connected.set(1);
                        conn = Some(c);
                        sent.clear();
                        backoff = Duration::from_millis(500);
                        offline_reported = false;
                    }
                    Err(e) => {
                        self.metrics.errors_total.get("connect").inc();
                        if !offline_reported {
                            warn!("hub unreachable, working offline: {e}");
                            offline_reported = true;
                        }
                        next_attempt = Instant::now() + backoff;
                        backoff = (backoff * 2).min(Duration::from_secs(30));
                    }
                }
            }
            if let Some(c) = conn.as_mut()
                && let Err(e) = self.push_outbox(c, &mut sent).await
            {
                warn!("sending changes failed: {e}");
                self.metrics.errors_total.get("push").inc();
                conn = None;
                self.metrics.connected.set(0);
            }

            let mut dirty: HashSet<PathBuf> = HashSet::new();
            let mut rescan = false;
            let mut batch = Vec::new();
            let mut lost = false;
            tokio::select! {
                _ = full.tick() => rescan = true,
                Some((req, reply)) = next_call(&mut calls) => {
                    let resp = self.provide(req).await;
                    let _ = reply.send(resp);
                }
                ev = rx.recv() => {
                    let Some(ev) = ev else { return Ok(()) };
                    if self.collect_event(ev, &mut dirty, &mut rescan) {
                        // Debounce: collect until no change for `debounce`,
                        // but never hold changes back longer than
                        // MAX_DEBOUNCE while the tree stays busy. Reads do
                        // not count as activity.
                        let start = Instant::now();
                        let mut quiet = start + debounce;
                        loop {
                            let until = quiet.min(start + MAX_DEBOUNCE);
                            let Some(wait) = until.checked_duration_since(Instant::now()) else { break };
                            match tokio::time::timeout(wait, rx.recv()).await {
                                Ok(Some(ev)) => {
                                    if self.collect_event(ev, &mut dirty, &mut rescan) {
                                        quiet = Instant::now() + debounce;
                                    }
                                }
                                _ => break,
                            }
                        }
                    }
                }
                ev = next_event(&mut conn) => {
                    let mut next = Some(ev);
                    while let Some(ev) = next {
                        match ev {
                            Some(Ok(Event::Changeset(cs))) => {
                                self.metrics.hub_head.max(cs.seq as i64);
                                batch.push(cs);
                            }
                            Some(Ok(Event::Pushed { .. })) => {}
                            Some(Err(e)) => { warn!("hub connection lost: {e}"); lost = true; break; }
                            None => { warn!("hub connection closed"); lost = true; break; }
                        }
                        next = conn.as_mut().and_then(|c| c.try_next()).map(Some);
                    }
                }
                _ = tokio::time::sleep_until(tokio::time::Instant::from_std(next_attempt)), if conn.is_none() => {}
            }
            if lost {
                self.metrics.errors_total.get("connect").inc();
                conn = None;
                self.metrics.connected.set(0);
                next_attempt = Instant::now() + backoff;
            }
            // Capture first, so a burst of remote changes never overwrites
            // an edit made a moment ago.
            if rescan || !dirty.is_empty() {
                let paths = (!rescan).then(|| dirty.into_iter().collect());
                match self.capture(paths).await {
                    Ok(s) => {
                        let tidy = s.is_some();
                        report(s);
                        if tidy && let Err(e) = self.tidy_git().await {
                            warn!("git pass failed, will retry: {e}");
                            self.metrics.errors_total.get("git").inc();
                        }
                    }
                    Err(e) => {
                        warn!("capturing changes failed, will retry: {e}");
                        self.metrics.errors_total.get("capture").inc();
                    }
                }
            }
            if !batch.is_empty() {
                let n = batch.len();
                match self.receive_upto(batch, catch_up).await {
                    Ok(()) => {
                        debug!(changesets = n, applied = self.counters.applied, "applied");
                        if let Err(e) = self.tidy_git().await {
                            warn!("git pass failed, will retry: {e}");
                            self.metrics.errors_total.get("git").inc();
                        }
                    }
                    Err(e @ (Error::Index(_) | Error::Corrupt(_))) => return Err(e),
                    Err(e) => {
                        // Reconnecting replays from the last applied changeset.
                        warn!("applying changes failed, will retry: {e}");
                        self.metrics.errors_total.get("apply").inc();
                        conn = None;
                        self.metrics.connected.set(0);
                        next_attempt = Instant::now() + backoff;
                    }
                }
            }
        }
    }

    /// Records what an event changed. Returns whether it changed anything
    /// that syncs.
    fn collect_event(
        &self,
        ev: notify::Result<notify::Event>,
        dirty: &mut HashSet<PathBuf>,
        rescan: &mut bool,
    ) -> bool {
        let ev = match ev {
            Ok(ev) => ev,
            Err(e) => {
                warn!("watch error, rescanning: {e}");
                *rescan = true;
                return true;
            }
        };
        if ev.need_rescan() {
            *rescan = true;
            return true;
        }
        // Reads (including our own scans) change nothing; reacting to them
        // would make every scan trigger another.
        use notify::EventKind;
        use notify::event::{AccessKind, AccessMode};
        match ev.kind {
            EventKind::Access(AccessKind::Close(AccessMode::Write)) => {}
            EventKind::Access(_) => return false,
            _ => {}
        }
        let mut relevant = false;
        for p in ev.paths {
            let Ok(rel) = p.strip_prefix(&self.root) else {
                continue;
            };
            if rel.starts_with(STATE_DIR) {
                continue;
            }
            // Changed ignore rules can reveal or hide whole subtrees.
            let name = rel.file_name().and_then(|n| n.to_str());
            if matches!(name, Some(".gitignore" | crate::ignore::IGNORE_FILE))
                || rel.ends_with(".git/info/exclude")
            {
                self.ignore.invalidate();
                *rescan = true;
                relevant = true;
            }
            if self.ignore.is_ignored(rel, false) {
                continue;
            }
            dirty.insert(rel.to_path_buf());
            relevant = true;
        }
        relevant
    }
}

/// The next File Provider request; never ready without a channel.
async fn next_call(
    calls: &mut Option<tokio::sync::mpsc::Receiver<crate::provider::Call>>,
) -> Option<crate::provider::Call> {
    match calls {
        Some(rx) => rx.recv().await,
        None => std::future::pending().await,
    }
}

struct AbortOnDrop(tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

async fn next_event(conn: &mut Option<HubClient>) -> Option<rustfuse_hub::Result<Event>> {
    match conn {
        Some(c) => c.next().await,
        None => std::future::pending().await,
    }
}

fn report(stats: Option<CaptureStats>) {
    if let Some(s) = stats {
        info!(
            ops = s.ops,
            files = s.files_read,
            uploaded = s.packs,
            spooled = s.packs_spooled,
            "captured local changes"
        );
    }
}

/// Which parts of a node differ between what the tree knows and what is on
/// disk now.
fn changed_fields(old: &Entry, new: &Entry) -> Fields {
    let mut f = Fields::default();
    if old.parent != new.parent || old.name != new.name {
        f |= Fields::LOC;
    }
    // A directory's mtime moves whenever its children change; it is not
    // synced as a change of its own.
    if !old.content.same_data(&new.content) || (!new.content.is_dir() && old.mtime != new.mtime) {
        f |= Fields::DATA;
    }
    if old.mode != new.mode {
        f |= Fields::META;
    }
    f
}

#[derive(Default)]
struct Uploaded {
    packs: usize,
    bytes: u64,
    spooled: usize,
}

fn spool_pack(dir: &Path, id: PackId, data: &[u8]) -> Result<()> {
    fs::create_dir_all(dir).at(dir)?;
    let name = format!("{:032x}", id.0);
    let tmp = dir.join(format!("{name}.tmp"));
    fs::write(&tmp, data).at(&tmp)?;
    let path = dir.join(name);
    fs::rename(&tmp, &path).at(&path)?;
    Ok(())
}

fn tokio_stream_from<T: Send + 'static>(
    mut rx: tokio::sync::mpsc::Receiver<T>,
) -> impl futures::Stream<Item = T> + Send {
    futures::stream::poll_fn(move |cx| rx.poll_recv(cx))
}

/// Reads one file or symlink into a [`Content`], chunking large files.
/// Reads and chunks one file. In a lazy directory (`lazy_root`), a
/// placeholder counts as holding the content it stands for.
fn read_one(
    path: &Path,
    kind: Kind,
    builder: &Mutex<PackBuilder>,
    tx: &tokio::sync::mpsc::Sender<SealedPack>,
    lazy_root: Option<&Path>,
) -> Result<Option<Content>> {
    let not_found = |e: &std::io::Error| e.kind() == std::io::ErrorKind::NotFound;
    if kind == Kind::Symlink {
        return match fs::read_link(path) {
            Ok(t) => Ok(Some(Content::Symlink(
                crate::tree::name_bytes(t.as_os_str()).to_vec(),
            ))),
            Err(e) if not_found(&e) => Ok(None),
            Err(e) => Err(e).at(path),
        };
    }
    let file = match fs::File::open(path) {
        Ok(f) => f,
        Err(e) if not_found(&e) => return Ok(None),
        Err(e) => return Err(e).at(path),
    };
    if let Some(root) = lazy_root
        && let Some(content) = crate::lazy::placeholder_content(root, &file)?
    {
        return Ok(Some(content));
    }
    let len = file.metadata().at(path)?.len();
    if len <= INLINE_MAX {
        let mut data = Vec::with_capacity(len as usize);
        file.take(INLINE_MAX + 1).read_to_end(&mut data).at(path)?;
        if data.len() as u64 <= INLINE_MAX {
            return Ok(Some(Content::Inline(data)));
        }
        // Grew while we looked; fall through and chunk it.
        return read_one(path, kind, builder, tx, lazy_root);
    }
    let mut chunks = Vec::new();
    let mut size = 0u64;
    rustfuse_chunk::chunk_reader(BufReader::with_capacity(1 << 20, file), |data| {
        let hash = rustfuse_chunk::hash(data);
        size += data.len() as u64;
        let known = builder.lock().unwrap().lookup(&hash);
        let r = match known {
            Some(r) => r,
            None => {
                let prepared = rustfuse_chunk::prepare(hash, data);
                let (r, sealed) = builder.lock().unwrap().add(prepared);
                if let Some(pack) = sealed {
                    tx.blocking_send(pack)
                        .map_err(|_| std::io::Error::other("uploader stopped"))?;
                }
                r
            }
        };
        chunks.push(r);
        Ok(())
    })
    .map_err(|e| match e {
        rustfuse_chunk::Error::Io(source) => Error::Io {
            path: path.to_owned(),
            source,
        },
        other => other.into(),
    })?;
    Ok(Some(Content::File { size, chunks }))
}
