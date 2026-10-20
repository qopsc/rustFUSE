use std::sync::Arc;
use std::time::Duration;

use rustfuse_types::Changeset;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tracing::debug;

use crate::codec::{read_frame, write_frame};
use crate::{Error, Request, Response, Result};

pub(crate) type Reader = Box<dyn AsyncRead + Send + Unpin>;
pub(crate) type Writer = Box<dyn AsyncWrite + Send + Unpin>;

/// How long to wait for one hub to accept a connection.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// Opens a connection to the hub, over TLS when `tls` is given. `addrs`
/// may list several hubs serving the same bucket, comma-separated; they
/// are tried in order.
pub(crate) async fn connect_stream(
    addrs: &str,
    tls: Option<&Arc<rustls::ClientConfig>>,
) -> Result<(Reader, Writer)> {
    let mut last = None;
    for addr in addrs.split(',').map(str::trim).filter(|a| !a.is_empty()) {
        match connect_one(addr, tls).await {
            Ok(s) => return Ok(s),
            Err(e) => {
                debug!(addr, "hub unreachable: {e}");
                last = Some(e);
            }
        }
    }
    Err(last.unwrap_or_else(|| Error::Protocol("no hub address".into())))
}

async fn connect_one(addr: &str, tls: Option<&Arc<rustls::ClientConfig>>) -> Result<(Reader, Writer)> {
    let stream = tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(addr))
        .await
        .map_err(|_| {
            std::io::Error::new(std::io::ErrorKind::TimedOut, format!("{addr}: connect timed out"))
        })??;
    let _ = stream.set_nodelay(true);
    Ok(match tls {
        Some(config) => {
            let name = crate::tls::server_name(addr)?;
            let stream = tokio_rustls::TlsConnector::from(config.clone())
                .connect(name, stream)
                .await
                .map_err(|e| crate::Error::Tls(format!("{addr}: {e}")))?;
            let (r, w) = tokio::io::split(stream);
            (Box::new(r), Box::new(w))
        }
        None => {
            let (r, w) = stream.into_split();
            (Box::new(r), Box::new(w))
        }
    })
}

/// Something the hub sent.
#[derive(Debug)]
pub enum Event {
    /// The next changeset of the log.
    Changeset(Changeset),
    /// One of our changesets was accepted.
    Pushed { local: u64, seq: u64 },
}

/// The volumes in the hub's bucket that `token` opens, by name.
pub async fn volumes(
    addr: &str,
    tls: Option<&Arc<rustls::ClientConfig>>,
    token: &str,
) -> Result<Vec<String>> {
    let (mut rd, mut wr) = connect_stream(addr, tls).await?;
    write_frame(
        &mut wr,
        &Request::Volumes {
            token: token.to_owned(),
        },
    )
    .await?;
    match read_frame(&mut rd).await? {
        Some(Response::Volumes(v)) => Ok(v),
        Some(Response::Error(e)) => Err(Error::Refused(e)),
        Some(other) => Err(Error::Protocol(format!("unexpected {other:?}"))),
        None => Err(Error::Protocol(
            "hub closed the connection (is it older than this machine's rustfuse?)".into(),
        )),
    }
}

/// A connection to the hub for one volume.
pub struct HubClient {
    wr: Writer,
    events: mpsc::UnboundedReceiver<Result<Event>>,
    head: u64,
}

impl HubClient {
    /// Connects, authenticates and subscribes to changesets after `after`.
    pub async fn connect(addr: &str, volume: &str, token: &str, after: u64) -> Result<HubClient> {
        Self::connect_with(addr, None, volume, token, after).await
    }

    /// Like [`connect`](Self::connect), over TLS when `tls` is given.
    pub async fn connect_with(
        addr: &str,
        tls: Option<&Arc<rustls::ClientConfig>>,
        volume: &str,
        token: &str,
        after: u64,
    ) -> Result<HubClient> {
        let (mut rd, mut wr) = connect_stream(addr, tls).await?;
        write_frame(
            &mut wr,
            &Request::Hello {
                volume: volume.to_owned(),
                token: token.to_owned(),
                after,
            },
        )
        .await?;
        let head = match read_frame(&mut rd).await? {
            Some(Response::Welcome { head }) => head,
            Some(Response::Error(e)) => return Err(Error::Refused(e)),
            Some(other) => return Err(Error::Protocol(format!("unexpected {other:?}"))),
            None => return Err(Error::Protocol("hub closed the connection".into())),
        };
        let (tx, events) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            loop {
                let ev = match read_frame(&mut rd).await {
                    Ok(Some(Response::Changeset(cs))) => Ok(Event::Changeset(cs)),
                    Ok(Some(Response::Pushed { local, seq })) => Ok(Event::Pushed { local, seq }),
                    Ok(Some(Response::Error(e))) => Err(Error::Refused(e)),
                    Ok(Some(other)) => Err(Error::Protocol(format!("unexpected {other:?}"))),
                    Ok(None) => return,
                    Err(e) => Err(e),
                };
                let stop = ev.is_err();
                if tx.send(ev).is_err() || stop {
                    return;
                }
            }
        });
        Ok(HubClient { wr, events, head })
    }

    /// Last sequence number in the log when we connected.
    pub fn head(&self) -> u64 {
        self.head
    }

    /// Sends a changeset; the acknowledgement arrives as [`Event::Pushed`].
    pub async fn push(&mut self, cs: &Changeset) -> Result<()> {
        write_frame(&mut self.wr, &Request::Push(cs.clone())).await
    }

    /// Next event; `None` once the connection is closed. Cancel-safe.
    pub async fn next(&mut self) -> Option<Result<Event>> {
        self.events.recv().await
    }

    /// An event that has already arrived, without waiting.
    pub fn try_next(&mut self) -> Option<Result<Event>> {
        self.events.try_recv().ok()
    }
}
