//! Client-side encryption of a volume (`docs/design.md` §5.6): file content,
//! names and every other piece of metadata are sealed on the machine before
//! they reach the hub or the bucket, which then see only sizes and timing.
//!
//! Each encrypted volume has one random 256-bit [`VolumeKey`]. The bucket
//! keeps it only wrapped by a key derived from the volume's passphrase with
//! Argon2id ([`KeyInfo`], in `volume.json`), so changing the passphrase
//! rewraps one small record and never touches the data. From the volume
//! key, a [`Cipher`] derives separate keys for:
//!
//! - chunk ids: a keyed BLAKE3 hash of the chunk's content, so identical
//!   content still deduplicates within the volume but the ids say nothing
//!   to anyone without the key (no confirming a file by its hash);
//! - chunk content and metadata (changesets, snapshots): XChaCha20-Poly1305
//!   with a random 192-bit nonce per object, stored as `nonce || ciphertext`.
//!   Metadata is bound to what it is (its kind and, where known, its
//!   position) as associated data, so the hub cannot pass one record off as
//!   another.

use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use rustfuse_types::ChunkHash;
use serde::{Deserialize, Serialize};
use zeroize::{Zeroize, ZeroizeOnDrop};

const NONCE: usize = 24;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("wrong passphrase")]
    WrongPassphrase,
    #[error("cannot decrypt {0}: corrupt, or sealed with another key")]
    Corrupt(&'static str),
    #[error("bad key: {0}")]
    BadKey(String),
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

fn random<const N: usize>() -> [u8; N] {
    let mut b = [0u8; N];
    getrandom::fill(&mut b).expect("no system randomness");
    b
}

/// The key everything in one volume is sealed with.
#[derive(Clone, PartialEq, Eq, Zeroize, ZeroizeOnDrop)]
pub struct VolumeKey([u8; 32]);

impl std::fmt::Debug for VolumeKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("VolumeKey(..)")
    }
}

impl VolumeKey {
    pub fn generate() -> VolumeKey {
        VolumeKey(random())
    }

    /// Hex, for keeping on a machine (`.rustfuse/key`).
    pub fn to_hex(&self) -> String {
        hex::encode(self.0)
    }

    pub fn from_hex(s: &str) -> Result<VolumeKey> {
        let mut k = [0u8; 32];
        hex::decode_to_slice(s.trim(), &mut k).map_err(|e| Error::BadKey(e.to_string()))?;
        Ok(VolumeKey(k))
    }
}

/// Argon2id cost: memory in KiB, passes and lanes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct KdfParams {
    pub memory_kib: u32,
    pub iterations: u32,
    pub lanes: u32,
}

impl Default for KdfParams {
    /// About half a second on a laptop: cheap once per attach, costly per
    /// guess.
    fn default() -> Self {
        KdfParams {
            memory_kib: 64 << 10,
            iterations: 3,
            lanes: 1,
        }
    }
}

/// The volume key, wrapped with the passphrase. Kept in `volume.json`;
/// safe to publish (the passphrase is the secret).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeyInfo {
    /// Always `argon2id+xchacha20poly1305`.
    pub scheme: String,
    pub kdf: KdfParams,
    /// Hex.
    pub salt: String,
    /// Hex: `nonce || sealed volume key`.
    pub wrapped: String,
}

const SCHEME: &str = "argon2id+xchacha20poly1305";

impl KeyInfo {
    /// A new volume key, and its record wrapped with `passphrase`.
    pub fn create(passphrase: &str) -> Result<(KeyInfo, VolumeKey)> {
        Self::create_with(passphrase, KdfParams::default())
    }

    pub fn create_with(passphrase: &str, kdf: KdfParams) -> Result<(KeyInfo, VolumeKey)> {
        let key = VolumeKey::generate();
        Ok((Self::wrap(&key, passphrase, kdf)?, key))
    }

    /// The same key under a new passphrase.
    pub fn wrap(key: &VolumeKey, passphrase: &str, kdf: KdfParams) -> Result<KeyInfo> {
        if passphrase.is_empty() {
            return Err(Error::BadKey("the passphrase is empty".into()));
        }
        let salt: [u8; 16] = random();
        let wrapping = derive(passphrase, &salt, kdf)?;
        let sealed = seal(&wrapping, SCHEME.as_bytes(), &key.0);
        Ok(KeyInfo {
            scheme: SCHEME.into(),
            kdf,
            salt: hex::encode(salt),
            wrapped: hex::encode(sealed),
        })
    }

    pub fn unlock(&self, passphrase: &str) -> Result<VolumeKey> {
        if self.scheme != SCHEME {
            return Err(Error::BadKey(format!("unknown scheme {:?}", self.scheme)));
        }
        let salt = hex::decode(&self.salt).map_err(|e| Error::BadKey(e.to_string()))?;
        let wrapped = hex::decode(&self.wrapped).map_err(|e| Error::BadKey(e.to_string()))?;
        let wrapping = derive(passphrase, &salt, self.kdf)?;
        let mut plain = open(&wrapping, SCHEME.as_bytes(), &wrapped).map_err(|_| Error::WrongPassphrase)?;
        let key: [u8; 32] = plain
            .as_slice()
            .try_into()
            .map_err(|_| Error::BadKey("wrong key length".into()))?;
        plain.zeroize();
        Ok(VolumeKey(key))
    }
}

fn derive(passphrase: &str, salt: &[u8], p: KdfParams) -> Result<XChaCha20Poly1305> {
    let params = argon2::Params::new(p.memory_kib, p.iterations, p.lanes, Some(32))
        .map_err(|e| Error::BadKey(format!("key derivation parameters: {e}")))?;
    let argon = argon2::Argon2::new(argon2::Algorithm::Argon2id, argon2::Version::V0x13, params);
    let mut out = [0u8; 32];
    argon
        .hash_password_into(passphrase.as_bytes(), salt, &mut out)
        .map_err(|e| Error::BadKey(format!("key derivation: {e}")))?;
    let aead = XChaCha20Poly1305::new((&out).into());
    out.zeroize();
    Ok(aead)
}

fn seal(aead: &XChaCha20Poly1305, aad: &[u8], plain: &[u8]) -> Vec<u8> {
    let nonce: [u8; NONCE] = random();
    let ct = aead
        .encrypt(XNonce::from_slice(&nonce), Payload { msg: plain, aad })
        .expect("encryption cannot fail");
    let mut out = Vec::with_capacity(NONCE + ct.len());
    out.extend_from_slice(&nonce);
    out.extend_from_slice(&ct);
    out
}

fn open(aead: &XChaCha20Poly1305, aad: &[u8], sealed: &[u8]) -> Result<Vec<u8>, ()> {
    if sealed.len() < NONCE {
        return Err(());
    }
    let (nonce, ct) = sealed.split_at(NONCE);
    aead.decrypt(XNonce::from_slice(nonce), Payload { msg: ct, aad })
        .map_err(|_| ())
}

/// What a piece of sealed metadata is, bound to it as associated data.
#[derive(Clone, Copy, Debug)]
pub enum Meta {
    /// A changeset's operations, by its author's version and counter (both
    /// stored in the clear next to them).
    Changeset { version: [u8; 16], local: u64 },
    /// The snapshot taken after `seq`.
    Snapshot { seq: u64 },
}

impl Meta {
    fn aad(self) -> Vec<u8> {
        match self {
            Meta::Changeset { version, local } => [b"cs".as_slice(), &version, &local.to_le_bytes()].concat(),
            Meta::Snapshot { seq } => [b"snap".as_slice(), &seq.to_le_bytes()].concat(),
        }
    }

    fn what(self) -> &'static str {
        match self {
            Meta::Changeset { .. } => "a changeset",
            Meta::Snapshot { .. } => "a snapshot",
        }
    }
}

/// Seals and opens one volume's chunks and metadata.
pub struct Cipher {
    id_key: [u8; 32],
    chunks: XChaCha20Poly1305,
    meta: XChaCha20Poly1305,
}

impl Drop for Cipher {
    fn drop(&mut self) {
        self.id_key.zeroize();
    }
}

impl std::fmt::Debug for Cipher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Cipher(..)")
    }
}

impl Cipher {
    pub fn new(key: &VolumeKey) -> Cipher {
        let sub = |context: &str| blake3::derive_key(context, &key.0);
        let (mut c, mut m) = (
            sub("rustfuse 2026-10 chunk content"),
            sub("rustfuse 2026-10 metadata"),
        );
        let cipher = Cipher {
            id_key: sub("rustfuse 2026-10 chunk id"),
            chunks: XChaCha20Poly1305::new((&c).into()),
            meta: XChaCha20Poly1305::new((&m).into()),
        };
        c.zeroize();
        m.zeroize();
        cipher
    }

    /// A chunk's id: keyed, so it reveals nothing about the content.
    pub fn chunk_hash(&self, data: &[u8]) -> ChunkHash {
        ChunkHash(*blake3::keyed_hash(&self.id_key, data).as_bytes())
    }

    /// `nonce || ciphertext` of a chunk's stored (compressed) bytes, bound
    /// to its id.
    pub fn seal_chunk(&self, id: &ChunkHash, stored: &[u8]) -> Vec<u8> {
        seal(&self.chunks, &id.0, stored)
    }

    pub fn open_chunk(&self, id: &ChunkHash, sealed: &[u8]) -> Result<Vec<u8>> {
        open(&self.chunks, &id.0, sealed).map_err(|()| Error::Corrupt("a chunk"))
    }

    pub fn seal_meta(&self, what: Meta, plain: &[u8]) -> Vec<u8> {
        seal(&self.meta, &what.aad(), plain)
    }

    pub fn open_meta(&self, what: Meta, sealed: &[u8]) -> Result<Vec<u8>> {
        open(&self.meta, &what.aad(), sealed).map_err(|()| Error::Corrupt(what.what()))
    }
}

/// Bytes that sealing adds to each chunk or record.
pub const OVERHEAD: usize = NONCE + 16;

#[cfg(test)]
mod tests {
    use super::*;

    const CHEAP: KdfParams = KdfParams {
        memory_kib: 64,
        iterations: 1,
        lanes: 1,
    };

    #[test]
    fn passphrase_wraps_the_key() {
        let (info, key) = KeyInfo::create_with("correct horse", CHEAP).unwrap();
        // It round-trips through volume.json.
        let info: KeyInfo = serde_json::from_str(&serde_json::to_string(&info).unwrap()).unwrap();
        assert_eq!(info.unlock("correct horse").unwrap(), key);
        assert!(matches!(info.unlock("wrong horse"), Err(Error::WrongPassphrase)));
        // A new passphrase keeps the key.
        let again = KeyInfo::wrap(&key, "battery staple", CHEAP).unwrap();
        assert_eq!(again.unlock("battery staple").unwrap(), key);
        assert!(KeyInfo::create_with("", CHEAP).is_err());
        assert_eq!(VolumeKey::from_hex(&key.to_hex()).unwrap(), key);
    }

    #[test]
    fn chunks_and_metadata_round_trip_and_stay_apart() {
        let a = Cipher::new(&VolumeKey::generate());
        let b = Cipher::new(&VolumeKey::generate());
        let data = b"some file content";
        let id = a.chunk_hash(data);
        assert_eq!(id, a.chunk_hash(data), "same content, same id: dedupe works");
        assert_ne!(id, b.chunk_hash(data), "another volume's ids differ");
        assert_ne!(id.0, *blake3::hash(data).as_bytes(), "not the plain hash");

        let sealed = a.seal_chunk(&id, data);
        assert_eq!(sealed.len(), data.len() + OVERHEAD);
        assert_ne!(sealed, a.seal_chunk(&id, data), "random nonces");
        assert_eq!(a.open_chunk(&id, &sealed).unwrap(), data);
        assert!(b.open_chunk(&id, &sealed).is_err());
        assert!(
            a.open_chunk(&a.chunk_hash(b"other"), &sealed).is_err(),
            "bound to its id"
        );
        let mut bad = sealed.clone();
        *bad.last_mut().unwrap() ^= 1;
        assert!(a.open_chunk(&id, &bad).is_err());

        let cs = Meta::Changeset {
            version: [7; 16],
            local: 3,
        };
        let m = a.seal_meta(cs, b"ops");
        assert_eq!(a.open_meta(cs, &m).unwrap(), b"ops");
        let other = Meta::Changeset {
            version: [7; 16],
            local: 4,
        };
        assert!(a.open_meta(other, &m).is_err(), "bound to its changeset");
        assert!(a.open_meta(Meta::Snapshot { seq: 3 }, &m).is_err());
    }
}
