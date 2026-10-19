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
    decode("frame", &buf)
        .map(Some)
        .map_err(|e| Error::Protocol(e.to_string()))
}
