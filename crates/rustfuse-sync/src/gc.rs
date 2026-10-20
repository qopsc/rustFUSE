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

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use futures::{StreamExt, TryStreamExt, stream};
use rustfuse_store::{ObjectMeta, ObjectStore};
use rustfuse_types::{Content, Op, PackId};
use tracing::{debug, info};

use crate::tree::Tree;
use crate::{Error, Result, Volume};

/// Snapshots kept; machines only ever read the latest.
const KEEP_SNAPSHOTS: usize = 2;
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
}

impl Default for GcOptions {
    fn default() -> Self {
        GcOptions {
            retention: Duration::from_secs(14 * 24 * 3600),
            delay: Duration::from_secs(3600),
            dry_run: false,
            now: SystemTime::now(),
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
}

/// Runs one cleanup pass over the volume `store` is scoped to.
pub async fn collect(store: Arc<dyn ObjectStore>, opts: &GcOptions) -> Result<GcStats> {
    let volume = Volume::new(store.clone());
    volume.check().await?;
    let cutoff = opts.now.checked_sub(opts.retention).unwrap_or(UNIX_EPOCH);
    let used = used_packs(&volume, cutoff).await?;
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

    let snaps = list(store.as_ref(), "snap/").await?;
    let old_snaps: Vec<String> = snaps
        .iter()
        .rev()
        .skip(KEEP_SNAPSHOTS)
        .map(|m| m.key.clone())
        .collect();
    stats.snapshots_deleted = old_snaps.len();

    if !opts.dry_run {
        let stamp = Bytes::from(millis(opts.now).to_string());
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
    }
    info!(?stats, dry_run = opts.dry_run, "cleanup done");
    Ok(stats)
}

/// Packs used by the current state (files that exist) or by any changeset
/// from after `cutoff`.
async fn used_packs(volume: &Volume, cutoff: SystemTime) -> Result<HashSet<PackId>> {
    let mut used = HashSet::new();
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
        let cs = volume.get_changeset(seq).await?;
        tree.apply(cs.version, &cs.ops);
        if cs.version.time.0 >= cutoff_ns {
            add_ops(&mut used, &cs.ops);
        }
    }
    for seq in before.into_iter().rev() {
        let cs = volume.get_changeset(seq).await?;
        // The log is close to HLC order; this window is only a margin
        // (machines repair what they need), so the first old one ends it.
        if cs.version.time.0 < cutoff_ns {
            break;
        }
        add_ops(&mut used, &cs.ops);
    }
    for (_, n) in tree.iter() {
        if let Content::File { chunks, .. } = &n.entry.content {
            used.extend(chunks.iter().map(|c| c.pack));
        }
    }
    debug!(packs = used.len(), "packs in use");
    Ok(used)
}

fn add_ops(used: &mut HashSet<PackId>, ops: &[Op]) {
    for op in ops {
        if let Op::Upsert { entry, .. } = op
            && let Content::File { chunks, .. } = &entry.content
        {
            used.extend(chunks.iter().map(|c| c.pack));
        }
    }
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
