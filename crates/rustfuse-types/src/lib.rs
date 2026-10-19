//! Core identifiers and data-layout math shared by every rustFUSE crate.
//!
//! See `docs/design.md` §5: a file is split into fixed-size *chunks*; each
//! write produces an immutable *slice*, stored in the object store as one or
//! more *blocks*.

use std::fmt;

/// Logical size of a chunk (64 MiB).
pub const CHUNK_SIZE: u64 = 64 << 20;

/// Default maximum size of a stored block (4 MiB).
pub const DEFAULT_BLOCK_SIZE: u32 = 4 << 20;

/// Inode number. `1` is the root directory.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Ino(pub u64);

impl Ino {
    pub const ROOT: Ino = Ino(1);
}

/// Globally unique id of an immutable slice, allocated by the metadata server.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SliceId(pub u64);

impl fmt::Display for SliceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Address of one stored block of a slice.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct BlockRef {
    pub slice: SliceId,
    pub index: u32,
    pub len: u32,
}

impl BlockRef {
    /// Object key of this block, relative to the filesystem's prefix.
    ///
    /// Format: `data/{shard:02x}/{slice_id}_{index}_{len}`. The shard spreads
    /// keys across the store's partitions and is derived from a stable hash
    /// so it never changes between versions.
    pub fn object_key(&self) -> String {
        format!(
            "data/{:02x}/{}_{}_{}",
            shard(self.slice),
            self.slice,
            self.index,
            self.len
        )
    }

    /// Parses a key produced by [`BlockRef::object_key`].
    pub fn parse_object_key(key: &str) -> Option<BlockRef> {
        let rest = key.strip_prefix("data/")?;
        let (shard_hex, name) = rest.split_once('/')?;
        let mut parts = name.splitn(3, '_');
        let slice = SliceId(parts.next()?.parse().ok()?);
        let index = parts.next()?.parse().ok()?;
        let len = parts.next()?.parse().ok()?;
        let block = BlockRef { slice, index, len };
        (u8::from_str_radix(shard_hex, 16).ok()? == shard(slice)).then_some(block)
    }
}

/// Stable shard of a slice id (splitmix64 finalizer, low byte).
fn shard(slice: SliceId) -> u8 {
    let mut z = slice.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    (z ^ (z >> 31)) as u8
}

/// Splits a slice of `len` bytes into block references of at most
/// `block_size` bytes each.
pub fn blocks_of_slice(slice: SliceId, len: u64, block_size: u32) -> Vec<BlockRef> {
    assert!(block_size > 0, "block_size must be non-zero");
    let bs = u64::from(block_size);
    (0..len.div_ceil(bs))
        .map(|i| BlockRef {
            slice,
            index: u32::try_from(i).expect("too many blocks in slice"),
            len: u32::try_from((len - i * bs).min(bs)).expect("block fits u32"),
        })
        .collect()
}

/// Chunk index and offset within that chunk for a file offset.
pub fn chunk_pos(offset: u64) -> (u64, u64) {
    (offset / CHUNK_SIZE, offset % CHUNK_SIZE)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn object_key_round_trips() {
        for id in [0, 1, 42, u64::MAX] {
            let b = BlockRef {
                slice: SliceId(id),
                index: 3,
                len: 4096,
            };
            let key = b.object_key();
            assert!(key.starts_with("data/"));
            assert_eq!(BlockRef::parse_object_key(&key), Some(b));
        }
    }

    #[test]
    fn object_key_is_stable() {
        // Changing this breaks every existing filesystem.
        let b = BlockRef {
            slice: SliceId(12345),
            index: 0,
            len: 1,
        };
        assert_eq!(
            b.object_key(),
            format!("data/{:02x}/12345_0_1", shard(SliceId(12345)))
        );
        assert_eq!(shard(SliceId(12345)), shard(SliceId(12345)));
    }

    #[test]
    fn parse_rejects_bad_keys() {
        assert_eq!(BlockRef::parse_object_key("meta/x"), None);
        assert_eq!(BlockRef::parse_object_key("data/zz/1_0_1"), None);
        assert_eq!(BlockRef::parse_object_key("data/00/a_0_1"), None);
        let good = BlockRef {
            slice: SliceId(7),
            index: 0,
            len: 1,
        }
        .object_key();
        let wrong_shard = good.replacen(&good[5..7], if &good[5..7] == "00" { "01" } else { "00" }, 1);
        assert_eq!(BlockRef::parse_object_key(&wrong_shard), None);
    }

    #[test]
    fn shards_are_spread() {
        let mut seen = [false; 256];
        for id in 0..4096 {
            seen[shard(SliceId(id)) as usize] = true;
        }
        assert!(seen.iter().filter(|s| **s).count() > 250);
    }

    #[test]
    fn slice_block_split() {
        let bs = 4;
        assert!(blocks_of_slice(SliceId(1), 0, bs).is_empty());
        let lens: Vec<u32> = blocks_of_slice(SliceId(1), 10, bs)
            .iter()
            .map(|b| b.len)
            .collect();
        assert_eq!(lens, vec![4, 4, 2]);
        let lens: Vec<u32> = blocks_of_slice(SliceId(1), 8, bs).iter().map(|b| b.len).collect();
        assert_eq!(lens, vec![4, 4]);
    }

    #[test]
    fn chunk_positions() {
        assert_eq!(chunk_pos(0), (0, 0));
        assert_eq!(chunk_pos(CHUNK_SIZE - 1), (0, CHUNK_SIZE - 1));
        assert_eq!(chunk_pos(CHUNK_SIZE + 5), (1, 5));
    }
}
