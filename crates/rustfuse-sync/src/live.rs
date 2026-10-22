//! What a running sync is doing, for `rustfuse status` and other tools
//! while the sync holds the index: `.rustfuse/live.json`, rewritten when
//! it changes and removed when the sync stops.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::STATE_DIR;
use crate::metrics::SyncMetrics;

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Live {
    /// The syncing process.
    pub pid: u32,
    /// When it started (Unix seconds).
    pub started: u64,
    pub volume: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hub: Option<String>,
    pub connected: bool,
    /// Last changeset applied here.
    pub applied: u64,
    /// Last changeset known to be in the log.
    pub hub_head: u64,
    /// Local changesets the hub has not accepted yet.
    pub outbox: u64,
    /// When this machine was last connected, up to date and had nothing
    /// left to send (Unix seconds; 0: not yet this run).
    pub in_sync_at: u64,
}

impl Live {
    /// Connected, up to date, nothing left to send.
    pub fn in_sync(&self) -> bool {
        self.connected && self.outbox == 0 && self.applied >= self.hub_head
    }

    /// The live state of the sync running in `root`, if one is.
    pub fn read(root: &Path) -> Option<Live> {
        let live: Live = serde_json::from_slice(&std::fs::read(path(root)).ok()?).ok()?;
        // Left behind by a sync that did not stop cleanly.
        let pid = rustix::process::Pid::from_raw(live.pid as i32)?;
        rustix::process::test_kill_process(pid).ok()?;
        Some(live)
    }
}

fn path(root: &Path) -> PathBuf {
    root.join(STATE_DIR).join("live.json")
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// Keeps `live.json` current for one run; removes it when dropped.
pub(crate) struct Reporter {
    path: PathBuf,
    last: Live,
}

impl Reporter {
    pub fn new(root: &Path, volume: &str, hub: Option<&str>) -> Reporter {
        Reporter {
            path: path(root),
            last: Live {
                pid: std::process::id(),
                started: now(),
                volume: volume.to_owned(),
                hub: hub.map(str::to_owned),
                ..Live::default()
            },
        }
    }

    /// Writes the state if it changed since the last write.
    pub fn update(&mut self, m: &SyncMetrics) {
        m.check_in_sync();
        let get = |g: &rustfuse_hub::metrics::Gauge| g.get().max(0) as u64;
        let live = Live {
            connected: m.connected.get() == 1,
            applied: get(&m.applied),
            hub_head: get(&m.hub_head),
            outbox: get(&m.outbox),
            in_sync_at: get(&m.in_sync_at),
            ..self.last.clone()
        };
        if live == self.last && self.path.exists() {
            return;
        }
        let tmp = self.path.with_extension("json.tmp");
        let res = serde_json::to_vec(&live)
            .map_err(std::io::Error::other)
            .and_then(|data| std::fs::write(&tmp, data))
            .and_then(|()| std::fs::rename(&tmp, &self.path));
        match res {
            Ok(()) => self.last = live,
            Err(e) => tracing::debug!("cannot write {}: {e}", self.path.display()),
        }
    }
}

impl Drop for Reporter {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn written_while_running_and_gone_after() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join(STATE_DIR)).unwrap();
        assert_eq!(Live::read(dir.path()), None);
        let m = SyncMetrics::default();
        let mut r = Reporter::new(dir.path(), "v", Some("hub:7420"));
        m.connected.set(1);
        m.position(7, 2);
        r.update(&m);
        let live = Live::read(dir.path()).unwrap();
        assert_eq!(
            (live.pid, live.connected, live.applied, live.hub_head, live.outbox),
            (std::process::id(), true, 7, 7, 2)
        );
        assert!(!live.in_sync());
        m.position(7, 0);
        r.update(&m);
        assert!(Live::read(dir.path()).unwrap().in_sync());
        drop(r);
        assert_eq!(Live::read(dir.path()), None);

        // A sync that died without cleaning up does not count.
        let dead = Live {
            pid: i32::MAX as u32,
            ..Live::default()
        };
        std::fs::write(path(dir.path()), serde_json::to_vec(&dead).unwrap()).unwrap();
        assert_eq!(Live::read(dir.path()), None);
    }
}
