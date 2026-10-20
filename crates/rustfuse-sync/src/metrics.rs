//! What a syncing machine reports at `/metrics` (`rustfuse sync --metrics`).

use std::time::{SystemTime, UNIX_EPOCH};

use rustfuse_hub::metrics::{Counter, Family, Gauge, Histogram, Text};

pub struct SyncMetrics {
    /// 1 while connected to the hub.
    pub connected: Gauge,
    /// Last changeset applied here.
    pub applied: Gauge,
    /// Last changeset known to be in the log (from the hub).
    pub hub_head: Gauge,
    /// Local changesets the hub has not accepted yet.
    pub outbox: Gauge,
    /// When this machine was last connected, up to date and had nothing
    /// left to send (Unix seconds).
    pub in_sync_at: Gauge,
    pub pushed_total: Counter,
    pub received_total: Counter,
    pub captures_total: Counter,
    pub captured_ops_total: Counter,
    pub bytes_read_total: Counter,
    pub bytes_uploaded_total: Counter,
    pub packs_uploaded_total: Counter,
    pub packs_spooled_total: Counter,
    pub capture_seconds: Histogram,
    pub apply_seconds: Histogram,
    /// Failures by stage (`connect`, `push`, `capture`, `apply`, `git`).
    pub errors_total: Family<Counter>,
}

impl Default for SyncMetrics {
    fn default() -> Self {
        SyncMetrics {
            connected: Gauge::default(),
            applied: Gauge::default(),
            hub_head: Gauge::default(),
            outbox: Gauge::default(),
            in_sync_at: Gauge::default(),
            pushed_total: Counter::default(),
            received_total: Counter::default(),
            captures_total: Counter::default(),
            captured_ops_total: Counter::default(),
            bytes_read_total: Counter::default(),
            bytes_uploaded_total: Counter::default(),
            packs_uploaded_total: Counter::default(),
            packs_spooled_total: Counter::default(),
            capture_seconds: Histogram::default(),
            apply_seconds: Histogram::default(),
            errors_total: Family::new("stage"),
        }
    }
}

impl SyncMetrics {
    /// Updates the position gauges, and the in-sync time when there is
    /// nothing left to do.
    pub(crate) fn position(&self, applied: u64, outbox: usize) {
        self.applied.set(applied as i64);
        self.hub_head.max(applied as i64);
        self.outbox.set(outbox as i64);
        self.check_in_sync();
    }

    fn check_in_sync(&self) {
        if self.connected.get() == 1 && self.outbox.get() == 0 && self.applied.get() >= self.hub_head.get() {
            let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
            self.in_sync_at.set(now.as_secs() as i64);
        }
    }

    pub fn render(&self) -> String {
        self.check_in_sync();
        Text::default()
            .metric(
                "rustfuse_connected",
                "1 while connected to the hub.",
                &self.connected,
            )
            .metric(
                "rustfuse_applied_seq",
                "Last changeset applied on this machine.",
                &self.applied,
            )
            .metric(
                "rustfuse_hub_head_seq",
                "Last changeset known to be in the log.",
                &self.hub_head,
            )
            .metric(
                "rustfuse_outbox_changesets",
                "Local changesets not yet accepted by the hub.",
                &self.outbox,
            )
            .metric(
                "rustfuse_in_sync_timestamp_seconds",
                "When this machine was last up to date with nothing left to send.",
                &self.in_sync_at,
            )
            .metric(
                "rustfuse_changesets_pushed_total",
                "Changesets sent to the hub.",
                &self.pushed_total,
            )
            .metric(
                "rustfuse_changesets_received_total",
                "Changesets applied from the log.",
                &self.received_total,
            )
            .metric(
                "rustfuse_captures_total",
                "Scans that found local changes.",
                &self.captures_total,
            )
            .metric(
                "rustfuse_captured_ops_total",
                "Local changes captured.",
                &self.captured_ops_total,
            )
            .metric(
                "rustfuse_read_bytes_total",
                "File bytes read to capture changes.",
                &self.bytes_read_total,
            )
            .metric(
                "rustfuse_uploaded_bytes_total",
                "Pack bytes uploaded.",
                &self.bytes_uploaded_total,
            )
            .metric(
                "rustfuse_packs_uploaded_total",
                "Packs uploaded.",
                &self.packs_uploaded_total,
            )
            .metric(
                "rustfuse_packs_spooled_total",
                "Packs kept locally because the bucket was unreachable.",
                &self.packs_spooled_total,
            )
            .metric(
                "rustfuse_capture_seconds",
                "Time to capture local changes.",
                &self.capture_seconds,
            )
            .metric(
                "rustfuse_apply_seconds",
                "Time to apply a batch of changesets from the log.",
                &self.apply_seconds,
            )
            .family("rustfuse_errors_total", "Failures, by stage.", &self.errors_total)
            .finish()
    }
}
