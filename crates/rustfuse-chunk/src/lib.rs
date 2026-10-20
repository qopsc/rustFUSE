//! Content-defined chunking, pack building and chunk fetching
//! (`docs/design.md` §5.3).
//!
//! Files are cut into variable-size chunks with FastCDC so that an edit only
//! changes the chunks around it, hashed with BLAKE3 for deduplication, and
//! batched into immutable pack objects so a large tree does not become one
//! S3 object per file.

use std::collections::HashMap;
use std::io::Read;
use std::sync::Arc;

use bytes::Bytes;
use futures::{StreamExt, TryStreamExt, stream};
use rustfuse_store::ObjectStore;
use rustfuse_types::{ChunkHash, ChunkRef, Codec, PackId};

pub const MIN_CHUNK: usize = 16 << 10;
pub const AVG_CHUNK: usize = 64 << 10;
pub const MAX_CHUNK: usize = 256 << 10;

/// Packs are sealed once they reach this size.
pub const PACK_TARGET: usize = 16 << 20;

/// Ranged reads closer than this are merged into one request.
const COALESCE_GAP: u64 = 256 << 10;
/// Upper bound on one merged ranged read.
const MAX_RANGE: u64 = 8 << 20;
const FETCH_CONCURRENCY: usize = 32;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Store(#[from] rustfuse_store::Error),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("corrupt chunk {0:?}: {1}")]
    Corrupt(ChunkHash, String),
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Forwarding hops followed before giving up (a pack compacted again
/// after its chunks moved).
const MAX_HOPS: usize = 8;

/// Where the chunks of a compacted pack went, by their old offset. Stored
/// as `fwd/<pack>` before the pack is deleted, so references written
/// earlier (in the log, snapshots, unsent changesets) still resolve.
pub type Forward = HashMap<u64, ChunkRef>;

pub fn forward_key(pack: PackId) -> String {
    format!("fwd/{:032x}", pack.0)
}

pub fn encode_forward(f: &Forward) -> Bytes {
    let mut entries: Vec<(&u64, &ChunkRef)> = f.iter().collect();
    entries.sort_by_key(|(o, _)| **o);
    Bytes::from(postcard::to_allocvec(&entries).expect("serialization cannot fail"))
}

/// The forwarding table of `pack`, if it was compacted.
pub async fn load_forward(store: &dyn ObjectStore, pack: PackId) -> Result<Option<Forward>> {
    let raw = match store.get(&forward_key(pack)).await {
        Ok(raw) => raw,
        Err(rustfuse_store::Error::NotFound(_)) => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let entries: Vec<(u64, ChunkRef)> = postcard::from_bytes(&raw).map_err(|e| {
        Error::Store(rustfuse_store::Error::Backend(
            format!("{}: {e}", forward_key(pack)).into(),
        ))
    })?;
    Ok(Some(entries.into_iter().collect()))
}

/// A new, random pack id (for packs written by cleanup).
pub fn random_pack_id() -> PackId {
    let mut b = [0u8; 16];
    getrandom::fill(&mut b).expect("no system randomness");
    PackId(u128::from_le_bytes(b))
}

pub fn hash(data: &[u8]) -> ChunkHash {
    ChunkHash(*blake3::hash(data).as_bytes())
}

/// Splits a stream into content-defined chunks, calling `f` for each.
pub fn chunk_reader<R: Read>(reader: R, mut f: impl FnMut(&[u8]) -> Result<()>) -> Result<()> {
    let cdc = fastcdc::v2020::StreamCDC::new(reader, MIN_CHUNK, AVG_CHUNK, MAX_CHUNK);
    for chunk in cdc {
        let chunk = chunk.map_err(|e| match e {
            fastcdc::v2020::Error::IoError(e) => Error::Io(e),
            other => Error::Io(std::io::Error::other(other.to_string())),
        })?;
        f(&chunk.data)?;
    }
    Ok(())
}

/// A chunk ready to be appended to a pack: hashed and, when it helps,
/// compressed. Preparing is CPU work done outside any shared lock.
pub struct Prepared {
    pub hash: ChunkHash,
    pub len: u32,
    pub codec: Codec,
    pub stored: Vec<u8>,
}

pub fn prepare(hash: ChunkHash, data: &[u8]) -> Prepared {
    let len = u32::try_from(data.len()).expect("chunk larger than 4 GiB");
    match zstd::bulk::compress(data, 3) {
        // Only keep compression when it saves at least ~10%.
        Ok(z) if z.len() < data.len() - data.len() / 10 => Prepared {
            hash,
            len,
            codec: Codec::Zstd,
            stored: z,
        },
        _ => Prepared {
            hash,
            len,
            codec: Codec::Raw,
            stored: data.to_vec(),
        },
    }
}

/// A finished pack, ready to upload.
pub struct SealedPack {
    pub id: PackId,
    pub data: Bytes,
}

/// Accumulates chunks into packs and remembers every chunk it has placed,
/// so identical content is stored once.
pub struct PackBuilder {
    seed: [u8; 16],
    /// Random per builder: a restarted process must never reuse a pack id
    /// (that would overwrite a pack other files still point into).
    nonce: [u8; 16],
    counter: u64,
    current: PackId,
    buf: Vec<u8>,
    known: HashMap<ChunkHash, ChunkRef>,
}

impl PackBuilder {
    /// `seed` must be unique per writer (e.g. random per replica) so pack ids
    /// from different machines never collide.
    pub fn new(seed: [u8; 16]) -> Self {
        let mut nonce = [0u8; 16];
        getrandom::fill(&mut nonce).expect("no system randomness");
        let mut b = PackBuilder {
            seed,
            nonce,
            counter: 0,
            current: PackId(0),
            buf: Vec::new(),
            known: HashMap::new(),
        };
        b.current = b.next_id();
        b
    }

    fn next_id(&mut self) -> PackId {
        let mut h = blake3::Hasher::new();
        h.update(&self.seed);
        h.update(&self.nonce);
        h.update(&self.counter.to_le_bytes());
        self.counter += 1;
        let bytes: [u8; 16] = h.finalize().as_bytes()[..16].try_into().unwrap();
        PackId(u128::from_le_bytes(bytes))
    }

    /// Registers a chunk that already exists in the store.
    pub fn insert_known(&mut self, r: ChunkRef) {
        self.known.insert(r.hash, r);
    }

    pub fn lookup(&self, hash: &ChunkHash) -> Option<ChunkRef> {
        self.known.get(hash).copied()
    }

    /// Places a prepared chunk (deduplicating) and returns where it lives,
    /// plus a sealed pack when the current one is full. The returned ref is
    /// only readable once that pack has been uploaded.
    pub fn add(&mut self, p: Prepared) -> (ChunkRef, Option<SealedPack>) {
        if let Some(r) = self.lookup(&p.hash) {
            return (r, None);
        }
        let r = ChunkRef {
            hash: p.hash,
            len: p.len,
            pack: self.current,
            offset: self.buf.len() as u64,
            stored_len: p.stored.len() as u32,
            codec: p.codec,
        };
        self.buf.extend_from_slice(&p.stored);
        self.known.insert(r.hash, r);
        let sealed = (self.buf.len() >= PACK_TARGET).then(|| self.seal()).flatten();
        (r, sealed)
    }

    /// Seals the current pack if it has any data.
    pub fn seal(&mut self) -> Option<SealedPack> {
        if self.buf.is_empty() {
            return None;
        }
        let pack = SealedPack {
            id: self.current,
            data: Bytes::from(std::mem::take(&mut self.buf)),
        };
        self.current = self.next_id();
        Some(pack)
    }

    /// Forgets the chunks stored in `packs` (they were deleted), so new
    /// content with the same bytes is stored again. Returns their hashes.
    pub fn forget_packs(&mut self, packs: &std::collections::HashSet<PackId>) -> Vec<ChunkHash> {
        let gone: Vec<ChunkHash> = self
            .known
            .values()
            .filter(|r| packs.contains(&r.pack))
            .map(|r| r.hash)
            .collect();
        for h in &gone {
            self.known.remove(h);
        }
        gone
    }

    /// Every chunk this builder knows about.
    pub fn known(&self) -> impl Iterator<Item = &ChunkRef> {
        self.known.values()
    }
}

pub async fn upload_pack(store: &dyn ObjectStore, pack: SealedPack) -> Result<()> {
    store.put(&pack.id.object_key(), pack.data).await?;
    Ok(())
}

/// Decodes and verifies one chunk from its stored bytes.
pub fn decode(r: &ChunkRef, stored: &[u8]) -> Result<Bytes> {
    let data = match r.codec {
        Codec::Raw => Bytes::copy_from_slice(stored),
        Codec::Zstd => Bytes::from(
            zstd::bulk::decompress(stored, r.len as usize)
                .map_err(|e| Error::Corrupt(r.hash, e.to_string()))?,
        ),
    };
    if data.len() != r.len as usize || hash(&data) != r.hash {
        return Err(Error::Corrupt(r.hash, "hash mismatch".into()));
    }
    Ok(data)
}

/// Fetches and verifies chunks, merging nearby reads from the same pack
/// into single ranged requests. Chunks of a compacted pack are read from
/// where they moved ([`Forward`]).
pub async fn fetch(store: Arc<dyn ObjectStore>, refs: &[ChunkRef]) -> Result<HashMap<ChunkHash, Bytes>> {
    let mut out = HashMap::new();
    let mut todo = refs.to_vec();
    for _ in 0..=MAX_HOPS {
        let (got, gone) = fetch_once(store.clone(), &todo).await?;
        out.extend(got);
        if gone.is_empty() {
            return Ok(out);
        }
        todo.clear();
        for (pack, members) in gone {
            let Some(fwd) = load_forward(store.as_ref(), pack).await? else {
                return Err(rustfuse_store::Error::NotFound(pack.object_key()).into());
            };
            for c in members {
                match fwd.get(&c.offset) {
                    Some(n) if n.hash == c.hash => todo.push(*n),
                    _ => return Err(rustfuse_store::Error::NotFound(pack.object_key()).into()),
                }
            }
        }
    }
    Err(Error::Store(rustfuse_store::Error::Backend(
        "too many forwarding hops".into(),
    )))
}

/// Chunks fetched, and those whose pack is gone (by pack).
type Fetched = (HashMap<ChunkHash, Bytes>, Vec<(PackId, Vec<ChunkRef>)>);
/// One ranged read: its chunks, or the pack and chunks it found gone.
type RangeRead = Result<Vec<(ChunkHash, Bytes)>, (PackId, Vec<ChunkRef>)>;

async fn fetch_once(store: Arc<dyn ObjectStore>, refs: &[ChunkRef]) -> Result<Fetched> {
    let mut by_pack: HashMap<PackId, Vec<ChunkRef>> = HashMap::new();
    for r in refs {
        by_pack.entry(r.pack).or_default().push(*r);
    }
    let mut ranges: Vec<(PackId, u64, u64, Vec<ChunkRef>)> = Vec::new();
    for (pack, mut chunks) in by_pack {
        chunks.sort_by_key(|c| c.offset);
        chunks.dedup_by_key(|c| c.hash);
        for c in chunks {
            let end = c.offset + u64::from(c.stored_len);
            match ranges.last_mut() {
                Some((p, start, last_end, members))
                    if *p == pack && c.offset <= *last_end + COALESCE_GAP && end - *start <= MAX_RANGE =>
                {
                    *last_end = (*last_end).max(end);
                    members.push(c);
                }
                _ => ranges.push((pack, c.offset, end, vec![c])),
            }
        }
    }

    let results: Vec<RangeRead> = stream::iter(ranges)
        .map(|(pack, start, end, members)| {
            let store = store.clone();
            async move {
                let data = match store.get_range(&pack.object_key(), start..end).await {
                    Ok(d) => d,
                    Err(rustfuse_store::Error::NotFound(_)) => return Ok(Err((pack, members))),
                    Err(e) => return Err(e.into()),
                };
                let mut out = Vec::with_capacity(members.len());
                for c in members {
                    let lo = (c.offset - start) as usize;
                    let hi = lo + c.stored_len as usize;
                    let stored = data
                        .get(lo..hi)
                        .ok_or_else(|| Error::Corrupt(c.hash, "pack too short".into()))?;
                    out.push((c.hash, decode(&c, stored)?));
                }
                Ok::<_, Error>(Ok(out))
            }
        })
        .buffer_unordered(FETCH_CONCURRENCY)
        .try_collect()
        .await?;
    let mut got = HashMap::new();
    let mut gone: HashMap<PackId, Vec<ChunkRef>> = HashMap::new();
    for r in results {
        match r {
            Ok(chunks) => got.extend(chunks),
            Err((pack, members)) => gone.entry(pack).or_default().extend(members),
        }
    }
    Ok((got, gone.into_iter().collect()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustfuse_store::MemoryStore;

    fn pseudo_random(n: usize, seed: u64) -> Vec<u8> {
        let mut x = seed | 1;
        (0..n)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                x as u8
            })
            .collect()
    }

    fn chunks_of(data: &[u8]) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        chunk_reader(data, |c| {
            out.push(c.to_vec());
            Ok(())
        })
        .unwrap();
        out
    }

    #[test]
    fn chunking_is_content_defined() {
        let data = pseudo_random(2 << 20, 42);
        let a = chunks_of(&data);
        assert!(a.len() > 10);
        assert!(a.iter().all(|c| c.len() <= MAX_CHUNK));
        assert_eq!(a.concat(), data);

        // Inserting bytes near the start only disturbs nearby chunks.
        let mut edited = data[..1000].to_vec();
        edited.extend_from_slice(b"inserted");
        edited.extend_from_slice(&data[1000..]);
        let b = chunks_of(&edited);
        let hashes_a: std::collections::HashSet<_> = a.iter().map(|c| hash(c)).collect();
        let shared = b.iter().filter(|c| hashes_a.contains(&hash(c))).count();
        assert!(
            shared >= b.len() - 3,
            "only {shared} of {} chunks shared",
            b.len()
        );
    }

    #[tokio::test]
    async fn pack_round_trip_with_dedupe() {
        let store: Arc<dyn ObjectStore> = Arc::new(MemoryStore::new());
        let mut builder = PackBuilder::new([7; 16]);
        let compressible = vec![b'a'; 100_000];
        let random = pseudo_random(300_000, 9);
        let mut refs = Vec::new();
        for data in [&compressible, &random, &compressible] {
            for c in chunks_of(data) {
                let (r, sealed) = builder.add(prepare(hash(&c), &c));
                assert!(sealed.is_none());
                refs.push(r);
            }
        }
        let first_len = chunks_of(&compressible).len();
        assert_eq!(
            refs[0],
            refs[refs.len() - first_len],
            "duplicate content stored twice"
        );
        assert_eq!(refs[0].codec, Codec::Zstd);

        upload_pack(store.as_ref(), builder.seal().unwrap())
            .await
            .unwrap();
        let got = fetch(store, &refs).await.unwrap();
        let rebuilt: Vec<u8> = refs.iter().flat_map(|r| got[&r.hash].to_vec()).collect();
        let mut expected = compressible.clone();
        expected.extend_from_slice(&random);
        expected.extend_from_slice(&compressible);
        assert_eq!(rebuilt, expected);
    }

    #[tokio::test]
    async fn corruption_is_detected() {
        let store: Arc<dyn ObjectStore> = Arc::new(MemoryStore::new());
        let mut builder = PackBuilder::new([1; 16]);
        let data = pseudo_random(1000, 3);
        let (r, _) = builder.add(prepare(hash(&data), &data));
        let mut pack = builder.seal().unwrap();
        let mut bytes = pack.data.to_vec();
        bytes[10] ^= 0xff;
        pack.data = Bytes::from(bytes);
        upload_pack(store.as_ref(), pack).await.unwrap();
        assert!(matches!(fetch(store, &[r]).await, Err(Error::Corrupt(..))));
    }

    #[test]
    fn packs_seal_at_target() {
        let mut builder = PackBuilder::new([2; 16]);
        let mut sealed = 0;
        for i in 0..400 {
            let data = pseudo_random(MAX_CHUNK, i * 2 + 1);
            if builder.add(prepare(hash(&data), &data)).1.is_some() {
                sealed += 1;
            }
        }
        assert!(sealed >= 5);
    }

    #[test]
    fn restarted_builders_never_reuse_pack_ids() {
        let data = pseudo_random(1000, 4);
        let ids: Vec<PackId> = (0..2)
            .map(|_| {
                let mut b = PackBuilder::new([3; 16]);
                b.add(prepare(hash(&data), &data));
                b.seal().unwrap().id
            })
            .collect();
        assert_ne!(ids[0], ids[1]);
    }
}
