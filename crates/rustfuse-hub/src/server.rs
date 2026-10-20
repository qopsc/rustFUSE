use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};

use rustfuse_store::{ObjectStore, PrefixStore};
use rustfuse_types::{Changeset, FORMAT_VERSION};
use tokio::net::TcpListener;
use tokio::sync::{Mutex, broadcast, mpsc};
use tracing::{debug, info, warn};

use crate::codec::{read_frame, write_frame};
use crate::keys::{KeyCheck, Keys};
use crate::log::Volume;
use crate::metrics::{Counter, Family, Gauge, Histogram, Text};
use crate::{Error, Request, Response, Result, valid_volume_name};

/// Changesets kept in memory per volume for clients that are slightly
/// behind; older ones are read back from the bucket.
const RECENT: usize = 4096;
/// Changesets buffered per connection before a slow client is dropped
/// (it reconnects and catches up).
const BROADCAST_CAPACITY: usize = 4096;
/// How often a hub looks in the bucket for changesets another hub
/// appended, while machines are connected to the volume.
const PEER_POLL: Duration = Duration::from_secs(2);
/// Changesets read into the cache when a hub opens a volume.
const WARM: usize = 64;
/// Times an append retries after another hub took its number.
const MAX_RACES: usize = 16;

pub struct Hub {
    /// Store scoped to the bucket prefix that holds `volumes/`.
    root: Arc<dyn ObjectStore>,
    /// Shared token; empty when only per-machine keys are accepted.
    token: String,
    keys: Arc<Keys>,
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
    /// Changesets another hub appended, found in the bucket.
    pub peer_changesets_total: Counter,
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
            peer_changesets_total: Counter::default(),
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
            .metric(
                "rustfuse_hub_peer_changesets_total",
                "Changesets appended by another hub and picked up from the bucket.",
                &self.peer_changesets_total,
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
    metrics: Arc<HubMetrics>,
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
        cs.format = FORMAT_VERSION;
        for _ in 0..MAX_RACES {
            // A machine re-sends what it pushed just before losing its
            // connection, maybe to another hub: answer with the number it
            // already got.
            if let Some(seq) = inner
                .recent
                .iter()
                .rev()
                .find(|c| c.version.replica == cs.version.replica && c.local == cs.local)
                .map(|c| c.seq)
            {
                return Ok(seq);
            }
            cs.seq = inner.head + 1;
            match self.volume.put_changeset(&cs).await {
                Ok(()) => {
                    let seq = cs.seq;
                    self.publish(&mut inner, Arc::new(cs));
                    return Ok(seq);
                }
                // Another hub appended: take what it wrote, then try the
                // next number.
                Err(Error::Conflict(seq)) => {
                    debug!(seq, "changeset number taken by another hub");
                    self.catch_up(&mut inner).await?;
                }
                Err(e) => return Err(e),
            }
        }
        Err(Error::Refused("log too busy; try again".into()))
    }

    /// Records an appended changeset and sends it to connected machines.
    fn publish(&self, inner: &mut LogInner, cs: Arc<Changeset>) {
        inner.head = cs.seq;
        if inner.recent.len() == RECENT {
            inner.recent.pop_front();
        }
        inner.recent.push_back(cs.clone());
        let _ = self.tx.send(cs);
    }

    /// Takes in changesets other hubs appended after this hub's head, in
    /// order. Returns how many.
    async fn catch_up(&self, inner: &mut LogInner) -> Result<usize> {
        let mut n = 0;
        for seq in self.volume.changesets_after(inner.head).await? {
            if seq != inner.head + 1 {
                break;
            }
            let cs = self.volume.get_changeset(seq).await?;
            self.publish(inner, Arc::new(cs));
            n += 1;
        }
        self.metrics.peer_changesets_total.add(n as u64);
        Ok(n)
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
        Self::with_key_ttl(root, token, crate::keys::CACHE_TTL)
    }

    /// Like [`new`](Self::new); per-machine keys ([`crate::keys`]) are
    /// re-read after `key_ttl`. An empty `token` accepts only those keys.
    pub fn with_key_ttl(root: Arc<dyn ObjectStore>, token: String, key_ttl: Duration) -> Arc<Hub> {
        Arc::new(Hub {
            keys: Arc::new(Keys::new(root.clone(), key_ttl)),
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
        let _poll = AbortOnDrop(tokio::spawn(self.clone().poll_peers()));
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

    /// Picks up changesets other hubs append to the same volumes, so
    /// machines on this hub see them without waiting for their own next
    /// push. One hub per bucket finds nothing; several can serve at once.
    async fn poll_peers(self: Arc<Self>) {
        let mut tick = tokio::time::interval(PEER_POLL);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            let logs: Vec<(String, Arc<Log>)> = {
                let logs = self.logs.lock().await;
                logs.iter()
                    .filter(|(_, l)| l.tx.receiver_count() > 0)
                    .map(|(n, l)| (n.clone(), l.clone()))
                    .collect()
            };
            for (name, log) in logs {
                let mut inner = log.inner.lock().await;
                match log.catch_up(&mut inner).await {
                    Ok(0) => {}
                    Ok(n) => {
                        debug!(
                            volume = name,
                            n,
                            head = inner.head,
                            "took changesets from another hub"
                        );
                        self.metrics.head.get(&name).max(inner.head as i64);
                    }
                    Err(e) => warn!(volume = name, "reading the log from the bucket failed: {e}"),
                }
            }
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
        // The tail of the log, so changesets re-sent after a failover from
        // another hub are recognised.
        let mut recent = VecDeque::new();
        for seq in head.saturating_sub(WARM as u64 - 1).max(1)..=head {
            recent.push_back(Arc::new(volume.get_changeset(seq).await?));
        }
        let log = Arc::new(Log {
            volume,
            metrics: self.metrics.clone(),
            inner: Mutex::new(LogInner { head, recent }),
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
            Some(Request::Volumes { token }) => return self.list_volumes(&token, &mut wr).await,
            _ => return Err(Error::Protocol("expected hello".into())),
        };
        if !valid_volume_name(&volume) {
            self.metrics.refused_total.inc();
            write_frame(&mut wr, &Response::Error("invalid volume name".into())).await?;
            return Err(Error::Refused("invalid volume name".into()));
        }
        let mut check = match self.authorize(&token, &volume).await {
            Ok(None) => None,
            Ok(Some(id)) => Some(KeyCheck {
                keys: self.keys.clone(),
                id,
                volume: volume.clone(),
                last: Instant::now(),
            }),
            Err(msg) => {
                self.metrics.refused_total.inc();
                write_frame(&mut wr, &Response::Error(msg.clone())).await?;
                return Err(Error::Refused(msg));
            }
        };
        let Some(after) = after else {
            // Storage works before the volume exists: `init` creates it.
            let store = Arc::new(PrefixStore::new(self.root.clone(), &format!("volumes/{volume}")));
            debug!(volume, "storage connection");
            let _open = Open::new(&self.metrics, "store");
            return crate::store::serve(store, rd, wr, self.metrics.clone(), check).await;
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
        let mut recheck =
            tokio::time::interval(check.as_ref().map_or(Duration::from_secs(3600), KeyCheck::ttl));
        recheck.tick().await;
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
                _ = recheck.tick(), if check.is_some() => {
                    if let Some(c) = check.as_mut()
                        && !c.ok().await
                    {
                        debug!(volume, key = c.id, "key revoked; closing");
                        write_frame(&mut wr, &Response::Error("key revoked".into())).await?;
                        return Ok(());
                    }
                }
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
                    Some(Ok(Request::Hello { .. } | Request::Store { .. } | Request::Volumes { .. })) => {
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

struct AbortOnDrop(tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

impl Hub {
    /// Answers [`Request::Volumes`]: every volume for the shared token,
    /// those a per-machine key opens for a key.
    async fn list_volumes<W: tokio::io::AsyncWrite + Unpin>(&self, token: &str, wr: &mut W) -> Result<()> {
        let shared = !self.token.is_empty() && constant_time_eq(token.as_bytes(), self.token.as_bytes());
        let key = if shared {
            None
        } else {
            match self.keys.verify(token).await {
                Ok(Some(k)) => Some(k),
                Ok(None) => {
                    self.metrics.refused_total.inc();
                    write_frame(wr, &Response::Error("invalid token".into())).await?;
                    return Err(Error::Refused("invalid token".into()));
                }
                Err(e) => {
                    let msg = format!("cannot check the key: {e}");
                    write_frame(wr, &Response::Error(msg.clone())).await?;
                    return Err(Error::Refused(msg));
                }
            }
        };
        let mut names = match crate::log::list_volumes(self.root.as_ref()).await {
            Ok(n) => n,
            Err(e) => {
                write_frame(wr, &Response::Error(e.to_string())).await?;
                return Err(e);
            }
        };
        if let Some(k) = key {
            names.retain(|v| k.allows(v));
        }
        write_frame(wr, &Response::Volumes(names)).await
    }

    /// Checks a token for `volume`: `None` for the shared token, the key
    /// id for a per-machine key, or why it is refused.
    async fn authorize(&self, token: &str, volume: &str) -> std::result::Result<Option<String>, String> {
        if !self.token.is_empty() && constant_time_eq(token.as_bytes(), self.token.as_bytes()) {
            return Ok(None);
        }
        match self.keys.verify(token).await {
            Ok(Some(k)) if k.allows(volume) => Ok(Some(k.id.clone())),
            Ok(Some(k)) => Err(format!("key {} does not open volume {volume}", k.name)),
            Ok(None) => Err("invalid token".into()),
            Err(e) => Err(format!("cannot check the key: {e}")),
        }
    }
}

pub(crate) fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}
