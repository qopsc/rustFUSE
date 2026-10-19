//! The volume's shared state in the bucket: an ordered changeset log plus
//! periodic snapshots (`docs/design.md` §5.2).
//!
//! Layout under the volume prefix:
//! - `volume.json`: format marker
//! - `log/{seq:020}`: one [`Changeset`] (postcard + zstd)
//! - `snap/{seq:020}`: one [`Snapshot`] taken after applying `seq`
//! - `pack/..`: content packs
//!
//! Phase 1 has a single writer per volume. Phase 2 replaces the writer side
//! of the log with the hub, which orders concurrent writers.

use std::sync::Arc;

use bytes::Bytes;
use rustfuse_store::ObjectStore;
use rustfuse_types::{Changeset, FORMAT_VERSION, Snapshot};
use serde::{Deserialize, Serialize, de::DeserializeOwned};

use crate::{Error, Result};

const VOLUME_KEY: &str = "volume.json";

#[derive(Debug, Serialize, Deserialize)]
struct VolumeInfo {
    format: u32,
    created: rustfuse_types::Timestamp,
}

fn log_key(seq: u64) -> String {
    format!("log/{seq:020}")
}

fn snap_key(seq: u64) -> String {
    format!("snap/{seq:020}")
}

fn seq_of(key: &str) -> Option<u64> {
    key.rsplit('/').next()?.parse().ok()
}

fn encode<T: Serialize>(v: &T) -> Bytes {
    let raw = postcard::to_allocvec(v).expect("serialization cannot fail");
    Bytes::from(zstd::bulk::compress(&raw, 3).expect("zstd compression cannot fail"))
}

fn decode<T: DeserializeOwned>(key: &str, data: &[u8]) -> Result<T> {
    let raw = zstd::stream::decode_all(data).map_err(|e| Error::Corrupt(format!("{key}: {e}")))?;
    postcard::from_bytes(&raw).map_err(|e| Error::Corrupt(format!("{key}: {e}")))
}

#[derive(Clone)]
pub struct Volume {
    store: Arc<dyn ObjectStore>,
}

impl Volume {
    /// `store` must already be scoped to the volume's prefix.
    pub fn new(store: Arc<dyn ObjectStore>) -> Self {
        Volume { store }
    }

    pub fn store(&self) -> &Arc<dyn ObjectStore> {
        &self.store
    }

    pub async fn exists(&self) -> Result<bool> {
        Ok(self.store.head(VOLUME_KEY).await?.is_some())
    }

    pub async fn create(&self) -> Result<()> {
        if self.exists().await? {
            return Err(Error::VolumeExists);
        }
        let info = VolumeInfo {
            format: FORMAT_VERSION,
            created: rustfuse_types::Timestamp::now(),
        };
        self.store
            .put(VOLUME_KEY, Bytes::from(serde_json::to_vec(&info).unwrap()))
            .await?;
        Ok(())
    }

    pub async fn check(&self) -> Result<()> {
        let raw = match self.store.get(VOLUME_KEY).await {
            Ok(raw) => raw,
            Err(rustfuse_store::Error::NotFound(_)) => return Err(Error::NoVolume),
            Err(e) => return Err(e.into()),
        };
        let info: VolumeInfo =
            serde_json::from_slice(&raw).map_err(|e| Error::Corrupt(format!("{VOLUME_KEY}: {e}")))?;
        if info.format != FORMAT_VERSION {
            return Err(Error::Corrupt(format!(
                "unsupported volume format {}",
                info.format
            )));
        }
        Ok(())
    }

    /// Appends a changeset. Fails with [`Error::Conflict`] if its sequence
    /// number is already taken (another writer got there first).
    pub async fn put_changeset(&self, cs: &Changeset) -> Result<()> {
        let key = log_key(cs.seq);
        if self.store.head(&key).await?.is_some() {
            return Err(Error::Conflict(cs.seq));
        }
        self.store.put(&key, encode(cs)).await?;
        Ok(())
    }

    pub async fn get_changeset(&self, seq: u64) -> Result<Changeset> {
        let key = log_key(seq);
        let cs: Changeset = decode(&key, &self.store.get(&key).await?)?;
        if cs.format != FORMAT_VERSION || cs.seq != seq {
            return Err(Error::Corrupt(format!("{key}: unexpected format or sequence")));
        }
        Ok(cs)
    }

    /// Sequence numbers of changesets after `after`, in order.
    pub async fn changesets_after(&self, after: u64) -> Result<Vec<u64>> {
        self.list_seqs("log/", Some(log_key(after))).await
    }

    pub async fn put_snapshot(&self, snap: &Snapshot) -> Result<()> {
        self.store.put(&snap_key(snap.seq), encode(snap)).await?;
        Ok(())
    }

    pub async fn latest_snapshot(&self) -> Result<Option<Snapshot>> {
        let Some(seq) = self.list_seqs("snap/", None).await?.pop() else {
            return Ok(None);
        };
        let key = snap_key(seq);
        let snap: Snapshot = decode(&key, &self.store.get(&key).await?)?;
        Ok(Some(snap))
    }

    async fn list_seqs(&self, prefix: &str, start_after: Option<String>) -> Result<Vec<u64>> {
        let mut out = Vec::new();
        let mut after = start_after;
        loop {
            let page = self.store.list(prefix, after.as_deref(), 1000).await?;
            out.extend(page.objects.iter().filter_map(|o| seq_of(&o.key)));
            match page.next_start_after {
                Some(next) => after = Some(next),
                None => break,
            }
        }
        out.sort_unstable();
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustfuse_store::MemoryStore;
    use rustfuse_types::{ReplicaId, Timestamp};

    fn cs(seq: u64) -> Changeset {
        Changeset {
            format: FORMAT_VERSION,
            seq,
            replica: ReplicaId(1),
            time: Timestamp(0),
            ops: vec![],
        }
    }

    #[tokio::test]
    async fn log_and_snapshots() {
        let v = Volume::new(Arc::new(MemoryStore::new()));
        assert!(matches!(v.check().await, Err(Error::NoVolume)));
        v.create().await.unwrap();
        v.check().await.unwrap();
        assert!(matches!(v.create().await, Err(Error::VolumeExists)));

        for seq in 1..=12 {
            v.put_changeset(&cs(seq)).await.unwrap();
        }
        assert!(matches!(v.put_changeset(&cs(3)).await, Err(Error::Conflict(3))));
        assert_eq!(v.changesets_after(9).await.unwrap(), vec![10, 11, 12]);
        assert_eq!(v.changesets_after(0).await.unwrap().len(), 12);
        assert_eq!(v.get_changeset(4).await.unwrap(), cs(4));

        assert!(v.latest_snapshot().await.unwrap().is_none());
        for seq in [2, 10] {
            v.put_snapshot(&Snapshot {
                format: FORMAT_VERSION,
                seq,
                nodes: vec![],
            })
            .await
            .unwrap();
        }
        assert_eq!(v.latest_snapshot().await.unwrap().unwrap().seq, 10);
    }
}
