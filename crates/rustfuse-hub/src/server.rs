use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::Instant;

use rustfuse_store::{ObjectStore, PrefixStore};
use rustfuse_types::{Changeset, FORMAT_VERSION};
use tokio::net::TcpListener;
use tokio::sync::{Mutex, broadcast, mpsc};
use tracing::{debug, info, warn};

use crate::codec::{read_frame, write_frame};
use crate::log::Volume;
use crate::metrics::{Counter, Family, Gauge, Histogram, Text};
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
    metrics: Arc<HubMetrics>,
}

/// What the hub reports at `/metrics` (`rustfuse hub --metrics`).
pub struct HubMetrics {
    /// Open connections, by kind (`log` or `store`).
    pub connections: Family<Gauge>,
    pub connections_total: Family<Counter>,
    /// Connections turned away for a wrong token or volume name.
    pub refused_total: Counter,
    pub appended_total: Family<Counter>,
    pub append_errors_total: Counter,
    pub append_seconds: Histogram,
    /// Last changeset of each volume opened since the hub started.
    pub head: Family<Gauge>,
    /// Machines dropped for falling too far behind the broadcast.
    pub slow_clients_total: Counter,
    pub store_requests_total: Family<Counter>,
    pub store_errors_total: Family<Counter>,
    pub store_seconds: Family<Histogram>,
    /// Object bytes received from machines (`in`) and sent to them (`out`).
    pub store_bytes_total: Family<Counter>,
}

impl Default for HubMetrics {
    fn default() -> Self {
        HubMetrics {
            connections: Family::new("kind"),
            connections_total: Family::new("kind"),
            refused_total: Counter::default(),
            appended_total: Family::new("volume"),
            append_errors_total: Counter::default(),
            append_seconds: Histogram::default(),
            head: Family::new("volume"),
            slow_clients_total: Counter::default(),
            store_requests_total: Family::new("op"),
            store_errors_total: Family::new("op"),
            store_seconds: Family::new("op"),
            store_bytes_total: Family::new("direction"),
        }
    }
}

impl HubMetrics {
    pub fn render(&self) -> String {
        Text::default()
            .family("rustfuse_hub_connections", "Open connections.", &self.connections)
            .family(
                "rustfuse_hub_connections_total",
                "Connections accepted.",
                &self.connections_total,
            )
            .metric(
                "rustfuse_hub_refused_total",
                "Connections refused for a wrong token or volume name.",
                &self.refused_total,
            )
            .family(
                "rustfuse_hub_changesets_appended_total",
                "Changesets appended to the log.",
                &self.appended_total,
            )
            .metric(
                "rustfuse_hub_append_errors_total",
                "Changesets that could not be written to the bucket.",
                &self.append_errors_total,
            )
            .metric(
                "rustfuse_hub_append_seconds",
                "Time to write a changeset to the bucket.",
                &self.append_seconds,
            )
            .family("rustfuse_hub_log_head", "Last changeset in the log.", &self.head)
            .metric(
                "rustfuse_hub_slow_clients_total",
                "Machines disconnected for falling behind.",
                &self.slow_clients_total,
            )
            .family(
                "rustfuse_hub_store_requests_total",
                "Object calls made through the hub.",
                &self.store_requests_total,
            )
            .family(
                "rustfuse_hub_store_errors_total",
                "Object calls through the hub that failed.",
                &self.store_errors_total,
            )
            .family(
                "rustfuse_hub_store_seconds",
                "Time to answer an object call.",
                &self.store_seconds,
            )
            .family(
                "rustfuse_hub_store_bytes_total",
                "Object bytes passed through the hub.",
                &self.store_bytes_total,
            )
            .finish()
    }
}

/// Counts a connection as open until dropped.
struct Open(Arc<Gauge>);

impl Open {
    fn new(m: &HubMetrics, kind: &str) -> Open {
        m.connections_total.get(kind).inc();
        let g = m.connections.get(kind);
        g.inc();
        Open(g)
    }
}

impl Drop for Open {
    fn drop(&mut self) {
        self.0.dec();
    }
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
            metrics: Arc::default(),
        })
    }

    pub fn metrics(&self) -> &Arc<HubMetrics> {
        &self.metrics
    }

    /// Accepts plain TCP connections until the listener fails.
    pub async fn serve(self: Arc<Self>, listener: TcpListener) -> Result<()> {
        self.serve_with(listener, None).await
    }

    /// Accepts connections until the listener fails, over TLS when `tls`
    /// is given.
    pub async fn serve_with(
        self: Arc<Self>,
        listener: TcpListener,
        tls: Option<Arc<rustls::ServerConfig>>,
    ) -> Result<()> {
        let acceptor = tls.map(tokio_rustls::TlsAcceptor::from);
        loop {
            let (stream, peer) = listener.accept().await?;
            let _ = stream.set_nodelay(true);
            let hub = self.clone();
            let acceptor = acceptor.clone();
            tokio::spawn(async move {
                debug!(%peer, "connected");
                let res = match acceptor {
                    Some(a) => match a.accept(stream).await {
                        Ok(s) => hub.handle(s).await,
                        Err(e) => Err(Error::Tls(e.to_string())),
                    },
                    None => hub.handle(stream).await,
                };
                if let Err(e) = res {
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
        self.metrics.head.get(name).set(head as i64);
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

    async fn handle<S>(&self, stream: S) -> Result<()>
    where
        S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Send + Unpin + 'static,
    {
        let (mut rd, mut wr) = tokio::io::split(stream);
        let (volume, token, after) = match read_frame(&mut rd).await? {
            Some(Request::Hello { volume, token, after }) => (volume, token, Some(after)),
            Some(Request::Store { volume, token }) => (volume, token, None),
            _ => return Err(Error::Protocol("expected hello".into())),
        };
        if !constant_time_eq(token.as_bytes(), self.token.as_bytes()) {
            self.metrics.refused_total.inc();
            write_frame(&mut wr, &Response::Error("invalid token".into())).await?;
            return Err(Error::Refused("invalid token".into()));
        }
        if !valid_volume_name(&volume) {
            self.metrics.refused_total.inc();
            write_frame(&mut wr, &Response::Error("invalid volume name".into())).await?;
            return Err(Error::Refused("invalid volume name".into()));
        }
        let Some(after) = after else {
            // Storage works before the volume exists: `init` creates it.
            let store = Arc::new(PrefixStore::new(self.root.clone(), &format!("volumes/{volume}")));
            debug!(volume, "storage connection");
            let _open = Open::new(&self.metrics, "store");
            return crate::store::serve(store, rd, wr, self.metrics.clone()).await;
        };
        let log = match self.log(&volume).await {
            Ok(l) => l,
            Err(e) => {
                write_frame(&mut wr, &Response::Error(e.to_string())).await?;
                return Err(e);
            }
        };

        let _open = Open::new(&self.metrics, "log");
        let appended = self.metrics.appended_total.get(&volume);
        let head_gauge = self.metrics.head.get(&volume);

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
                        let t0 = Instant::now();
                        match log.append(cs).await {
                            Ok(seq) => {
                                debug!(volume, seq, "appended");
                                self.metrics.append_seconds.observe(t0.elapsed());
                                appended.inc();
                                head_gauge.max(seq as i64);
                                write_frame(&mut wr, &Response::Pushed { local, seq }).await?;
                            }
                            Err(e) => {
                                warn!(volume, "append failed: {e}");
                                self.metrics.append_errors_total.inc();
                                write_frame(&mut wr, &Response::Error(e.to_string())).await?;
                                return Err(e);
                            }
                        }
                    }
                    Some(Ok(Request::Hello { .. } | Request::Store { .. })) => {
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
                        self.metrics.slow_clients_total.inc();
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
