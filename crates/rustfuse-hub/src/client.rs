use rustfuse_types::Changeset;
use tokio::net::TcpStream;
use tokio::net::tcp::OwnedWriteHalf;
use tokio::sync::mpsc;

use crate::codec::{read_frame, write_frame};
use crate::{Error, Request, Response, Result};

/// Something the hub sent.
#[derive(Debug)]
pub enum Event {
    /// The next changeset of the log.
    Changeset(Changeset),
    /// One of our changesets was accepted.
    Pushed { local: u64, seq: u64 },
}

/// A connection to the hub for one volume.
pub struct HubClient {
    wr: OwnedWriteHalf,
    events: mpsc::UnboundedReceiver<Result<Event>>,
    head: u64,
}

impl HubClient {
    /// Connects, authenticates and subscribes to changesets after `after`.
    pub async fn connect(addr: &str, volume: &str, token: &str, after: u64) -> Result<HubClient> {
        let stream = TcpStream::connect(addr).await?;
        let _ = stream.set_nodelay(true);
        let (mut rd, mut wr) = stream.into_split();
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
                    Ok(Some(Response::Welcome { .. })) => Err(Error::Protocol("unexpected welcome".into())),
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
