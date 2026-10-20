//! Per-machine keys: each machine gets its own token, which can be limited
//! to some volumes and revoked without touching the other machines.
//!
//! Keys live in the bucket next to the volumes (`keys/<id>`, JSON), so every
//! hub of the bucket sees the same set and the storage proxy, confined to
//! `volumes/<name>/`, can never reach them. A token is `rfk_<id>_<secret>`;
//! the bucket holds only the BLAKE3 hash of the 256-bit secret. Hubs cache
//! keys for [`CACHE_TTL`], so a revoked key stops working within that time,
//! open connections included.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use rustfuse_store::ObjectStore;
use serde::{Deserialize, Serialize};
use tracing::warn;

use crate::{Error, Result, valid_volume_name};

/// Where keys live, under the hub's root (the prefix that holds `volumes/`).
pub const KEY_PREFIX: &str = "keys/";
const TOKEN_PREFIX: &str = "rfk_";
/// How long a hub trusts what it read about a key.
pub const CACHE_TTL: Duration = Duration::from_secs(30);

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Key {
    /// 16 hex digits; part of the token and the object name.
    pub id: String,
    /// What the key is for, e.g. the machine's name.
    pub name: String,
    /// BLAKE3 of the secret, hex.
    pub hash: String,
    /// Volumes the key opens; empty means every volume.
    pub volumes: Vec<String>,
    /// Milliseconds since the epoch.
    pub created: u64,
}

impl Key {
    pub fn allows(&self, volume: &str) -> bool {
        self.volumes.is_empty() || self.volumes.iter().any(|v| v == volume)
    }
}

fn key_path(id: &str) -> String {
    format!("{KEY_PREFIX}{id}")
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn random_hex(n: usize) -> String {
    let mut buf = vec![0u8; n];
    getrandom::fill(&mut buf).expect("system random source");
    hex(&buf)
}

fn hash_secret(secret: &str) -> String {
    blake3::hash(secret.as_bytes()).to_hex().to_string()
}

/// Splits a token into key id and secret, if it is shaped like a key token.
fn parse(token: &str) -> Option<(&str, &str)> {
    let (id, secret) = token.strip_prefix(TOKEN_PREFIX)?.split_once('_')?;
    let ok = |s: &str, len: usize| s.len() == len && s.bytes().all(|b| b.is_ascii_hexdigit());
    (ok(id, 16) && ok(secret, 64)).then_some((id, secret))
}

/// Creates a key for `name`, limited to `volumes` (empty: all), and returns
/// it with its token. The token is not stored anywhere; show it once.
pub async fn create(root: &dyn ObjectStore, name: &str, volumes: Vec<String>) -> Result<(Key, String)> {
    if let Some(bad) = volumes.iter().find(|v| !valid_volume_name(v)) {
        return Err(Error::Refused(format!("invalid volume name {bad:?}")));
    }
    let (id, secret) = (random_hex(8), random_hex(32));
    let created = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;
    let key = Key {
        id: id.clone(),
        name: name.to_owned(),
        hash: hash_secret(&secret),
        volumes,
        created,
    };
    let json = serde_json::to_vec_pretty(&key).expect("serializable");
    if !root.put_if_absent(&key_path(&id), Bytes::from(json)).await? {
        return Err(Error::Corrupt(format!("key id {id} already exists")));
    }
    Ok((key, format!("{TOKEN_PREFIX}{id}_{secret}")))
}

/// Every key, by id.
pub async fn list(root: &dyn ObjectStore) -> Result<Vec<Key>> {
    let mut out = Vec::new();
    let mut after = None;
    loop {
        let page = root.list(KEY_PREFIX, after.as_deref(), 1000).await?;
        for o in &page.objects {
            if let Some(k) = read(root, &o.key).await? {
                out.push(k);
            }
        }
        match page.next_start_after {
            Some(n) => after = Some(n),
            None => break,
        }
    }
    Ok(out)
}

/// Deletes a key. Returns whether it existed.
pub async fn revoke(root: &dyn ObjectStore, id: &str) -> Result<bool> {
    let path = key_path(id);
    let existed = root.head(&path).await?.is_some();
    root.delete(&path).await?;
    Ok(existed)
}

async fn read(root: &dyn ObjectStore, path: &str) -> Result<Option<Key>> {
    match root.get(path).await {
        Ok(raw) => serde_json::from_slice(&raw)
            .map(Some)
            .map_err(|e| Error::Corrupt(format!("{path}: {e}"))),
        Err(rustfuse_store::Error::NotFound(_)) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// A key as last read (`None`: no such key), and when.
type Cached = (Option<Arc<Key>>, Instant);

/// The hub's view of the keys, cached for [`CACHE_TTL`].
pub(crate) struct Keys {
    root: Arc<dyn ObjectStore>,
    ttl: Duration,
    cache: Mutex<HashMap<String, Cached>>,
}

impl Keys {
    pub(crate) fn new(root: Arc<dyn ObjectStore>, ttl: Duration) -> Keys {
        Keys {
            root,
            ttl,
            cache: Mutex::new(HashMap::new()),
        }
    }

    /// The key a token belongs to, if the token is a valid key token.
    pub(crate) async fn verify(&self, token: &str) -> Result<Option<Arc<Key>>> {
        let Some((id, secret)) = parse(token) else {
            return Ok(None);
        };
        let Some(key) = self.get(id).await? else {
            return Ok(None);
        };
        let hash = hash_secret(secret);
        Ok(crate::server::constant_time_eq(hash.as_bytes(), key.hash.as_bytes()).then_some(key))
    }

    /// Whether key `id` still exists and opens `volume`. A failed read of
    /// the bucket keeps the last answer: a hiccup must not cut machines off.
    pub(crate) async fn still_allows(&self, id: &str, volume: &str) -> bool {
        match self.get(id).await {
            Ok(k) => k.is_some_and(|k| k.allows(volume)),
            Err(e) => {
                warn!(key = id, "cannot re-check key: {e}");
                let cache = self.cache.lock().unwrap();
                cache
                    .get(id)
                    .is_some_and(|(k, _)| k.as_ref().is_some_and(|k| k.allows(volume)))
            }
        }
    }

    async fn get(&self, id: &str) -> Result<Option<Arc<Key>>> {
        if let Some((k, at)) = self.cache.lock().unwrap().get(id)
            && at.elapsed() < self.ttl
        {
            return Ok(k.clone());
        }
        let key = read(&*self.root, &key_path(id)).await?.map(Arc::new);
        // A key file must describe itself.
        let key = key.filter(|k| k.id == id);
        let mut cache = self.cache.lock().unwrap();
        // Unknown ids are cached too, but bounded: they cost a read each.
        if cache.len() > 10_000 {
            let ttl = self.ttl;
            cache.retain(|_, (_, at)| at.elapsed() < ttl);
        }
        cache.insert(id.to_owned(), (key.clone(), Instant::now()));
        Ok(key)
    }
}

/// Re-checks a connection's key at most once per TTL.
pub(crate) struct KeyCheck {
    pub(crate) keys: Arc<Keys>,
    pub(crate) id: String,
    pub(crate) volume: String,
    pub(crate) last: Instant,
}

impl KeyCheck {
    pub(crate) async fn ok(&mut self) -> bool {
        if self.last.elapsed() < self.keys.ttl {
            return true;
        }
        self.last = Instant::now();
        self.keys.still_allows(&self.id, &self.volume).await
    }

    pub(crate) fn ttl(&self) -> Duration {
        self.keys.ttl
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustfuse_store::MemoryStore;

    #[tokio::test]
    async fn create_verify_revoke() {
        let root: Arc<dyn ObjectStore> = Arc::new(MemoryStore::new());
        let (k, token) = create(&*root, "laptop", vec!["v".into()]).await.unwrap();
        assert!(parse(&token).is_some());
        assert!(!format!("{k:?}").contains(&token[21..]), "secret stored");
        let keys = Keys::new(root.clone(), CACHE_TTL);
        assert_eq!(keys.verify(&token).await.unwrap().unwrap().name, "laptop");
        let mut wrong = token.clone();
        wrong.replace_range(token.len() - 1.., if token.ends_with('0') { "1" } else { "0" });
        assert!(keys.verify(&wrong).await.unwrap().is_none());
        assert!(keys.verify("secret").await.unwrap().is_none());
        assert!(keys.still_allows(&k.id, "v").await);
        assert!(!keys.still_allows(&k.id, "w").await);
        assert_eq!(list(&*root).await.unwrap(), vec![k.clone()]);

        assert!(revoke(&*root, &k.id).await.unwrap());
        assert!(!revoke(&*root, &k.id).await.unwrap());
        // Cached until the TTL runs out; a fresh view sees the revocation.
        assert!(keys.still_allows(&k.id, "v").await);
        let fresh = Keys::new(root.clone(), CACHE_TTL);
        assert!(fresh.verify(&token).await.unwrap().is_none());
        assert!(!fresh.still_allows(&k.id, "v").await);
        assert!(create(&*root, "x", vec!["../y".into()]).await.is_err());
    }
}
