//! Talks to a running volume daemon over its provider socket (one JSON
//! request per line, one JSON answer per line), as the Mac app does.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use rustfuse_sync::lazy::Usage;
use rustfuse_sync::provider::{Item, Request, Response};

pub struct Client {
    socket: PathBuf,
    timeout: Duration,
}

/// What the daemon says about the space its volume takes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VolumeUsage {
    pub usage: Usage,
    pub limit: Option<u64>,
    pub lazy: bool,
}

impl Client {
    pub fn new(socket: &Path) -> Client {
        Client {
            socket: socket.to_path_buf(),
            timeout: Duration::from_secs(10),
        }
    }

    /// For requests that download (fetch, keep): waits up to an hour.
    pub fn patient(socket: &Path) -> Client {
        Client {
            socket: socket.to_path_buf(),
            timeout: Duration::from_secs(3600),
        }
    }

    /// One request, on a connection of its own.
    pub fn call(&self, req: &Request) -> Result<Response> {
        let mut stream = UnixStream::connect(&self.socket).context("the volume's daemon is not running")?;
        stream.set_read_timeout(Some(self.timeout))?;
        stream.set_write_timeout(Some(self.timeout))?;
        let mut line = serde_json::to_vec(req)?;
        line.push(b'\n');
        stream.write_all(&line)?;
        let mut answer = String::new();
        BufReader::new(stream).read_line(&mut answer)?;
        match serde_json::from_str(&answer).context("unreadable answer from the daemon")? {
            Response::Error { message, .. } => bail!("{message}"),
            resp => Ok(resp),
        }
    }

    /// Whether the daemon answers.
    pub fn alive(&self) -> bool {
        UnixStream::connect(&self.socket).is_ok() && self.call(&Request::Anchor).is_ok()
    }

    pub fn usage(&self) -> Result<VolumeUsage> {
        match self.call(&Request::Usage)? {
            Response::Usage { usage, limit, lazy } => Ok(VolumeUsage { usage, limit, lazy }),
            other => bail!("unexpected answer {other:?}"),
        }
    }

    /// Every child of directory `id`, by name.
    pub fn list(&self, id: &str) -> Result<Vec<Item>> {
        let mut all = Vec::new();
        let mut after = None;
        loop {
            match self.call(&Request::List {
                id: id.to_string(),
                after,
            })? {
                Response::List { items, next } => {
                    all.extend(items);
                    match next {
                        Some(n) => after = Some(n),
                        None => return Ok(all),
                    }
                }
                other => bail!("unexpected answer {other:?}"),
            }
        }
    }

    pub fn keep(&self, id: &str, keep: bool) -> Result<()> {
        self.done(Request::Keep {
            id: id.to_string(),
            keep,
        })
    }

    pub fn free(&self, id: &str) -> Result<()> {
        self.done(Request::Free { id: id.to_string() })
    }

    /// Downloads a placeholder's content.
    pub fn fetch(&self, id: &str) -> Result<()> {
        match self.call(&Request::Fetch { id: id.to_string() })? {
            Response::Path { .. } => Ok(()),
            other => bail!("unexpected answer {other:?}"),
        }
    }

    fn done(&self, req: Request) -> Result<()> {
        match self.call(&req)? {
            Response::Done => Ok(()),
            other => bail!("unexpected answer {other:?}"),
        }
    }
}
