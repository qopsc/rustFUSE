//! `rustfuse bench`: raw object-store throughput and latency.

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use bytes::Bytes;
use clap::Args;
use futures::{StreamExt, stream};
use rustfuse_store::ObjectStore;

use crate::{Credentials, StoreArgs};

#[derive(Args)]
pub struct BenchArgs {
    #[command(flatten)]
    store: StoreArgs,
    #[command(flatten)]
    creds: Credentials,
    /// Total bytes to write and read back (MiB).
    #[arg(long, default_value_t = 256)]
    size_mib: u64,
    /// Object size in KiB.
    #[arg(long, default_value_t = 4096)]
    block_kib: u32,
    /// Concurrent requests in flight.
    #[arg(long, default_value_t = 32)]
    concurrency: usize,
    /// Leave the benchmark objects in the bucket.
    #[arg(long)]
    keep: bool,
}

pub async fn bench(args: BenchArgs) -> Result<()> {
    let store: Arc<dyn ObjectStore> = Arc::new(args.store.connect(&args.creds, "").await?);
    let block_size = u64::from(args.block_kib) * 1024;
    let count = ((args.size_mib << 20) / block_size).max(1);
    let run = std::process::id();
    let keys: Vec<String> = (0..count).map(|i| format!("bench/{run}/{i:08}")).collect();
    let payload = Bytes::from(vec![0xa5u8; block_size as usize]);
    let total_bytes = count * block_size;

    println!(
        "{} objects of {} KiB, concurrency {}",
        keys.len(),
        args.block_kib,
        args.concurrency
    );

    let put = run_phase(&keys, args.concurrency, |key| {
        let store = store.clone();
        let data = payload.clone();
        async move { store.put(&key, data).await.map(|_| ()) }
    })
    .await?;
    put.report("PUT", total_bytes);

    let get = run_phase(&keys, args.concurrency, |key| {
        let store = store.clone();
        async move { store.get(&key).await.map(|_| ()) }
    })
    .await?;
    get.report("GET", total_bytes);

    let ranged = run_phase(&keys, args.concurrency, |key| {
        let store = store.clone();
        async move { store.get_range(&key, 0..4096).await.map(|_| ()) }
    })
    .await?;
    ranged.report("GET 4 KiB range", 4096 * keys.len() as u64);

    if !args.keep {
        run_phase(&keys, args.concurrency, |key| {
            let store = store.clone();
            async move { store.delete(&key).await }
        })
        .await?;
    }
    Ok(())
}

struct PhaseStats {
    elapsed: Duration,
    latencies: Vec<Duration>,
}

impl PhaseStats {
    fn report(&self, name: &str, bytes: u64) {
        let secs = self.elapsed.as_secs_f64();
        let pct = |p: f64| {
            let i = ((self.latencies.len() as f64 - 1.0) * p).round() as usize;
            self.latencies[i].as_secs_f64() * 1e3
        };
        println!(
            "{name:<16} {:>9.1} MiB/s {:>8.0} ops/s   p50 {:>7.2} ms  p99 {:>7.2} ms  max {:>7.2} ms",
            bytes as f64 / (1 << 20) as f64 / secs,
            self.latencies.len() as f64 / secs,
            pct(0.50),
            pct(0.99),
            pct(1.0),
        );
    }
}

async fn run_phase<F, Fut>(keys: &[String], concurrency: usize, op: F) -> Result<PhaseStats>
where
    F: Fn(String) -> Fut,
    Fut: Future<Output = rustfuse_store::Result<()>>,
{
    let start = Instant::now();
    let mut latencies: Vec<Duration> = stream::iter(keys.iter().cloned())
        .map(|key| {
            let fut = op(key);
            async move {
                let t = Instant::now();
                fut.await.map(|_| t.elapsed())
            }
        })
        .buffer_unordered(concurrency.max(1))
        .collect::<Vec<_>>()
        .await
        .into_iter()
        .collect::<Result<_, _>>()?;
    latencies.sort();
    Ok(PhaseStats {
        elapsed: start.elapsed(),
        latencies,
    })
}
