//! The volume's objects, read and written through the hub.

use std::collections::HashMap;
use std::ops::Range;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use bytes::Bytes;
use rustfuse_store::{ListPage, ObjectMeta, ObjectStore};
use tokio::io::AsyncWriteExt;
use tokio::sync::{mpsc, oneshot};
use tracing::debug;

use crate::client::connect_stream;
use crate::codec::{encode_plain, read_frame, read_plain, write_frame};
use crate::server::HubMetrics;
use crate::{Error, ObjectError, ObjectOp, ObjectReply, ObjectRequest, ObjectResponse, Request, Response};

/// Frames queued for the writer task of one connection.
const QUEUE: usize = 64;

type Reply = std::result::Result<ObjectReply, ObjectError>;
/// Calls waiting for a reply; `None` once the connection is gone.
type Pending = Arc<Mutex<Option<HashMap<u64, oneshot::Sender<Reply>>>>>;

/// An [`ObjectStore`] for one volume that goes through the hub, for
/// machines that cannot reach the bucket. Calls share one connection and
/// run concurrently; a lost connection is reopened on the next call, and a
/// call cut off by it is retried once (every call is idempotent).
pub struct HubStore {
    addr: String,
    tls: Option<Arc<rustls::ClientConfig>>,
    volume: String,
    token: String,
    conn: tokio::sync::Mutex<Option<Arc<Conn>>>,
}

struct Conn {
    frames: mpsc::Sender<Bytes>,
    pending: Pending,
    next: AtomicU64,
}

impl Conn {
    fn alive(&self) -> bool {
        !self.frames.is_closed() && self.pending.lock().unwrap().is_some()
    }
}

impl HubStore {
    /// Opens a storage connection and checks the token, so a wrong address
    /// or token shows up here rather than on first use.
    pub async fn connect(
        addr: &str,
        tls: Option<Arc<rustls::ClientConfig>>,
        volume: &str,
        token: &str,
    ) -> crate::Result<HubStore> {
        let store = HubStore {
            addr: addr.to_owned(),
            tls,
            volume: volume.to_owned(),
            token: token.to_owned(),
            conn: tokio::sync::Mutex::new(None),
        };
        store.conn().await?;
        Ok(store)
    }

    async fn conn(&self) -> crate::Result<Arc<Conn>> {
        let mut slot = self.conn.lock().await;
        if let Some(c) = slot.as_ref().filter(|c| c.alive()) {
            return Ok(c.clone());
        }
        let c = Arc::new(self.open().await?);
        *slot = Some(c.clone());
        Ok(c)
    }

    async fn open(&self) -> crate::Result<Conn> {
        let (mut rd, mut wr) = connect_stream(&self.addr, self.tls.as_ref()).await?;
        write_frame(
            &mut wr,
            &Request::Store {
                volume: self.volume.clone(),
                token: self.token.clone(),
            },
        )
        .await?;
        match read_frame(&mut rd).await? {
            Some(Response::StoreReady) => {}
            Some(Response::Error(e)) => return Err(Error::Refused(e)),
            Some(other) => return Err(Error::Protocol(format!("unexpected {other:?}"))),
            None => {
                return Err(Error::Protocol(
                    "hub closed the connection (is it older than this machine's rustfuse?)".into(),
                ));
            }
        }
        debug!(addr = self.addr, volume = self.volume, "storage connection open");

        // Frames go out from one task so a cancelled call never leaves half
        // a frame on the wire.
        let (frames, mut queue) = mpsc::channel::<Bytes>(QUEUE);
        tokio::spawn(async move {
            while let Some(f) = queue.recv().await {
                if wr.write_all(&f).await.is_err() || (queue.is_empty() && wr.flush().await.is_err()) {
                    return;
                }
            }
        });
        let pending: Pending = Arc::new(Mutex::new(Some(HashMap::new())));
        let reader_pending = pending.clone();
        tokio::spawn(async move {
            while let Ok(Some(r)) = read_plain::<_, ObjectResponse>(&mut rd).await {
                let tx = reader_pending
                    .lock()
                    .unwrap()
                    .as_mut()
                    .and_then(|p| p.remove(&r.id));
                if let Some(tx) = tx {
                    let _ = tx.send(r.result);
                }
            }
            // Dropping the senders fails every call still waiting.
            reader_pending.lock().unwrap().take();
        });
        Ok(Conn {
            frames,
            pending,
            next: AtomicU64::new(1),
        })
    }

    async fn call(&self, op: impl Fn() -> ObjectOp) -> rustfuse_store::Result<ObjectReply> {
        let mut retried = false;
        loop {
            match self.try_call(op()).await {
                Ok(Ok(reply)) => return Ok(reply),
                Ok(Err(ObjectError::NotFound(k))) => return Err(rustfuse_store::Error::NotFound(k)),
                Ok(Err(ObjectError::Other(e))) => return Err(backend(Error::Refused(e))),
                Err(e) if !retried => {
                    debug!("storage call failed, reconnecting: {e}");
                    retried = true;
                }
                Err(e) => return Err(backend(e)),
            }
        }
    }

    async fn try_call(&self, op: ObjectOp) -> crate::Result<Reply> {
        let conn = self.conn().await?;
        let id = conn.next.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        match conn.pending.lock().unwrap().as_mut() {
            Some(p) => p.insert(id, tx),
            None => return Err(Error::Protocol("hub storage connection closed".into())),
        };
        let frame = encode_plain(&ObjectRequest { id, op });
        if conn.frames.send(frame).await.is_err() {
            return Err(Error::Protocol("hub storage connection closed".into()));
        }
        rx.await
            .map_err(|_| Error::Protocol("hub storage connection closed".into()))
    }
}

fn to_millis(t: SystemTime) -> u64 {
    t.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
}

fn from_millis(ms: u64) -> SystemTime {
    UNIX_EPOCH + Duration::from_millis(ms)
}

fn backend(e: Error) -> rustfuse_store::Error {
    rustfuse_store::Error::Backend(Box::new(e))
}

fn unexpected(r: ObjectReply) -> rustfuse_store::Error {
    backend(Error::Protocol(format!("unexpected storage reply {r:?}")))
}

#[async_trait]
impl ObjectStore for HubStore {
    async fn get(&self, key: &str) -> rustfuse_store::Result<Bytes> {
        match self.call(|| ObjectOp::Get { key: key.to_owned() }).await? {
            ObjectReply::Data(d) => Ok(d),
            r => Err(unexpected(r)),
        }
    }

    async fn get_range(&self, key: &str, range: Range<u64>) -> rustfuse_store::Result<Bytes> {
        if range.is_empty() {
            return Ok(Bytes::new());
        }
        let op = || ObjectOp::GetRange {
            key: key.to_owned(),
            start: range.start,
            end: range.end,
        };
        match self.call(op).await? {
            ObjectReply::Data(d) => Ok(d),
            r => Err(unexpected(r)),
        }
    }

    async fn put(&self, key: &str, data: Bytes) -> rustfuse_store::Result<()> {
        let op = || ObjectOp::Put {
            key: key.to_owned(),
            data: data.clone(),
        };
        match self.call(op).await? {
            ObjectReply::Done => Ok(()),
            r => Err(unexpected(r)),
        }
    }

    /// Atomic on the hub's store. A call retried after a lost connection
    /// may report `false` for its own first write.
    async fn put_if_absent(&self, key: &str, data: Bytes) -> rustfuse_store::Result<bool> {
        let op = || ObjectOp::PutIfAbsent {
            key: key.to_owned(),
            data: data.clone(),
        };
        match self.call(op).await? {
            ObjectReply::Written(w) => Ok(w),
            r => Err(unexpected(r)),
        }
    }

    async fn head(&self, key: &str) -> rustfuse_store::Result<Option<ObjectMeta>> {
        match self.call(|| ObjectOp::Head { key: key.to_owned() }).await? {
            ObjectReply::Head(meta) => Ok(meta.map(|(size, modified)| ObjectMeta {
                key: key.to_owned(),
                size,
                modified: modified.map(from_millis),
            })),
            r => Err(unexpected(r)),
        }
    }

    async fn delete(&self, key: &str) -> rustfuse_store::Result<()> {
        match self.call(|| ObjectOp::Delete { key: key.to_owned() }).await? {
            ObjectReply::Done => Ok(()),
            r => Err(unexpected(r)),
        }
    }

    async fn list(
        &self,
        prefix: &str,
        start_after: Option<&str>,
        limit: usize,
    ) -> rustfuse_store::Result<ListPage> {
        let op = || ObjectOp::List {
            prefix: prefix.to_owned(),
            start_after: start_after.map(str::to_owned),
            limit: limit.min(u32::MAX as usize) as u32,
        };
        match self.call(op).await? {
            ObjectReply::List {
                objects,
                next_start_after,
            } => Ok(ListPage {
                objects: objects
                    .into_iter()
                    .map(|(key, size, modified)| ObjectMeta {
                        key,
                        size,
                        modified: modified.map(from_millis),
                    })
                    .collect(),
                next_start_after,
            }),
            r => Err(unexpected(r)),
        }
    }
}

/// Serves one storage connection on the hub, after the `Store` hello.
pub(crate) async fn serve<R, W>(
    store: Arc<dyn ObjectStore>,
    mut rd: R,
    mut wr: W,
    metrics: Arc<HubMetrics>,
) -> crate::Result<()>
where
    R: tokio::io::AsyncRead + Send + Unpin + 'static,
    W: tokio::io::AsyncWrite + Send + Unpin + 'static,
{
    write_frame(&mut wr, &Response::StoreReady).await?;
    let (frames, mut queue) = mpsc::channel::<Bytes>(QUEUE);
    let writer = tokio::spawn(async move {
        while let Some(f) = queue.recv().await {
            wr.write_all(&f).await?;
            if queue.is_empty() {
                wr.flush().await?;
            }
        }
        Ok::<_, std::io::Error>(())
    });
    // Bounds the calls (and so the memory) in flight per connection.
    let permits = Arc::new(tokio::sync::Semaphore::new(QUEUE));
    while let Some(req) = read_plain::<_, ObjectRequest>(&mut rd).await? {
        let permit = permits.clone().acquire_owned().await.expect("never closed");
        let store = store.clone();
        let frames = frames.clone();
        let metrics = metrics.clone();
        tokio::spawn(async move {
            let op = op_name(&req.op);
            if let ObjectOp::Put { data, .. } | ObjectOp::PutIfAbsent { data, .. } = &req.op {
                metrics.store_bytes_total.get("in").add(data.len() as u64);
            }
            let t0 = Instant::now();
            let result = run(&*store, req.op).await;
            metrics.store_seconds.get(op).observe(t0.elapsed());
            metrics.store_requests_total.get(op).inc();
            match &result {
                Ok(ObjectReply::Data(d)) => metrics.store_bytes_total.get("out").add(d.len() as u64),
                Err(ObjectError::Other(_)) => metrics.store_errors_total.get(op).inc(),
                _ => {}
            }
            let _ = frames
                .send(encode_plain(&ObjectResponse { id: req.id, result }))
                .await;
            drop(permit);
        });
    }
    drop(frames);
    let _ = writer.await;
    Ok(())
}

fn op_name(op: &ObjectOp) -> &'static str {
    match op {
        ObjectOp::Get { .. } => "get",
        ObjectOp::GetRange { .. } => "get_range",
        ObjectOp::Put { .. } => "put",
        ObjectOp::PutIfAbsent { .. } => "put_if_absent",
        ObjectOp::Head { .. } => "head",
        ObjectOp::Delete { .. } => "delete",
        ObjectOp::List { .. } => "list",
    }
}

/// Keys must stay inside the volume even on a store that would resolve
/// `..` or a leading `/`.
fn contained(key: &str) -> bool {
    !key.starts_with('/') && !key.split('/').any(|seg| seg == "." || seg == "..")
}

async fn run(store: &dyn ObjectStore, op: ObjectOp) -> Reply {
    let keys: &[Option<&str>] = match &op {
        ObjectOp::Get { key }
        | ObjectOp::GetRange { key, .. }
        | ObjectOp::Put { key, .. }
        | ObjectOp::PutIfAbsent { key, .. }
        | ObjectOp::Head { key }
        | ObjectOp::Delete { key } => &[Some(key.as_str())],
        ObjectOp::List {
            prefix, start_after, ..
        } => &[Some(prefix.as_str()), start_after.as_deref()],
    };
    if let Some(bad) = keys.iter().flatten().find(|k| !contained(k)) {
        return Err(ObjectError::Other(format!("key outside the volume: {bad}")));
    }
    let res = match op {
        ObjectOp::Get { key } => store.get(&key).await.map(ObjectReply::Data),
        ObjectOp::GetRange { key, start, end } => {
            store.get_range(&key, start..end).await.map(ObjectReply::Data)
        }
        ObjectOp::Put { key, data } => store.put(&key, data).await.map(|_| ObjectReply::Done),
        ObjectOp::PutIfAbsent { key, data } => {
            store.put_if_absent(&key, data).await.map(ObjectReply::Written)
        }
        ObjectOp::Head { key } => store
            .head(&key)
            .await
            .map(|m| ObjectReply::Head(m.map(|m| (m.size, m.modified.map(to_millis))))),
        ObjectOp::Delete { key } => store.delete(&key).await.map(|_| ObjectReply::Done),
        ObjectOp::List {
            prefix,
            start_after,
            limit,
        } => store
            .list(&prefix, start_after.as_deref(), limit as usize)
            .await
            .map(|page| ObjectReply::List {
                objects: page
                    .objects
                    .into_iter()
                    .map(|m| (m.key, m.size, m.modified.map(to_millis)))
                    .collect(),
                next_start_after: page.next_start_after,
            }),
    };
    res.map_err(|e| match e {
        rustfuse_store::Error::NotFound(k) => ObjectError::NotFound(k),
        e => ObjectError::Other(e.to_string()),
    })
}
