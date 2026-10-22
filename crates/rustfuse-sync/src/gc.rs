//! Removing content nobody can need any more (`docs/design.md` §5.5).
//!
//! A pack is deleted only when none of its chunks is used by a file in the
//! volume's current state or by any changeset from the last `retention`,
//! and the pack itself is older than that. Machines may still hold
//! changesets they have not sent yet that point into such a pack (they were
//! offline for longer than `retention`), so deletion takes two runs: the
//! first writes a tombstone `gc/dead/<pack>`, and a later run, at least
//! `delay` after, deletes the pack if it is still unused. Machines check
//! every pack their unsent changesets use right before sending them, and
//! treat a tombstoned pack as already gone: they upload that content again
//! (see `Replica::check_outbox_content`). A changeset checked before a
//! tombstone was written reaches the log well within `delay`, and the run
//! that would delete the pack then sees it and keeps the pack.
//!
//! A pack that is still used but mostly holds dead chunks is compacted: its
//! used chunks are copied into a new pack, a forwarding table
//! `fwd/<pack>` records where each went, and the old pack is marked like an
//! unused one. Readers that find a pack gone follow its forwarding table
//! (`rustfuse_chunk::fetch`), so references written before stay valid.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use futures::{StreamExt, TryStreamExt, stream};
use rustfuse_chunk::Forward;
use rustfuse_store::{ObjectMeta, ObjectStore};
use rustfuse_types::{ChunkRef, Content, Op, PackId};
use tracing::{debug, info};

use crate::tree::Tree;
use crate::{Error, Result, Volume};

/// Snapshots kept; machines only ever read the latest.
const KEEP_SNAPSHOTS: usize = 2;
/// Packs whose used chunks fill less than this share are compacted...
const COMPACT_BELOW: f64 = 0.5;
/// ...when that frees at least this much.
const COMPACT_MIN_GAIN: u64 = 1 << 20;
/// Forwarding hops followed (a pack compacted again).
const MAX_HOPS: usize = 8;
const CONCURRENCY: usize = 16;

pub const TOMBSTONE_PREFIX: &str = "gc/dead/";

/// Key of the tombstone that marks `pack` for deletion.
pub fn tombstone_key(pack: PackId) -> String {
    format!("{TOMBSTONE_PREFIX}{:032x}", pack.0)
}

fn pack_of(key: &str) -> Option<PackId> {
    let hex = key.rsplit('/').next()?;
    (hex.len() == 32).then(|| u128::from_str_radix(hex, 16).ok().map(PackId))?
}

#[derive(Clone, Debug)]
pub struct GcOptions {
    /// Content used within this long stays, and new packs are left alone
    /// for this long.
    pub retention: Duration,
    /// Time between marking a pack and deleting it.
    pub delay: Duration,
    /// Report what would happen without changing anything.
    pub dry_run: bool,
    /// The current time (a parameter so tests can move it).
    pub now: SystemTime,
    /// An encrypted volume's key: cleanup reads the log to see what is
    /// used, and checks chunks before copying them.
    pub cipher: Option<Arc<rustfuse_crypt::Cipher>>,
}

impl Default for GcOptions {
    fn default() -> Self {
        GcOptions {
            retention: Duration::from_secs(14 * 24 * 3600),
            delay: Duration::from_secs(3600),
            dry_run: false,
            now: SystemTime::now(),
            cipher: None,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GcStats {
    pub packs: usize,
    pub pack_bytes: u64,
    /// Packs some live file or recent changeset uses.
    pub used_packs: usize,
    /// Packs newly marked for deletion this run.
    pub marked: usize,
    /// Marked packs that are in use again, unmarked.
    pub revived: usize,
    pub deleted: usize,
    pub deleted_bytes: u64,
    /// Marked packs still waiting for their deletion run.
    pub waiting: usize,
    pub snapshots_deleted: usize,
    /// Packs whose used chunks were copied into new packs this run.
    pub compacted: usize,
    /// Their size, and the size of the packs written instead.
    pub compacted_from: u64,
    pub compacted_to: u64,
    /// Forwarding tables dropped because nothing points at their pack.
    pub forwards_deleted: usize,
}

/// Runs one cleanup pass over the volume `store` is scoped to.
pub async fn collect(store: Arc<dyn ObjectStore>, opts: &GcOptions) -> Result<GcStats> {
    let volume = Volume::new(store.clone()).with_cipher(opts.cipher.clone());
    volume.check().await?;
    if opts.cipher.is_none() && volume.key_info().await?.is_some() {
        return Err(rustfuse_hub::Error::NeedKey.into());
    }
    let cutoff = opts.now.checked_sub(opts.retention).unwrap_or(UNIX_EPOCH);
    let refs = used_refs(&volume, cutoff).await?;
    let forwards = load_forwards(store.as_ref()).await?;
    let live = follow_forwards(&refs, &forwards);
    let used: HashSet<PackId> = live.keys().copied().collect();
    let packs = list(store.as_ref(), "pack/").await?;
    let tombstones = list(store.as_ref(), TOMBSTONE_PREFIX).await?;
    let mut stats = GcStats {
        packs: packs.len(),
        pack_bytes: packs.iter().map(|m| m.size).sum(),
        used_packs: packs
            .iter()
            .filter(|m| pack_of(&m.key).is_some_and(|p| used.contains(&p)))
            .count(),
        ..GcStats::default()
    };
    let sizes: HashMap<PackId, &ObjectMeta> =
        packs.iter().filter_map(|m| Some((pack_of(&m.key)?, m))).collect();

    // Second step for packs marked by an earlier run.
    let mut marked = HashSet::new();
    let mut deletes = Vec::new();
    let mut unmarks = Vec::new();
    for t in &tombstones {
        let Some(pack) = pack_of(&t.key) else { continue };
        marked.insert(pack);
        let written = read_time(store.as_ref(), &t.key).await?;
        if used.contains(&pack) || !sizes.contains_key(&pack) {
            // In use again, or the pack is already gone.
            stats.revived += usize::from(used.contains(&pack));
            unmarks.push(t.key.clone());
        } else if written.is_some_and(|w| opts.now.duration_since(w).is_ok_and(|age| age >= opts.delay)) {
            stats.deleted += 1;
            stats.deleted_bytes += sizes[&pack].size;
            deletes.push((pack.object_key(), t.key.clone()));
        } else {
            stats.waiting += 1;
        }
    }

    // First step: mark unused packs old enough to have been seen by
    // everyone who still needs them.
    let marks: Vec<PackId> = sizes
        .iter()
        .filter(|(p, m)| !used.contains(p) && !marked.contains(p) && m.modified.is_some_and(|t| t < cutoff))
        .map(|(p, _)| *p)
        .collect();
    stats.marked = marks.len();
    stats.waiting += marks.len();

    // Old packs mostly full of unused chunks: copy the used ones out.
    let mut sparse: Vec<(PackId, u64)> = sizes
        .iter()
        .filter(|(p, m)| !marked.contains(p) && m.modified.is_some_and(|t| t < cutoff))
        .filter_map(|(p, m)| {
            let used_bytes: u64 = live.get(p)?.values().map(|r| u64::from(r.stored_len)).sum();
            debug!(pack = ?p, used_bytes, size = m.size, "pack use");
            let sparse = (used_bytes as f64) < m.size as f64 * COMPACT_BELOW
                && m.size - used_bytes >= COMPACT_MIN_GAIN;
            sparse.then_some((*p, m.size))
        })
        .collect();
    sparse.sort();
    stats.compacted = sparse.len();
    stats.compacted_from = sparse.iter().map(|(_, size)| size).sum();
    stats.compacted_to = sparse
        .iter()
        .map(|(p, _)| live[p].values().map(|r| u64::from(r.stored_len)).sum::<u64>())
        .sum();
    stats.waiting += sparse.len();

    // Forwarding tables of deleted packs nothing points at any more.
    let direct: HashSet<PackId> = refs.keys().map(|(p, _)| *p).collect();
    let stale_forwards: Vec<PackId> = forwards
        .keys()
        .filter(|p| !sizes.contains_key(p) && !direct.contains(p))
        .copied()
        .collect();
    stats.forwards_deleted = stale_forwards.len();

    let snaps = list(store.as_ref(), "snap/").await?;
    let old_snaps: Vec<String> = snaps
        .iter()
        .rev()
        .skip(KEEP_SNAPSHOTS)
        .map(|m| m.key.clone())
        .collect();
    stats.snapshots_deleted = old_snaps.len();

    let stamp = Bytes::from(millis(opts.now).to_string());
    if !opts.dry_run {
        run_all(marks.iter().map(|p| {
            let (store, key, stamp) = (store.clone(), tombstone_key(*p), stamp.clone());
            async move { store.put(&key, stamp).await }
        }))
        .await?;
        run_all(unmarks.into_iter().map(|key| {
            let store = store.clone();
            async move { store.delete(&key).await }
        }))
        .await?;
        // The tombstone goes last, so an interrupted run leaves it to retry.
        run_all(deletes.into_iter().map(|(pack, tomb)| {
            let store = store.clone();
            async move {
                store.delete(&pack).await?;
                store.delete(&tomb).await
            }
        }))
        .await?;
        run_all(old_snaps.into_iter().map(|key| {
            let store = store.clone();
            async move { store.delete(&key).await }
        }))
        .await?;
        run_all(stale_forwards.into_iter().map(|p| {
            let store = store.clone();
            async move { store.delete(&rustfuse_chunk::forward_key(p)).await }
        }))
        .await?;
        let sealing = rustfuse_chunk::Sealing::with(opts.cipher.clone());
        compact(store.as_ref(), &sealing, &sparse, &live, &forwards, &stamp).await?;
    }
    info!(?stats, dry_run = opts.dry_run, "cleanup done");
    Ok(stats)
}

/// Chunk references (by pack and offset) used by the current state (files
/// that exist) or by any changeset from after `cutoff`, as written: some
/// may point into packs that were compacted since.
async fn used_refs(volume: &Volume, cutoff: SystemTime) -> Result<HashMap<(PackId, u64), ChunkRef>> {
    let mut used = HashMap::new();
    let (mut tree, start) = match volume.latest_snapshot().await? {
        Some(s) => (Tree::from_snapshot(&s), s.seq),
        None => (Tree::default(), 0),
    };
    let seqs = volume.changesets_after(0).await?;
    let cutoff_ns = cutoff
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos() as u64);
    // Changesets after the snapshot build the current state; older ones only
    // count while recent.
    let (before, after): (Vec<u64>, Vec<u64>) = seqs.into_iter().partition(|&s| s <= start);
    for seq in after {
        let cs = volume.open_changeset(volume.get_changeset(seq).await?)?;
        tree.apply(cs.version, &cs.ops);
        if cs.version.time.0 >= cutoff_ns {
            add_ops(&mut used, &cs.ops);
        }
    }
    for seq in before.into_iter().rev() {
        let cs = volume.open_changeset(volume.get_changeset(seq).await?)?;
        // The log is close to HLC order; this window is only a margin
        // (machines repair what they need), so the first old one ends it.
        if cs.version.time.0 < cutoff_ns {
            break;
        }
        add_ops(&mut used, &cs.ops);
    }
    for (_, n) in tree.iter() {
        if let Content::File { chunks, .. } = &n.entry.content {
            add_chunks(&mut used, chunks);
        }
    }
    debug!(chunks = used.len(), "chunks in use");
    Ok(used)
}

fn add_chunks(used: &mut HashMap<(PackId, u64), ChunkRef>, chunks: &[ChunkRef]) {
    for c in chunks {
        used.insert((c.pack, c.offset), *c);
    }
}

fn add_ops(used: &mut HashMap<(PackId, u64), ChunkRef>, ops: &[Op]) {
    for op in ops {
        if let Op::Upsert { entry, .. } = op
            && let Content::File { chunks, .. } = &entry.content
        {
            add_chunks(used, chunks);
        }
    }
}

async fn load_forwards(store: &dyn ObjectStore) -> Result<HashMap<PackId, Forward>> {
    let mut out = HashMap::new();
    for m in list(store, "fwd/").await? {
        let Some(p) = pack_of(&m.key) else { continue };
        if let Some(f) = rustfuse_chunk::load_forward(store, p).await? {
            out.insert(p, f);
        }
    }
    Ok(out)
}

/// Where each used chunk lives now, by pack and offset.
fn follow_forwards(
    refs: &HashMap<(PackId, u64), ChunkRef>,
    forwards: &HashMap<PackId, Forward>,
) -> HashMap<PackId, HashMap<u64, ChunkRef>> {
    let mut live: HashMap<PackId, HashMap<u64, ChunkRef>> = HashMap::new();
    for r in refs.values() {
        let mut r = *r;
        for _ in 0..MAX_HOPS {
            match forwards.get(&r.pack).and_then(|f| f.get(&r.offset)) {
                Some(n) if n.hash == r.hash => r = *n,
                _ => break,
            }
        }
        live.entry(r.pack).or_default().insert(r.offset, r);
    }
    live
}

/// Copies the used chunks of `sparse` packs into new packs, writes their
/// forwarding tables, then marks them for deletion. In that order: until
/// the table exists the old pack must stay.
async fn compact(
    store: &dyn ObjectStore,
    sealing: &rustfuse_chunk::Sealing,
    sparse: &[(PackId, u64)],
    live: &HashMap<PackId, HashMap<u64, ChunkRef>>,
    forwards: &HashMap<PackId, Forward>,
    stamp: &Bytes,
) -> Result<()> {
    let mut buf: Vec<u8> = Vec::new();
    let mut id = rustfuse_chunk::random_pack_id();
    let mut sources: Vec<(PackId, Forward)> = Vec::new();
    for (i, (pack, _)) in sparse.iter().enumerate() {
        let data = store.get(&pack.object_key()).await?;
        let mut chunks: Vec<&ChunkRef> = live[pack].values().collect();
        chunks.sort_by_key(|c| c.offset);
        // A pack compacted before (and used again since) keeps what it
        // already forwarded.
        let mut fwd = forwards.get(pack).cloned().unwrap_or_default();
        for c in chunks {
            let stored = data
                .get(c.offset as usize..(c.offset + u64::from(c.stored_len)) as usize)
                .ok_or_else(|| Error::Corrupt(format!("{}: too short", pack.object_key())))?;
            // Never copy damage into the new pack.
            sealing.decode(c, stored)?;
            let moved = ChunkRef {
                pack: id,
                offset: buf.len() as u64,
                ..*c
            };
            buf.extend_from_slice(stored);
            fwd.insert(c.offset, moved);
        }
        sources.push((*pack, fwd));
        if buf.len() >= rustfuse_chunk::PACK_TARGET || i + 1 == sparse.len() {
            store
                .put(&id.object_key(), Bytes::from(std::mem::take(&mut buf)))
                .await?;
            for (src, fwd) in sources.drain(..) {
                store
                    .put(
                        &rustfuse_chunk::forward_key(src),
                        rustfuse_chunk::encode_forward(&fwd),
                    )
                    .await?;
                store.put(&tombstone_key(src), stamp.clone()).await?;
            }
            debug!(pack = ?id, "wrote compacted pack");
            id = rustfuse_chunk::random_pack_id();
        }
    }
    Ok(())
}

async fn list(store: &dyn ObjectStore, prefix: &str) -> Result<Vec<ObjectMeta>> {
    let mut out = Vec::new();
    let mut after = None;
    loop {
        let page = store.list(prefix, after.as_deref(), 1000).await?;
        out.extend(page.objects);
        match page.next_start_after {
            Some(next) => after = Some(next),
            None => return Ok(out),
        }
    }
}

async fn read_time(store: &dyn ObjectStore, key: &str) -> Result<Option<SystemTime>> {
    let raw = match store.get(key).await {
        Ok(raw) => raw,
        Err(rustfuse_store::Error::NotFound(_)) => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let ms: u64 = std::str::from_utf8(&raw)
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .ok_or_else(|| Error::Corrupt(format!("{key}: bad tombstone")))?;
    Ok(Some(UNIX_EPOCH + Duration::from_millis(ms)))
}

fn millis(t: SystemTime) -> u64 {
    t.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
}

async fn run_all<F>(jobs: impl Iterator<Item = F>) -> Result<()>
where
    F: Future<Output = rustfuse_store::Result<()>>,
{
    stream::iter(jobs)
        .buffer_unordered(CONCURRENCY)
        .try_collect::<Vec<()>>()
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pack_keys_round_trip() {
        let p = PackId(0xabc << 100);
        assert_eq!(pack_of(&p.object_key()), Some(p));
        assert_eq!(pack_of(&tombstone_key(p)), Some(p));
        assert_eq!(pack_of("pack/ab/zz"), None);
    }
}
