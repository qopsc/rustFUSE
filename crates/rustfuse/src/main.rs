use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use bytes::Bytes;
use clap::{Args, Parser, Subcommand};
use futures::{StreamExt, stream};
use rustfuse_store::{ObjectStore, S3Config, S3Store};
use rustfuse_types::{DEFAULT_BLOCK_SIZE, SliceId, blocks_of_slice};

#[derive(Parser)]
#[command(version, about = "Mount S3-compatible object storage as a filesystem")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Measure raw object-store throughput and latency with block-sized objects.
    Bench(BenchArgs),
}

#[derive(Args)]
struct StoreArgs {
    /// Bucket name.
    #[arg(long, env = "RUSTFUSE_BUCKET")]
    bucket: String,
    /// S3 endpoint, e.g. http://localhost:9000. Defaults to AWS.
    #[arg(long, env = "RUSTFUSE_ENDPOINT")]
    endpoint: Option<String>,
    #[arg(long, env = "RUSTFUSE_REGION", default_value = "us-east-1")]
    region: String,
    /// Key prefix inside the bucket.
    #[arg(long, env = "RUSTFUSE_PREFIX", default_value = "")]
    prefix: String,
    #[arg(long, env = "RUSTFUSE_ACCESS_KEY")]
    access_key: Option<String>,
    #[arg(long, env = "RUSTFUSE_SECRET_KEY", hide_env_values = true)]
    secret_key: Option<String>,
    /// Use virtual-hosted-style addressing instead of path-style.
    #[arg(long)]
    virtual_hosted: bool,
}

impl StoreArgs {
    async fn connect(&self) -> Result<S3Store> {
        let mut cfg = S3Config::new(&self.bucket);
        cfg.endpoint = self.endpoint.clone();
        cfg.region = self.region.clone();
        cfg.prefix = self.prefix.clone();
        cfg.access_key = self.access_key.clone();
        cfg.secret_key = self.secret_key.clone();
        cfg.path_style = !self.virtual_hosted;
        Ok(S3Store::connect(cfg).await?)
    }
}

#[derive(Args)]
struct BenchArgs {
    #[command(flatten)]
    store: StoreArgs,
    /// Total bytes to write and read back (MiB).
    #[arg(long, default_value_t = 256)]
    size_mib: u64,
    /// Object (block) size in KiB.
    #[arg(long, default_value_t = DEFAULT_BLOCK_SIZE / 1024)]
    block_kib: u32,
    /// Concurrent requests in flight.
    #[arg(long, default_value_t = 32)]
    concurrency: usize,
    /// Leave the benchmark objects in the bucket.
    #[arg(long)]
    keep: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
    match Cli::parse().command {
        Command::Bench(args) => bench(args).await,
    }
}

async fn bench(args: BenchArgs) -> Result<()> {
    let store: Arc<dyn ObjectStore> = Arc::new(args.store.connect().await?);
    let block_size = args.block_kib * 1024;
    // A synthetic slice id far from anything a real filesystem would allocate.
    let slice = SliceId(u64::MAX - u64::from(std::process::id()));
    let blocks = blocks_of_slice(slice, args.size_mib << 20, block_size);
    let keys: Vec<String> = blocks
        .iter()
        .map(|b| format!("bench/{}", b.object_key()))
        .collect();
    let payload = Bytes::from(vec![0xa5u8; block_size as usize]);
    let total_bytes: u64 = blocks.iter().map(|b| u64::from(b.len)).sum();

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
