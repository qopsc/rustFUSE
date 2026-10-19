//! Answers git's `core.fsmonitor` hook (protocol v2) from the daemon's file
//! watcher, so `git status` only looks at paths that changed instead of
//! stat-ing the whole worktree (`docs/design.md` §8).
//!
//! The daemon keeps a journal of changed paths numbered by a sequence. A
//! token is `rustfuse:<daemon instance>:<sequence>`; git hands back the last
//! token it saw and gets every path changed since. Anything the journal
//! cannot vouch for (another daemon run, a token from another machine's
//! index, a dropped event, a trimmed journal) answers "everything changed",
//! which makes git fall back to a full scan.
//!
//! Before answering, the daemon writes a cookie file and waits until the
//! watcher reports it. File events arrive in order, so every change made
//! before the query is in the journal by then.

use std::collections::{BTreeSet, VecDeque};
use std::io::{Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::STATE_DIR;

/// Socket the daemon listens on, under the volume's state directory.
pub const SOCKET: &str = "fsmonitor.sock";
/// Cookie files live directly in the state directory, which is watched
/// from the start; a new subdirectory might be watched too late.
const COOKIE_PREFIX: &str = "fsmonitor-cookie-";
/// Oldest entries are dropped beyond this; their tokens then get a full
/// scan.
const MAX_ENTRIES: usize = 1 << 18;
const COOKIE_WAIT: Duration = Duration::from_secs(2);
const TOKEN_PREFIX: &str = "rustfuse:";

/// Paths changed under the volume root, numbered in the order seen.
pub struct Journal {
    instance: u64,
    state: Mutex<State>,
    cookies: AtomicU64,
    cookie_seen: tokio::sync::Notify,
}

struct State {
    /// Sequence of the latest change.
    seq: u64,
    /// The journal holds every change after this sequence.
    floor: u64,
    entries: VecDeque<(u64, PathBuf)>,
    /// Highest cookie the watcher has reported.
    cookie: u64,
}

impl Journal {
    pub fn new() -> Arc<Self> {
        let mut b = [0u8; 8];
        getrandom::fill(&mut b).expect("system randomness");
        Arc::new(Self {
            instance: u64::from_le_bytes(b),
            state: Mutex::new(State {
                seq: 0,
                floor: 0,
                entries: VecDeque::new(),
                cookie: 0,
            }),
            cookies: AtomicU64::new(0),
            cookie_seen: tokio::sync::Notify::new(),
        })
    }

    /// Records a watcher event. Called on the watcher's thread, before the
    /// event is queued for capture.
    pub fn record(&self, root: &Path, ev: &notify::Result<notify::Event>) {
        let mut s = self.state.lock().unwrap();
        let ev = match ev {
            Ok(ev) if !ev.need_rescan() => ev,
            // Something may have been missed: no earlier token is valid.
            _ => {
                s.seq += 1;
                s.floor = s.seq;
                s.entries.clear();
                return;
            }
        };
        if matches!(ev.kind, notify::EventKind::Access(_)) {
            return;
        }
        let state_dir = Path::new(STATE_DIR);
        let mut cookie_seen = false;
        for p in &ev.paths {
            let Ok(rel) = p.strip_prefix(root) else {
                continue;
            };
            if let Some(name) = rel.strip_prefix(state_dir).ok().and_then(|n| n.to_str())
                && let Some(n) = name.strip_prefix(COOKIE_PREFIX)
            {
                if let Ok(n) = n.parse::<u64>()
                    && n > s.cookie
                {
                    s.cookie = n;
                    cookie_seen = true;
                }
                continue;
            }
            // git does not want its own files reported, and leaving them
            // out keeps the token unchanged when only git wrote, so a
            // repeated `git status` does not rewrite the index.
            if rel.starts_with(STATE_DIR) || rel.components().any(|c| c.as_os_str() == ".git") {
                continue;
            }
            s.seq += 1;
            let seq = s.seq;
            s.entries.push_back((seq, rel.to_path_buf()));
            if s.entries.len() > MAX_ENTRIES
                && let Some((dropped, _)) = s.entries.pop_front()
            {
                s.floor = dropped;
            }
        }
        drop(s);
        if cookie_seen {
            self.cookie_seen.notify_waiters();
        }
    }

    fn token(&self, seq: u64) -> String {
        format!("{TOKEN_PREFIX}{:016x}:{seq}", self.instance)
    }

    /// The hook's answer: a new token, then the paths under `worktree`
    /// (relative to the volume root) changed since `token`, relative to
    /// the worktree, each NUL-terminated. `/` stands for everything.
    pub fn answer(&self, token: &str, worktree: &Path) -> Vec<u8> {
        let s = self.state.lock().unwrap();
        let mut out = self.token(s.seq).into_bytes();
        out.push(0);
        let since = token
            .strip_prefix(TOKEN_PREFIX)
            .and_then(|t| t.split_once(':'))
            .filter(|(inst, _)| u64::from_str_radix(inst, 16).ok() == Some(self.instance))
            .and_then(|(_, seq)| seq.parse::<u64>().ok())
            .filter(|&seq| seq >= s.floor && seq <= s.seq);
        let Some(since) = since else {
            out.extend_from_slice(b"/\0");
            return out;
        };
        let start = s.entries.partition_point(|(seq, _)| *seq <= since);
        let mut paths = BTreeSet::new();
        for (_, p) in s.entries.range(start..) {
            if let Ok(rel) = p.strip_prefix(worktree)
                && !rel.as_os_str().is_empty()
            {
                paths.insert(rel);
            }
        }
        for p in paths {
            let p = p.as_os_str().as_bytes();
            // The path may be a directory that was created, removed or
            // renamed; the trailing-slash form invalidates what is under it.
            out.extend_from_slice(p);
            out.push(0);
            out.extend_from_slice(p);
            out.extend_from_slice(b"/\0");
        }
        out
    }

    /// Waits until the watcher has delivered every event for changes made
    /// before this call. False if the watcher did not catch up in time.
    pub async fn sync_point(&self, root: &Path) -> bool {
        let n = self.cookies.fetch_add(1, Ordering::SeqCst) + 1;
        let path = root.join(STATE_DIR).join(format!("{COOKIE_PREFIX}{n}"));
        if std::fs::write(&path, b"").is_err() {
            return false;
        }
        let seen = tokio::time::timeout(COOKIE_WAIT, async {
            loop {
                let wait = self.cookie_seen.notified();
                if self.state.lock().unwrap().cookie >= n {
                    return;
                }
                wait.await;
            }
        })
        .await
        .is_ok();
        let _ = std::fs::remove_file(&path);
        seen
    }
}

/// Serves hook queries on the volume's socket until the task is dropped.
pub async fn serve(root: PathBuf, journal: Arc<Journal>) {
    let path = root.join(STATE_DIR).join(SOCKET);
    let _ = std::fs::remove_file(&path);
    let listener = match tokio::net::UnixListener::bind(&path) {
        Ok(l) => l,
        Err(e) => {
            tracing::warn!(
                "git fsmonitor support off: cannot listen on {}: {e}",
                path.display()
            );
            return;
        }
    };
    loop {
        let Ok((stream, _)) = listener.accept().await else {
            continue;
        };
        let root = root.clone();
        let journal = journal.clone();
        tokio::spawn(async move {
            if let Err(e) = handle(stream, &root, &journal).await {
                tracing::debug!("fsmonitor query failed: {e}");
            }
        });
    }
}

async fn handle(mut stream: tokio::net::UnixStream, root: &Path, journal: &Journal) -> std::io::Result<()> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut req = Vec::new();
    stream.read_to_end(&mut req).await?;
    let mut parts = req.split(|&b| b == 0);
    let token = String::from_utf8_lossy(parts.next().unwrap_or_default()).into_owned();
    let worktree = PathBuf::from(std::ffi::OsStr::from_bytes(parts.next().unwrap_or_default()));
    let out = if journal.sync_point(root).await {
        journal.answer(&token, &worktree)
    } else {
        everything(&journal.token(u64::MAX))
    };
    stream.write_all(&out).await?;
    stream.shutdown().await
}

fn everything(token: &str) -> Vec<u8> {
    let mut out = token.as_bytes().to_vec();
    out.extend_from_slice(b"\0/\0");
    out
}

/// The hook side: run by git in a worktree as `<hook> <version> <token>`.
/// Asks the daemon for this worktree's changes; if no daemon answers, says
/// everything changed so git scans as it would without a hook.
pub fn hook(version: u32, token: &str, cwd: &Path) -> Vec<u8> {
    if version != 2 {
        return b"/\0".to_vec();
    }
    let fallback = || everything(&format!("{TOKEN_PREFIX}none:0"));
    let Some(worktree) = cwd.ancestors().find(|d| d.join(".git").exists()) else {
        return fallback();
    };
    let Some(root) = worktree.ancestors().find(|d| d.join(STATE_DIR).is_dir()) else {
        return fallback();
    };
    let rel = worktree.strip_prefix(root).unwrap_or(Path::new(""));
    let mut socket = root.join(STATE_DIR).join(SOCKET);
    // Socket paths are limited to about 100 bytes; a relative one is not.
    if socket.as_os_str().len() > 100 {
        if std::env::set_current_dir(root.join(STATE_DIR)).is_err() {
            return fallback();
        }
        socket = PathBuf::from(SOCKET);
    }
    let query = || -> std::io::Result<Vec<u8>> {
        let mut s = UnixStream::connect(&socket)?;
        s.set_read_timeout(Some(COOKIE_WAIT * 2))?;
        let mut req = token.as_bytes().to_vec();
        req.push(0);
        req.extend_from_slice(rel.as_os_str().as_bytes());
        s.write_all(&req)?;
        s.shutdown(std::net::Shutdown::Write)?;
        let mut out = Vec::new();
        s.read_to_end(&mut out)?;
        Ok(out)
    };
    match query() {
        Ok(out) if out.contains(&0) => out,
        _ => fallback(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use notify::event::{CreateKind, EventKind, Flag, ModifyKind};

    fn ev(root: &Path, kind: EventKind, paths: &[&str]) -> notify::Result<notify::Event> {
        let mut e = notify::Event::new(kind);
        for p in paths {
            e = e.add_path(root.join(p));
        }
        Ok(e)
    }

    fn parse(out: &[u8]) -> (String, Vec<String>) {
        let mut it = out
            .split(|&b| b == 0)
            .map(|s| String::from_utf8(s.to_vec()).unwrap());
        let token = it.next().unwrap();
        let mut rest: Vec<String> = it.collect();
        assert_eq!(rest.pop().as_deref(), Some(""), "not NUL-terminated");
        (token, rest)
    }

    #[test]
    fn reports_changes_since_token() {
        let root = Path::new("/vol");
        let j = Journal::new();
        let (t0, all) = parse(&j.answer("", Path::new("repo")));
        assert_eq!(all, ["/"], "unknown token must mean everything");

        j.record(
            root,
            &ev(root, EventKind::Modify(ModifyKind::Any), &["repo/src/a.rs"]),
        );
        j.record(
            root,
            &ev(
                root,
                EventKind::Create(CreateKind::Any),
                &["repo/.git/index.lock"],
            ),
        );
        j.record(
            root,
            &ev(root, EventKind::Modify(ModifyKind::Any), &["other/b.rs"]),
        );
        j.record(
            root,
            &ev(
                root,
                EventKind::Create(CreateKind::Any),
                &[".rustfuse/index.redb"],
            ),
        );
        let (t1, paths) = parse(&j.answer(&t0, Path::new("repo")));
        assert_eq!(paths, ["src/a.rs", "src/a.rs/"]);

        // Only git's own files changed: same token, nothing to report.
        j.record(
            root,
            &ev(root, EventKind::Modify(ModifyKind::Any), &["repo/.git/index"]),
        );
        let (t2, paths) = parse(&j.answer(&t1, Path::new("repo")));
        assert_eq!((t2.as_str(), paths.len()), (t1.as_str(), 0));

        // A worktree at the volume root sees everything outside `.git`.
        let (_, paths) = parse(&j.answer(&t0, Path::new("")));
        assert_eq!(
            paths,
            ["other/b.rs", "other/b.rs/", "repo/src/a.rs", "repo/src/a.rs/"]
        );
    }

    #[test]
    fn missed_events_invalidate_tokens() {
        let root = Path::new("/vol");
        let j = Journal::new();
        let (t0, _) = parse(&j.answer("", Path::new("")));
        j.record(
            root,
            &Ok(notify::Event::new(EventKind::Other).set_flag(Flag::Rescan)),
        );
        assert_eq!(parse(&j.answer(&t0, Path::new(""))).1, ["/"]);
        let other = Journal::new();
        assert_eq!(
            parse(&other.answer(&t0, Path::new(""))).1,
            ["/"],
            "token from another daemon"
        );
        assert_eq!(parse(&j.answer("rustfuse:zz:1", Path::new(""))).1, ["/"]);
    }
}
