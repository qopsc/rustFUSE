//! Prometheus metrics: counters, gauges and histograms kept in atomics,
//! written in the text exposition format and served over plain HTTP
//! (`GET /metrics`). Small on purpose: no registry, each component keeps
//! its own struct of metrics and writes it out with [`Text`].

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tracing::debug;

/// Default upper bounds, in seconds, of histogram buckets.
pub const LATENCY_BUCKETS: &[f64] = &[
    0.0005, 0.001, 0.0025, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0,
];

/// Something that can be written as one or more samples.
pub trait Metric: Default {
    const KIND: &'static str;
    /// Writes the samples of `name`; `labels` is `k="v"` pairs, maybe empty.
    fn write(&self, name: &str, labels: &str, out: &mut String);
}

#[derive(Default)]
pub struct Counter(AtomicU64);

impl Counter {
    pub fn inc(&self) {
        self.add(1);
    }
    pub fn add(&self, n: u64) {
        self.0.fetch_add(n, Ordering::Relaxed);
    }
    pub fn get(&self) -> u64 {
        self.0.load(Ordering::Relaxed)
    }
}

impl Metric for Counter {
    const KIND: &'static str = "counter";
    fn write(&self, name: &str, labels: &str, out: &mut String) {
        let _ = writeln!(out, "{name}{} {}", braces(labels), self.get());
    }
}

#[derive(Default)]
pub struct Gauge(AtomicI64);

impl Gauge {
    pub fn set(&self, v: i64) {
        self.0.store(v, Ordering::Relaxed);
    }
    pub fn inc(&self) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }
    pub fn dec(&self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
    /// Raises the value to `v` if it is lower.
    pub fn max(&self, v: i64) {
        self.0.fetch_max(v, Ordering::Relaxed);
    }
    pub fn get(&self) -> i64 {
        self.0.load(Ordering::Relaxed)
    }
}

impl Metric for Gauge {
    const KIND: &'static str = "gauge";
    fn write(&self, name: &str, labels: &str, out: &mut String) {
        let _ = writeln!(out, "{name}{} {}", braces(labels), self.get());
    }
}

/// Durations in fixed buckets ([`LATENCY_BUCKETS`]).
pub struct Histogram {
    buckets: Vec<AtomicU64>,
    count: AtomicU64,
    sum_nanos: AtomicU64,
}

impl Default for Histogram {
    fn default() -> Self {
        Histogram {
            buckets: LATENCY_BUCKETS.iter().map(|_| AtomicU64::new(0)).collect(),
            count: AtomicU64::new(0),
            sum_nanos: AtomicU64::new(0),
        }
    }
}

impl Histogram {
    pub fn observe(&self, d: Duration) {
        let secs = d.as_secs_f64();
        // Buckets are stored non-cumulative and summed when written.
        if let Some(i) = LATENCY_BUCKETS.iter().position(|&b| secs <= b) {
            self.buckets[i].fetch_add(1, Ordering::Relaxed);
        }
        self.count.fetch_add(1, Ordering::Relaxed);
        let nanos = u64::try_from(d.as_nanos()).unwrap_or(u64::MAX);
        self.sum_nanos.fetch_add(nanos, Ordering::Relaxed);
    }

    pub fn count(&self) -> u64 {
        self.count.load(Ordering::Relaxed)
    }
}

impl Metric for Histogram {
    const KIND: &'static str = "histogram";
    fn write(&self, name: &str, labels: &str, out: &mut String) {
        let sep = if labels.is_empty() { "" } else { "," };
        let mut cum = 0;
        for (b, n) in LATENCY_BUCKETS.iter().zip(&self.buckets) {
            cum += n.load(Ordering::Relaxed);
            let _ = writeln!(out, "{name}_bucket{{{labels}{sep}le=\"{b}\"}} {cum}");
        }
        let count = self.count();
        let _ = writeln!(out, "{name}_bucket{{{labels}{sep}le=\"+Inf\"}} {count}");
        let sum = self.sum_nanos.load(Ordering::Relaxed) as f64 / 1e9;
        let _ = writeln!(out, "{name}_sum{} {sum}", braces(labels));
        let _ = writeln!(out, "{name}_count{} {count}", braces(labels));
    }
}

/// One metric per value of a label, created on first use.
pub struct Family<T> {
    label: &'static str,
    items: Mutex<BTreeMap<String, Arc<T>>>,
}

impl<T: Metric> Family<T> {
    pub fn new(label: &'static str) -> Self {
        Family {
            label,
            items: Mutex::new(BTreeMap::new()),
        }
    }

    pub fn get(&self, value: &str) -> Arc<T> {
        let mut items = self.items.lock().unwrap();
        if let Some(m) = items.get(value) {
            return m.clone();
        }
        items.entry(value.to_owned()).or_default().clone()
    }
}

/// The text exposition format, built up one metric at a time.
#[derive(Default)]
pub struct Text(String);

impl Text {
    pub fn metric<T: Metric>(&mut self, name: &str, help: &str, m: &T) -> &mut Self {
        self.header(name, help, T::KIND);
        m.write(name, "", &mut self.0);
        self
    }

    pub fn family<T: Metric>(&mut self, name: &str, help: &str, f: &Family<T>) -> &mut Self {
        self.header(name, help, T::KIND);
        for (v, m) in f.items.lock().unwrap().iter() {
            m.write(name, &format!("{}=\"{}\"", f.label, escape(v)), &mut self.0);
        }
        self
    }

    pub fn finish(&mut self) -> String {
        std::mem::take(&mut self.0)
    }

    fn header(&mut self, name: &str, help: &str, kind: &str) {
        let _ = writeln!(self.0, "# HELP {name} {help}");
        let _ = writeln!(self.0, "# TYPE {name} {kind}");
    }
}

fn braces(labels: &str) -> String {
    if labels.is_empty() {
        String::new()
    } else {
        format!("{{{labels}}}")
    }
}

fn escape(v: &str) -> String {
    v.replace('\\', "\\\\").replace('"', "\\\"").replace('\n', "\\n")
}

/// Answers `GET /metrics` (and `GET /`) with `render()` until the listener
/// fails. Plain HTTP/1.1, one request per connection.
pub async fn serve<F>(listener: TcpListener, render: F) -> std::io::Result<()>
where
    F: Fn() -> String + Send + Sync + 'static,
{
    let render = Arc::new(render);
    loop {
        let (mut stream, peer) = listener.accept().await?;
        let render = render.clone();
        tokio::spawn(async move {
            let res = tokio::time::timeout(Duration::from_secs(10), async {
                let mut buf = Vec::with_capacity(1024);
                let mut chunk = [0u8; 1024];
                while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
                    let n = stream.read(&mut chunk).await?;
                    if n == 0 || buf.len() > 8192 {
                        return Ok(());
                    }
                    buf.extend_from_slice(&chunk[..n]);
                }
                let line = buf.split(|&b| b == b'\r').next().unwrap_or_default();
                let mut parts = line.split(|&b| b == b' ');
                let (method, path) = (parts.next(), parts.next());
                let path = path.map(|p| p.split(|&b| b == b'?').next().unwrap_or_default());
                let (status, body) = match (method, path) {
                    (Some(b"GET"), Some(b"/metrics" | b"/")) => ("200 OK", render()),
                    (Some(b"GET"), _) => ("404 Not Found", "not found\n".to_owned()),
                    _ => ("405 Method Not Allowed", "only GET\n".to_owned()),
                };
                let head = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: text/plain; version=0.0.4; charset=utf-8\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                stream.write_all(head.as_bytes()).await?;
                stream.write_all(body.as_bytes()).await?;
                stream.shutdown().await
            })
            .await;
            if let Ok(Err(e)) = res {
                debug!(%peer, "metrics request failed: {e}");
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_format() {
        let c = Counter::default();
        c.add(3);
        let f: Family<Gauge> = Family::new("volume");
        f.get("b").set(7);
        f.get("a\"x").set(-1);
        let h = Histogram::default();
        h.observe(Duration::from_millis(3));
        h.observe(Duration::from_secs(60));
        let out = Text::default()
            .metric("t_total", "A counter.", &c)
            .family("t_head", "A gauge.", &f)
            .metric("t_seconds", "A histogram.", &h)
            .finish();
        assert!(out.contains("# TYPE t_total counter\nt_total 3\n"), "{out}");
        assert!(
            out.contains("t_head{volume=\"a\\\"x\"} -1\nt_head{volume=\"b\"} 7\n"),
            "{out}"
        );
        assert!(out.contains("t_seconds_bucket{le=\"0.0025\"} 0\n"), "{out}");
        assert!(out.contains("t_seconds_bucket{le=\"0.005\"} 1\n"), "{out}");
        assert!(out.contains("t_seconds_bucket{le=\"10\"} 1\n"), "{out}");
        assert!(out.contains("t_seconds_bucket{le=\"+Inf\"} 2\n"), "{out}");
        assert!(out.contains("t_seconds_count 2\n"), "{out}");
        assert!(out.contains("t_seconds_sum 60.003\n"), "{out}");
    }

    #[tokio::test]
    async fn serves_over_http() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(serve(listener, || "x_total 1\n".to_owned()));
        for (req, want) in [
            ("GET /metrics HTTP/1.1\r\nHost: x\r\n\r\n", "200 OK"),
            ("GET /nope HTTP/1.1\r\n\r\n", "404"),
            ("POST /metrics HTTP/1.1\r\n\r\n", "405"),
        ] {
            let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
            s.write_all(req.as_bytes()).await.unwrap();
            let mut resp = String::new();
            s.read_to_string(&mut resp).await.unwrap();
            assert!(resp.starts_with(&format!("HTTP/1.1 {want}")), "{resp}");
            if want == "200 OK" {
                assert!(resp.ends_with("\r\n\r\nx_total 1\n"), "{resp}");
            }
        }
    }
}
