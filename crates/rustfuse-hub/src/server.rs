use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

use rustfuse_store::{ObjectStore, PrefixStore};
use rustfuse_types::{Changeset, FORMAT_VERSION};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Mutex, broadcast, mpsc};
use tracing::{debug, info, warn};

use crate::codec::{read_frame, write_frame};
use crate::log::Volume;
use crate::{Error, Request, Response, Result, valid_volume_name};

/// Changesets kept in memory per volume for clients that are slightly
/// behind; older ones are read back from the bucket.
const RECENT: usize = 4096;
/// Changesets buffered per connection before a slow client is dropped
/// (it reconnects and catches up).
const BROADCAST_CAPACITY: usize = 4096;

pub struct Hub {
    /// Store scoped to the bucket prefix that holds `volumes/`.
    root: Arc<dyn ObjectStore>,
    token: String,
    logs: Mutex<HashMap<String, Arc<Log>>>,
}

struct Log {
    volume: Volume,
    inner: Mutex<LogInner>,
    tx: broadcast::Sender<Arc<Changeset>>,
}

struct LogInner {
    head: u64,
    recent: VecDeque<Arc<Changeset>>,
}

impl Log {
    async fn append(&self, mut cs: Changeset) -> Result<u64> {
        let mut inner = self.inner.lock().await;
        cs.seq = inner.head + 1;
        cs.format = FORMAT_VERSION;
        self.volume.put_changeset(&cs).await?;
        inner.head = cs.seq;
        let cs = Arc::new(cs);
        if inner.recent.len() == RECENT {
            inner.recent.pop_front();
        }
        inner.recent.push_back(cs.clone());
        let _ = self.tx.send(cs.clone());
        Ok(cs.seq)
    }

    /// Everything after `after` that is cached, the range that must be read
    /// from the bucket first, and a receiver for what comes next, taken
    /// together so nothing is missed or repeated.
    async fn since(
        &self,
        after: u64,
    ) -> (
        u64,
        std::ops::Range<u64>,
        Vec<Arc<Changeset>>,
        broadcast::Receiver<Arc<Changeset>>,
    ) {
        let inner = self.inner.lock().await;
        let rx = self.tx.subscribe();
        let first_cached = inner.recent.front().map_or(inner.head + 1, |c| c.seq);
        let from_bucket = (after + 1)..first_cached.max(after + 1);
        let cached = inner.recent.iter().filter(|c| c.seq > after).cloned().collect();
        (inner.head, from_bucket, cached, rx)
    }
}

impl Hub {
    /// `root` holds the volumes (`volumes/<name>/…`); `token` is the shared
    /// secret every client must present.
    pub fn new(root: Arc<dyn ObjectStore>, token: String) -> Arc<Hub> {
        Arc::new(Hub {
            root,
            token,
            logs: Mutex::new(HashMap::new()),
        })
    }

    /// Accepts connections until the listener fails.
    pub async fn serve(self: Arc<Self>, listener: TcpListener) -> Result<()> {
        loop {
            let (stream, peer) = listener.accept().await?;
            let _ = stream.set_nodelay(true);
            let hub = self.clone();
            tokio::spawn(async move {
                debug!(%peer, "connected");
                if let Err(e) = hub.handle(stream).await {
                    debug!(%peer, "connection ended: {e}");
                }
            });
        }
    }

    async fn log(&self, name: &str) -> Result<Arc<Log>> {
        let mut logs = self.logs.lock().await;
        if let Some(l) = logs.get(name) {
            return Ok(l.clone());
        }
        let store = Arc::new(PrefixStore::new(self.root.clone(), &format!("volumes/{name}")));
        let volume = Volume::new(store);
        volume.check().await?;
        let head = volume.head().await?;
        info!(volume = name, head, "opened volume");
        let log = Arc::new(Log {
            volume,
            inner: Mutex::new(LogInner {
                head,
                recent: VecDeque::new(),
            }),
            tx: broadcast::channel(BROADCAST_CAPACITY).0,
        });
        logs.insert(name.to_owned(), log.clone());
        Ok(log)
    }

    async fn handle(&self, stream: TcpStream) -> Result<()> {
        let (mut rd, mut wr) = stream.into_split();
        let Some(Request::Hello { volume, token, after }) = read_frame(&mut rd).await? else {
            return Err(Error::Protocol("expected hello".into()));
        };
        if !constant_time_eq(token.as_bytes(), self.token.as_bytes()) {
            write_frame(&mut wr, &Response::Error("invalid token".into())).await?;
            return Err(Error::Refused("invalid token".into()));
        }
        if !valid_volume_name(&volume) {
            write_frame(&mut wr, &Response::Error("invalid volume name".into())).await?;
            return Err(Error::Refused("invalid volume name".into()));
        }
        let log = match self.log(&volume).await {
            Ok(l) => l,
            Err(e) => {
                write_frame(&mut wr, &Response::Error(e.to_string())).await?;
                return Err(e);
            }
        };

        // Requests are read on their own task: frame reads are not
        // cancel-safe, so they must not race in the select below.
        let (req_tx, mut req_rx) = mpsc::channel::<Result<Request>>(64);
        tokio::spawn(async move {
            loop {
                match read_frame(&mut rd).await {
                    Ok(Some(r)) => {
                        if req_tx.send(Ok(r)).await.is_err() {
                            return;
                        }
                    }
                    Ok(None) => return,
                    Err(e) => {
                        let _ = req_tx.send(Err(e)).await;
                        return;
                    }
                }
            }
        });

        let (head, from_bucket, cached, mut rx) = log.since(after).await;
        write_frame(&mut wr, &Response::Welcome { head }).await?;
        let mut sent = after;
        for seq in from_bucket {
            let cs = log.volume.get_changeset(seq).await?;
            write_frame(&mut wr, &Response::Changeset(cs)).await?;
            sent = seq;
        }
        for cs in cached {
            write_frame(&mut wr, &Response::Changeset((*cs).clone())).await?;
            sent = cs.seq;
        }

        loop {
            tokio::select! {
                req = req_rx.recv() => match req {
                    None => return Ok(()),
                    Some(Err(e)) => return Err(e),
                    Some(Ok(Request::Push(cs))) => {
                        let local = cs.local;
                        match log.append(cs).await {
                            Ok(seq) => {
                                debug!(volume, seq, "appended");
                                write_frame(&mut wr, &Response::Pushed { local, seq }).await?;
                            }
                            Err(e) => {
                                warn!(volume, "append failed: {e}");
                                write_frame(&mut wr, &Response::Error(e.to_string())).await?;
                                return Err(e);
                            }
                        }
                    }
                    Some(Ok(Request::Hello { .. })) => {
                        return Err(Error::Protocol("unexpected hello".into()));
                    }
                },
                cs = rx.recv() => match cs {
                    Ok(cs) => {
                        if cs.seq > sent {
                            sent = cs.seq;
                            write_frame(&mut wr, &Response::Changeset((*cs).clone())).await?;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        write_frame(&mut wr, &Response::Error("client too slow; reconnect".into())).await?;
                        return Ok(());
                    }
                    Err(broadcast::error::RecvError::Closed) => return Ok(()),
                },
            }
        }
    }
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}
