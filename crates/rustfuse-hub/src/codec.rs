//! Encoding shared by the bucket log and the wire protocol: postcard,
//! compressed with zstd.

use bytes::Bytes;
use serde::{Serialize, de::DeserializeOwned};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::{Error, Result};

/// Largest frame accepted on the wire.
pub const MAX_FRAME: usize = 1 << 30;

pub fn encode<T: Serialize>(v: &T) -> Bytes {
    let raw = postcard::to_allocvec(v).expect("serialization cannot fail");
    Bytes::from(zstd::bulk::compress(&raw, 3).expect("zstd compression cannot fail"))
}

pub fn decode<T: DeserializeOwned>(what: &str, data: &[u8]) -> Result<T> {
    let raw = zstd::stream::decode_all(data).map_err(|e| Error::Corrupt(format!("{what}: {e}")))?;
    postcard::from_bytes(&raw).map_err(|e| Error::Corrupt(format!("{what}: {e}")))
}

pub async fn write_frame<W: AsyncWrite + Unpin, T: Serialize>(w: &mut W, msg: &T) -> Result<()> {
    let data = encode(msg);
    w.write_u32(data.len() as u32).await?;
    w.write_all(&data).await?;
    w.flush().await?;
    Ok(())
}

/// Reads one frame; `None` on a clean end of stream. Not cancel-safe.
pub async fn read_frame<R: AsyncRead + Unpin, T: DeserializeOwned>(r: &mut R) -> Result<Option<T>> {
    let Some(buf) = read_raw(r).await? else {
        return Ok(None);
    };
    decode("frame", &buf)
        .map(Some)
        .map_err(|e| Error::Protocol(e.to_string()))
}

/// A whole frame (length included) of plain postcard, for messages that
/// carry already-compressed data. Write it with `write_all`.
pub fn encode_plain<T: Serialize>(v: &T) -> Bytes {
    let mut out = postcard::to_extend(v, vec![0u8; 4]).expect("serialization cannot fail");
    let len = (out.len() - 4) as u32;
    out[..4].copy_from_slice(&len.to_be_bytes());
    Bytes::from(out)
}

/// Reads one plain postcard frame (see [`encode_plain`]).
pub async fn read_plain<R: AsyncRead + Unpin, T: DeserializeOwned>(r: &mut R) -> Result<Option<T>> {
    let Some(buf) = read_raw(r).await? else {
        return Ok(None);
    };
    postcard::from_bytes(&buf)
        .map(Some)
        .map_err(|e| Error::Protocol(format!("frame: {e}")))
}

async fn read_raw<R: AsyncRead + Unpin>(r: &mut R) -> Result<Option<Vec<u8>>> {
    let len = match r.read_u32().await {
        Ok(n) => n as usize,
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    if len > MAX_FRAME {
        return Err(Error::Protocol(format!("frame of {len} bytes is too large")));
    }
    let mut buf = vec![0; len];
    r.read_exact(&mut buf).await?;
    Ok(Some(buf))
}
