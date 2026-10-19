//! One synced directory: ties the index, scanner, chunker, applier and the
//! volume log together.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{BufReader, Read};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::StreamExt;
use rayon::prelude::*;
use rustfuse_chunk::{PackBuilder, SealedPack};
use rustfuse_store::ObjectStore;
use rustfuse_types::{
    Changeset, ChunkRef, Content, Entry, FORMAT_VERSION, INLINE_MAX, NodeId, Op, Snapshot, Timestamp,
};
use tracing::{debug, info, warn};

use crate::apply::Applier;
use crate::ignore::Ignore;
use crate::index::{Commit, Config, Index, Role, StatKey};
use crate::remote::Volume;
use crate::scan::{Change, Kind, ScanRoot, Scanner};
use crate::tree::{NodeChange, Tree};
use crate::{Error, IoContext, Result, STATE_DIR};

/// A snapshot is written every this many changesets so new machines do not
/// replay the whole log.
const SNAPSHOT_EVERY: u64 = 256;
const UPLOAD_CONCURRENCY: usize = 8;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PushStats {
    pub seq: u64,
    pub ops: usize,
    pub files_read: usize,
    pub bytes_read: u64,
    pub packs: usize,
    pub bytes_uploaded: u64,
}

pub struct Replica {
    root: PathBuf,
    config: Config,
    index: Index,
    tree: Tree,
    stats: HashMap<NodeId, StatKey>,
    applied: u64,
    counter: u64,
    volume: Volume,
    builder: Arc<Mutex<PackBuilder>>,
    ignore: Ignore,
}

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
        Ok(Index::open(&path)?.state()?.config)
    }

    /// Creates a new volume from the contents of `root` and uploads it.
    pub async fn init(root: &Path, config: Config, store: Arc<dyn ObjectStore>) -> Result<(Self, PushStats)> {
        if root.join(STATE_DIR).exists() {
            return Err(Error::AlreadyInitialized(root.to_owned()));
        }
        let volume = Volume::new(store.clone());
        volume.create().await?;
        create_state_dir(root)?;
        let index = Index::create(&index_path(root), &config)?;
        let mut r = Replica::load(root, index, store)?;
        let stats = r.push(None).await?.unwrap_or_default();
        Ok((r, stats))
    }

    /// Materializes an existing volume into the empty directory `root`.
    pub async fn attach(root: &Path, config: Config, store: Arc<dyn ObjectStore>) -> Result<(Self, usize)> {
        let volume = Volume::new(store.clone());
        volume.check().await?;
        if root.exists() && fs::read_dir(root).at(root)?.next().is_some() {
            return Err(Error::NotEmpty(root.to_owned()));
        }
        create_state_dir(root)?;
        let index = Index::create(&index_path(root), &config)?;
        let mut r = Replica::load(root, index, store)?;
        let n = r.pull().await?;
        Ok((r, n))
    }

    pub fn open(root: &Path, store: Arc<dyn ObjectStore>) -> Result<Self> {
        let path = index_path(root);
        if !path.exists() {
            return Err(Error::NotInitialized(root.to_owned()));
        }
        Replica::load(root, Index::open(&path)?, store)
    }

    fn load(root: &Path, index: Index, store: Arc<dyn ObjectStore>) -> Result<Self> {
        let state = index.state()?;
        let mut builder = PackBuilder::new(state.config.seed);
        for r in index.chunks()? {
            builder.insert_known(r);
        }
        // Watch events carry absolute, resolved paths.
        let root = root.canonicalize().at(root)?;
        Ok(Replica {
            ignore: Ignore::load(&root).map_err(|e| Error::Corrupt(format!("ignore file: {e}")))?,
            root,
            tree: index.tree()?,
            stats: index.stats()?,
            applied: state.applied,
            counter: state.counter,
            config: state.config,
            index,
            volume: Volume::new(store),
            builder: Arc::new(Mutex::new(builder)),
        })
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    pub fn applied(&self) -> u64 {
        self.applied
    }

    pub fn store(&self) -> &Arc<dyn ObjectStore> {
        self.volume.store()
    }

    pub fn tree(&self) -> &Tree {
        &self.tree
    }

    // ---- pull --------------------------------------------------------------

    /// Applies changesets written since the last pull. Returns how many.
    pub async fn pull(&mut self) -> Result<usize> {
        let mut applied = 0;
        if self.applied == 0
            && self.tree.is_empty()
            && let Some(snap) = self.volume.latest_snapshot().await?
        {
            info!(seq = snap.seq, nodes = snap.nodes.len(), "restoring snapshot");
            self.apply_snapshot(snap).await?;
            applied += 1;
        }
        for seq in self.volume.changesets_after(self.applied).await? {
            let cs = self.volume.get_changeset(seq).await?;
            debug!(seq, ops = cs.ops.len(), "applying changeset");
            let own = cs.replica == self.config.replica;
            self.apply_ops(seq, &cs.ops, !own).await?;
            applied += 1;
        }
        Ok(applied)
    }

    async fn apply_snapshot(&mut self, snap: Snapshot) -> Result<()> {
        // Replay the snapshot as upserts, parents first.
        let staged = Tree::from_snapshot(&snap);
        let mut nodes: Vec<(usize, Op)> = snap
            .nodes
            .into_iter()
            .map(|(node, entry)| {
                let depth = staged.path(node).map_or(0, |p| p.components().count());
                (depth, Op::Upsert { node, entry })
            })
            .collect();
        nodes.sort_by_key(|(d, _)| *d);
        let ops: Vec<Op> = nodes.into_iter().map(|(_, op)| op).collect();
        self.apply_ops(snap.seq, &ops, true).await
    }

    async fn apply_ops(&mut self, seq: u64, ops: &[Op], write_disk: bool) -> Result<()> {
        let changes = self.tree.apply(ops);
        for c in &changes {
            if (c.node.0 >> 64) as u64 == self.config.replica.0 {
                self.counter = self.counter.max(c.node.0 as u64 + 1);
            }
        }
        let stats = if write_disk {
            let applier = Applier {
                root: &self.root,
                store: self.volume.store().clone(),
            };
            applier.apply(&changes).await?
        } else {
            Vec::new()
        };
        self.commit(seq, &changes, stats, content_chunks(ops))
    }

    fn commit(
        &mut self,
        seq: u64,
        changes: &[NodeChange],
        stats: Vec<(NodeId, Option<StatKey>)>,
        chunks: Vec<ChunkRef>,
    ) -> Result<()> {
        for c in changes.iter().filter(|c| c.new.is_none()) {
            self.stats.remove(&c.node);
        }
        for (id, st) in &stats {
            match st {
                Some(st) => self.stats.insert(*id, *st),
                None => self.stats.remove(id),
            };
        }
        {
            let mut b = self.builder.lock().unwrap();
            for r in &chunks {
                b.insert_known(*r);
            }
        }
        self.index.commit(Commit {
            applied: seq,
            counter: self.counter,
            changes,
            stats,
            chunks,
        })?;
        self.applied = seq;
        Ok(())
    }

    // ---- push --------------------------------------------------------------

    /// Changes on disk not yet pushed, without reading content.
    pub fn pending(&self) -> Result<Vec<Change>> {
        let mut counter = self.counter;
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

    /// Scans for local changes (everything, or only the given directories),
    /// uploads new content and appends a changeset. Returns `None` when
    /// nothing changed.
    pub async fn push(&mut self, dirs: Option<Vec<PathBuf>>) -> Result<Option<PushStats>> {
        if self.config.role != Role::Writer {
            return Err(Error::NotWriter);
        }
        // Recover changesets we wrote but did not record (crash after
        // upload); anyone else's means a second writer.
        for seq in self.volume.changesets_after(self.applied).await? {
            let cs = self.volume.get_changeset(seq).await?;
            if cs.replica != self.config.replica {
                return Err(Error::ForeignWriter(seq));
            }
            self.apply_ops(seq, &cs.ops, false).await?;
        }

        let roots = match dirs {
            None => vec![ScanRoot {
                node: NodeId::ROOT,
                path: PathBuf::new(),
                recursive: true,
            }],
            Some(dirs) => self.scan_roots(dirs),
        };
        debug!(roots = roots.len(), "scanning");
        let mut counter = self.counter;
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
        self.counter = counter;

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
        let mut stats = PushStats {
            files_read: reads.len(),
            ..Default::default()
        };
        let (contents, packs, uploaded) = self.read_contents(reads).await?;
        stats.packs = packs;
        stats.bytes_uploaded = uploaded;

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
                    let content = if read {
                        match contents.next().unwrap() {
                            Some(c) => c,
                            None => continue, // vanished while reading; next scan sees it
                        }
                    } else if found.kind == Kind::Dir {
                        Content::Dir
                    } else {
                        self.tree
                            .get(node)
                            .map(|e| e.content.clone())
                            .unwrap_or(Content::Dir)
                    };
                    if read {
                        stats.bytes_read += match &content {
                            Content::Inline(d) => d.len() as u64,
                            Content::File { size, .. } => *size,
                            _ => 0,
                        };
                    }
                    new_stats.push((node, Some(found.stat)));
                    ops.push(Op::Upsert {
                        node,
                        entry: Entry {
                            parent,
                            name,
                            mode: found.stat.mode & 0o777,
                            mtime: found.mtime,
                            content,
                        },
                    });
                }
                Change::Remove { node, .. } => {
                    new_stats.push((node, None));
                    ops.push(Op::Remove { node });
                }
            }
        }
        if ops.is_empty() {
            return Ok(None);
        }

        let seq = self.applied + 1;
        let cs = Changeset {
            format: FORMAT_VERSION,
            seq,
            replica: self.config.replica,
            time: Timestamp::now(),
            ops,
        };
        self.volume.put_changeset(&cs).await?;
        let tree_changes = self.tree.apply(&cs.ops);
        if seq == 1 || seq.is_multiple_of(SNAPSHOT_EVERY) {
            self.volume.put_snapshot(&self.tree.snapshot(seq)).await?;
        }
        self.commit(seq, &tree_changes, new_stats, content_chunks(&cs.ops))?;
        stats.seq = seq;
        stats.ops = cs.ops.len();
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

    /// Reads and chunks files in parallel, uploading packs as they fill.
    /// Returns one content per input (`None` if the file vanished), the
    /// number of packs and bytes uploaded.
    async fn read_contents(&self, reads: Vec<(PathBuf, Kind)>) -> Result<(Vec<Option<Content>>, usize, u64)> {
        let (tx, rx) = tokio::sync::mpsc::channel::<SealedPack>(4);
        let store = self.volume.store().clone();
        let uploader = tokio::spawn(async move {
            let results: Vec<Result<u64>> = tokio_stream_from(rx)
                .map(|pack| {
                    let store = store.clone();
                    async move {
                        let n = pack.data.len() as u64;
                        rustfuse_chunk::upload_pack(store.as_ref(), pack).await?;
                        Ok(n)
                    }
                })
                .buffer_unordered(UPLOAD_CONCURRENCY)
                .collect()
                .await;
            let mut count = 0;
            let mut bytes = 0;
            for r in results {
                bytes += r?;
                count += 1;
            }
            Ok::<_, Error>((count, bytes))
        });

        let builder = self.builder.clone();
        let reader = tokio::task::spawn_blocking(move || {
            let out: Vec<Result<Option<Content>>> = reads
                .par_iter()
                .map(|(path, kind)| read_one(path, *kind, &builder, &tx))
                .collect();
            if let Some(last) = builder.lock().unwrap().seal() {
                let _ = tx.blocking_send(last);
            }
            out
        });
        let contents = reader.await.expect("reader task panicked");
        let (packs, bytes) = uploader.await.expect("uploader task panicked")?;
        let contents = contents.into_iter().collect::<Result<Vec<_>>>()?;
        Ok((contents, packs, bytes))
    }

    // ---- continuous modes --------------------------------------------------

    /// Pushes local changes as they happen until the task is cancelled.
    pub async fn watch_push(&mut self, debounce: Duration, full_scan_every: Duration) -> Result<()> {
        use notify::{RecursiveMode, Watcher};
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut watcher = notify::recommended_watcher(move |res| {
            let _ = tx.send(res);
        })
        .map_err(|e| Error::Corrupt(format!("cannot watch: {e}")))?;
        watcher
            .watch(&self.root, RecursiveMode::Recursive)
            .map_err(|e| Error::Corrupt(format!("cannot watch {}: {e}", self.root.display())))?;

        let first = self.push(None).await?;
        self.report(first);
        let mut full = tokio::time::interval(full_scan_every);
        full.tick().await;
        loop {
            let mut dirty: HashSet<PathBuf> = HashSet::new();
            let mut rescan = false;
            tokio::select! {
                _ = full.tick() => rescan = true,
                ev = rx.recv() => {
                    let Some(ev) = ev else { return Ok(()) };
                    rescan |= self.collect_event(ev, &mut dirty);
                    // Debounce: keep collecting until the tree is quiet.
                    while let Ok(Some(ev)) = tokio::time::timeout(debounce, rx.recv()).await {
                        rescan |= self.collect_event(ev, &mut dirty);
                    }
                }
            }
            if !rescan && dirty.is_empty() {
                continue;
            }
            let dirs = (!rescan).then(|| dirty.into_iter().collect());
            match self.push(dirs).await {
                Ok(stats) => Self::report_stats(stats),
                Err(e @ (Error::ForeignWriter(_) | Error::NotWriter)) => return Err(e),
                Err(e) => warn!("push failed, will retry on next change: {e}"),
            }
        }
    }

    fn collect_event(&self, ev: notify::Result<notify::Event>, dirty: &mut HashSet<PathBuf>) -> bool {
        let ev = match ev {
            Ok(ev) => ev,
            Err(e) => {
                warn!("watch error, rescanning: {e}");
                return true;
            }
        };
        if ev.need_rescan() {
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
        let mut rescan = false;
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
                rescan = true;
            }
            if self.ignore.is_ignored(rel, false) {
                continue;
            }
            dirty.insert(rel.to_path_buf());
        }
        rescan
    }

    fn report(&self, stats: Option<PushStats>) {
        Self::report_stats(stats)
    }

    fn report_stats(stats: Option<PushStats>) {
        if let Some(s) = stats {
            info!(
                seq = s.seq,
                ops = s.ops,
                files = s.files_read,
                uploaded = s.bytes_uploaded,
                "pushed changeset"
            );
        }
    }

    /// Pulls new changesets every `interval` until the task is cancelled.
    pub async fn follow(&mut self, interval: Duration) -> Result<()> {
        loop {
            match self.pull().await {
                Ok(0) => {}
                Ok(n) => info!(applied = self.applied, changesets = n, "pulled"),
                Err(e) => warn!("pull failed, will retry: {e}"),
            }
            tokio::time::sleep(interval).await;
        }
    }
}

fn tokio_stream_from<T: Send + 'static>(
    mut rx: tokio::sync::mpsc::Receiver<T>,
) -> impl futures::Stream<Item = T> + Send {
    futures::stream::poll_fn(move |cx| rx.poll_recv(cx))
}

/// Reads one file or symlink into a [`Content`], chunking large files.
fn read_one(
    path: &Path,
    kind: Kind,
    builder: &Mutex<PackBuilder>,
    tx: &tokio::sync::mpsc::Sender<SealedPack>,
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
    let len = file.metadata().at(path)?.len();
    if len <= INLINE_MAX {
        let mut data = Vec::with_capacity(len as usize);
        file.take(INLINE_MAX + 1).read_to_end(&mut data).at(path)?;
        if data.len() as u64 <= INLINE_MAX {
            return Ok(Some(Content::Inline(data)));
        }
        // Grew while we looked; fall through and chunk it.
        return read_one(path, kind, builder, tx);
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
