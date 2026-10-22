//! The volume's shared state in the bucket: an ordered changeset log plus
//! periodic snapshots (`docs/design.md` §5.2).
//!
//! Layout under the volume prefix:
//! - `volume.json`: format marker
//! - `log/{seq:020}`: one [`Changeset`] (postcard + zstd)
//! - `snap/{seq:020}`: one [`Snapshot`] taken after applying `seq`
//! - `pack/..`: content packs
//!
//! Only the hub appends to the log; machines read snapshots and log
//! entries directly to catch up.
//!
//! In an encrypted volume (`volume.json` holds its wrapped key), each
//! changeset carries its ops as one [`Op::Sealed`], and each snapshot
//! object is sealed whole. The hub stores and forwards them as they are;
//! machines seal and open them with a [`Volume`] that has the key.

use std::sync::Arc;

use bytes::Bytes;
use rustfuse_crypt::{Cipher, KeyInfo, Meta};
use rustfuse_store::ObjectStore;
use rustfuse_types::{Changeset, FORMAT_VERSION, Op, Snapshot, Version};
use serde::{Deserialize, Serialize};

use crate::{Error, Result, codec};

const VOLUME_KEY: &str = "volume.json";

#[derive(Debug, Serialize, Deserialize)]
struct VolumeInfo {
    format: u32,
    created: rustfuse_types::Timestamp,
    /// The volume key, wrapped with the passphrase; absent when the
    /// volume is not encrypted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    encryption: Option<KeyInfo>,
}

fn log_key(seq: u64) -> String {
    format!("log/{seq:020}")
}

fn snap_key(seq: u64) -> String {
    format!("snap/{seq:020}")
}

/// Names of the volumes under `root` (the bucket prefix holding
/// `volumes/`), sorted. One listing call per volume: each skips past the
/// previous volume's objects.
pub async fn list_volumes(root: &dyn ObjectStore) -> Result<Vec<String>> {
    const PREFIX: &str = "volumes/";
    let mut out = Vec::new();
    let mut after: Option<String> = None;
    loop {
        let page = root.list(PREFIX, after.as_deref(), 1).await?;
        let Some(obj) = page.objects.first() else {
            out.sort();
            return Ok(out);
        };
        let rest = &obj.key[PREFIX.len().min(obj.key.len())..];
        let Some((name, _)) = rest.split_once('/') else {
            // A stray object right under `volumes/`.
            after = Some(obj.key.clone());
            continue;
        };
        if crate::valid_volume_name(name)
            && root
                .head(&format!("{PREFIX}{name}/{VOLUME_KEY}"))
                .await?
                .is_some()
        {
            out.push(name.to_owned());
        }
        // Volume names are ASCII and every key below them is too, so this
        // sorts after all of this volume's objects.
        after = Some(format!("{PREFIX}{name}/\u{7f}"));
    }
}

fn seq_of(key: &str) -> Option<u64> {
    key.rsplit('/').next()?.parse().ok()
}

#[derive(Clone)]
pub struct Volume {
    store: Arc<dyn ObjectStore>,
    cipher: Option<Arc<Cipher>>,
}

fn version_bytes(v: Version) -> [u8; 16] {
    let mut b = [0u8; 16];
    b[..8].copy_from_slice(&v.time.0.to_le_bytes());
    b[8..].copy_from_slice(&v.replica.0.to_le_bytes());
    b
}

impl Volume {
    /// `store` must already be scoped to the volume's prefix.
    pub fn new(store: Arc<dyn ObjectStore>) -> Self {
        Volume { store, cipher: None }
    }

    /// Seals and opens with `cipher` (an encrypted volume's key).
    pub fn with_cipher(mut self, cipher: Option<Arc<Cipher>>) -> Self {
        self.cipher = cipher;
        self
    }

    pub fn cipher(&self) -> Option<&Arc<Cipher>> {
        self.cipher.as_ref()
    }

    /// The wrapped key of an encrypted volume (`None`: not encrypted).
    pub async fn key_info(&self) -> Result<Option<KeyInfo>> {
        Ok(self.info().await?.encryption)
    }

    async fn info(&self) -> Result<VolumeInfo> {
        let raw = match self.store.get(VOLUME_KEY).await {
            Ok(raw) => raw,
            Err(rustfuse_store::Error::NotFound(_)) => return Err(Error::NoVolume),
            Err(e) => return Err(e.into()),
        };
        serde_json::from_slice(&raw).map_err(|e| Error::Corrupt(format!("{VOLUME_KEY}: {e}")))
    }

    /// Replaces an encrypted volume's wrapped key (a new passphrase for
    /// the same key).
    pub async fn set_key_info(&self, key: KeyInfo) -> Result<()> {
        let mut info = self.info().await?;
        if info.encryption.is_none() {
            return Err(Error::Corrupt("the volume is not encrypted".into()));
        }
        info.encryption = Some(key);
        self.store
            .put(VOLUME_KEY, Bytes::from(serde_json::to_vec(&info).unwrap()))
            .await?;
        Ok(())
    }

    /// The changeset as the hub stores it: in an encrypted volume, its ops
    /// sealed into one [`Op::Sealed`].
    pub fn seal_changeset(&self, cs: &Changeset) -> Changeset {
        let Some(c) = &self.cipher else {
            return cs.clone();
        };
        let what = Meta::Changeset {
            version: version_bytes(cs.version),
            local: cs.local,
        };
        Changeset {
            ops: vec![Op::Sealed(c.seal_meta(what, &codec::encode(&cs.ops)))],
            ..cs.clone()
        }
    }

    /// The changeset with its ops readable. In an encrypted volume they
    /// must be sealed with its key; in any other they must not be sealed.
    pub fn open_changeset(&self, mut cs: Changeset) -> Result<Changeset> {
        let sealed = match cs.ops.as_slice() {
            [Op::Sealed(data)] => Some(data),
            ops if ops.iter().any(|o| matches!(o, Op::Sealed(_))) => {
                return Err(Error::Corrupt(format!("changeset {}: mixed sealed ops", cs.seq)));
            }
            _ => None,
        };
        match (&self.cipher, sealed) {
            (None, None) => Ok(cs),
            (None, Some(_)) => Err(Error::NeedKey),
            (Some(_), None) if cs.ops.is_empty() => Ok(cs),
            (Some(_), None) => Err(Error::Corrupt(format!(
                "changeset {} is not sealed in an encrypted volume",
                cs.seq
            ))),
            (Some(c), Some(data)) => {
                let what = Meta::Changeset {
                    version: version_bytes(cs.version),
                    local: cs.local,
                };
                let raw = c.open_meta(what, data)?;
                cs.ops = codec::decode(&format!("changeset {}", cs.seq), &raw)?;
                Ok(cs)
            }
        }
    }

    pub fn store(&self) -> &Arc<dyn ObjectStore> {
        &self.store
    }

    pub async fn exists(&self) -> Result<bool> {
        Ok(self.store.head(VOLUME_KEY).await?.is_some())
    }

    pub async fn create(&self) -> Result<()> {
        self.create_with(None).await
    }

    /// Creates the volume, encrypted with the key `encryption` wraps.
    pub async fn create_with(&self, encryption: Option<KeyInfo>) -> Result<()> {
        if self.exists().await? {
            return Err(Error::VolumeExists);
        }
        let info = VolumeInfo {
            format: FORMAT_VERSION,
            created: rustfuse_types::Timestamp::now(),
            encryption,
        };
        self.store
            .put(VOLUME_KEY, Bytes::from(serde_json::to_vec(&info).unwrap()))
            .await?;
        Ok(())
    }

    pub async fn check(&self) -> Result<()> {
        let info = self.info().await?;
        if info.format != FORMAT_VERSION {
            return Err(Error::Corrupt(format!(
                "unsupported volume format {}",
                info.format
            )));
        }
        Ok(())
    }

    /// Appends a changeset. Fails with [`Error::Conflict`] if its sequence
    /// number is already taken (another hub got there first); the write is
    /// atomic, so two hubs can never both claim a number.
    pub async fn put_changeset(&self, cs: &Changeset) -> Result<()> {
        if !self
            .store
            .put_if_absent(&log_key(cs.seq), codec::encode(cs))
            .await?
        {
            return Err(Error::Conflict(cs.seq));
        }
        Ok(())
    }

    pub async fn get_changeset(&self, seq: u64) -> Result<Changeset> {
        let key = log_key(seq);
        let cs: Changeset = codec::decode(&key, &self.store.get(&key).await?)?;
        if cs.format != FORMAT_VERSION || cs.seq != seq {
            return Err(Error::Corrupt(format!("{key}: unexpected format or sequence")));
        }
        Ok(cs)
    }

    /// Sequence numbers of changesets after `after`, in order.
    pub async fn changesets_after(&self, after: u64) -> Result<Vec<u64>> {
        self.list_seqs("log/", Some(log_key(after))).await
    }

    /// Sequence number of the last changeset (0 for an empty log).
    pub async fn head(&self) -> Result<u64> {
        Ok(self.changesets_after(0).await?.last().copied().unwrap_or(0))
    }

    pub async fn put_snapshot(&self, snap: &Snapshot) -> Result<()> {
        let mut data = codec::encode(snap);
        if let Some(c) = &self.cipher {
            data = Bytes::from(c.seal_meta(Meta::Snapshot { seq: snap.seq }, &data));
        }
        self.store.put(&snap_key(snap.seq), data).await?;
        Ok(())
    }

    pub async fn latest_snapshot(&self) -> Result<Option<Snapshot>> {
        let Some(seq) = self.list_seqs("snap/", None).await?.pop() else {
            return Ok(None);
        };
        let key = snap_key(seq);
        let mut data = self.store.get(&key).await?;
        if let Some(c) = &self.cipher {
            data = Bytes::from(c.open_meta(Meta::Snapshot { seq }, &data)?);
        }
        let snap: Snapshot = codec::decode(&key, &data)?;
        if snap.seq != seq {
            return Err(Error::Corrupt(format!("{key}: holds snapshot {}", snap.seq)));
        }
        if snap.format != FORMAT_VERSION {
            return Err(Error::Corrupt(format!(
                "{key}: unsupported format {}",
                snap.format
            )));
        }
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

    fn cs(seq: u64) -> Changeset {
        Changeset {
            format: FORMAT_VERSION,
            seq,
            version: Default::default(),
            local: seq,
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
        assert_eq!(v.head().await.unwrap(), 12);
        assert_eq!(v.changesets_after(0).await.unwrap().len(), 12);
        assert_eq!(v.get_changeset(4).await.unwrap(), cs(4));

        assert!(v.latest_snapshot().await.unwrap().is_none());
        for seq in [2, 10] {
            v.put_snapshot(&Snapshot {
                format: FORMAT_VERSION,
                seq,
                nodes: vec![],
                removed: vec![],
            })
            .await
            .unwrap();
        }
        assert_eq!(v.latest_snapshot().await.unwrap().unwrap().seq, 10);
    }

    #[tokio::test]
    async fn encrypted_volumes_seal_changesets_and_snapshots() {
        use rustfuse_types::{NodeId, Versions};
        let store: Arc<dyn ObjectStore> = Arc::new(MemoryStore::new());
        let (info, _) = KeyInfo::create_with(
            "pass",
            rustfuse_crypt::KdfParams {
                memory_kib: 64,
                iterations: 1,
                lanes: 1,
            },
        )
        .unwrap();
        let hub = Volume::new(store.clone());
        hub.create_with(Some(info.clone())).await.unwrap();
        assert_eq!(hub.key_info().await.unwrap(), Some(info.clone()));
        let key = info.unlock("pass").unwrap();
        let machine = Volume::new(store.clone()).with_cipher(Some(Arc::new(Cipher::new(&key))));

        let mut plain = cs(0);
        plain.ops = vec![Op::Remove {
            node: NodeId(42),
            base: Versions::default(),
        }];
        let mut sealed = machine.seal_changeset(&plain);
        assert!(matches!(sealed.ops.as_slice(), [Op::Sealed(_)]));
        // The hub numbers and stores it without reading it.
        sealed.seq = 1;
        hub.put_changeset(&sealed).await.unwrap();
        let back = machine
            .open_changeset(hub.get_changeset(1).await.unwrap())
            .unwrap();
        assert_eq!(back.ops, plain.ops);
        assert!(matches!(hub.open_changeset(sealed.clone()), Err(Error::NeedKey)));
        // A hub cannot pass off ops in the clear, or one changeset's ops
        // as another's.
        assert!(machine.open_changeset(plain.clone()).is_err());
        let mut moved = sealed.clone();
        moved.local += 1;
        assert!(machine.open_changeset(moved).is_err());

        let snap = Snapshot {
            format: FORMAT_VERSION,
            seq: 1,
            nodes: vec![],
            removed: vec![],
        };
        machine.put_snapshot(&snap).await.unwrap();
        assert_eq!(machine.latest_snapshot().await.unwrap(), Some(snap));
        assert!(hub.latest_snapshot().await.is_err());
    }
}
