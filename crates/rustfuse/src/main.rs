use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand};
use rustfuse_store::{ObjectStore, S3Config, S3Store};
use rustfuse_sync::{Config, Replica, Role};
use rustfuse_types::ReplicaId;

mod bench;

#[derive(Parser)]
#[command(
    version,
    about = "Keep a directory in sync across machines through S3-compatible storage"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Create a volume from an existing directory and upload it.
    Init {
        dir: PathBuf,
        /// Volume name, unique within the bucket and prefix.
        #[arg(long)]
        volume: String,
        #[command(flatten)]
        store: StoreArgs,
        #[command(flatten)]
        creds: Credentials,
    },
    /// Download a volume into a new, empty directory.
    Attach {
        dir: PathBuf,
        #[arg(long)]
        volume: String,
        #[command(flatten)]
        store: StoreArgs,
        #[command(flatten)]
        creds: Credentials,
    },
    /// Upload local changes (on the machine that ran `init`).
    Push {
        dir: PathBuf,
        /// Keep running and push changes as they happen.
        #[arg(long)]
        watch: bool,
        /// Quiet period before pushing a burst of changes.
        #[arg(long, default_value_t = 200)]
        debounce_ms: u64,
        /// Full rescan interval while watching, to catch missed events.
        #[arg(long, default_value_t = 600)]
        full_scan_secs: u64,
        #[command(flatten)]
        creds: Credentials,
    },
    /// Apply changes from the volume.
    Pull {
        dir: PathBuf,
        /// Keep running and apply changes as they arrive.
        #[arg(long)]
        follow: bool,
        #[arg(long, default_value_t = 1000)]
        interval_ms: u64,
        #[command(flatten)]
        creds: Credentials,
    },
    /// Show the volume, role, and local changes not yet pushed.
    Status { dir: PathBuf },
    /// Measure raw object-store throughput and latency.
    Bench(bench::BenchArgs),
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
    /// Use virtual-hosted-style addressing instead of path-style.
    #[arg(long)]
    virtual_hosted: bool,
}

/// Credentials are never written to disk; pass them each time (or via the
/// environment, or the standard AWS provider chain).
#[derive(Args)]
struct Credentials {
    #[arg(long, env = "RUSTFUSE_ACCESS_KEY")]
    access_key: Option<String>,
    #[arg(long, env = "RUSTFUSE_SECRET_KEY", hide_env_values = true)]
    secret_key: Option<String>,
}

impl StoreArgs {
    async fn connect(&self, creds: &Credentials, prefix: &str) -> Result<S3Store> {
        connect(
            &self.bucket,
            self.endpoint.clone(),
            &self.region,
            prefix,
            !self.virtual_hosted,
            creds,
        )
        .await
    }

    fn config(&self, volume: String, role: Role) -> Result<Config> {
        let mut seed = [0u8; 16];
        getrandom::fill(&mut seed).map_err(|e| anyhow::anyhow!("no randomness: {e}"))?;
        Ok(Config {
            volume,
            bucket: self.bucket.clone(),
            endpoint: self.endpoint.clone(),
            region: self.region.clone(),
            prefix: self.prefix.clone(),
            path_style: !self.virtual_hosted,
            replica: ReplicaId(getrandom::u64().map_err(|e| anyhow::anyhow!("no randomness: {e}"))?),
            seed,
            role,
        })
    }
}

async fn connect(
    bucket: &str,
    endpoint: Option<String>,
    region: &str,
    prefix: &str,
    path_style: bool,
    creds: &Credentials,
) -> Result<S3Store> {
    let mut cfg = S3Config::new(bucket);
    cfg.endpoint = endpoint;
    cfg.region = region.to_owned();
    cfg.prefix = prefix.to_owned();
    cfg.path_style = path_style;
    cfg.access_key = creds.access_key.clone();
    cfg.secret_key = creds.secret_key.clone();
    Ok(S3Store::connect(cfg).await?)
}

async fn volume_store(config: &Config, creds: &Credentials) -> Result<Arc<dyn ObjectStore>> {
    let s = connect(
        &config.bucket,
        config.endpoint.clone(),
        &config.region,
        &config.volume_prefix(),
        config.path_style,
        creds,
    )
    .await?;
    Ok(Arc::new(s))
}

async fn open(dir: &Path, creds: &Credentials) -> Result<Replica> {
    let config = Replica::config_of(dir)?;
    let store = volume_store(&config, creds).await?;
    Ok(Replica::open(dir, store)?)
}

fn mib(bytes: u64) -> String {
    format!("{:.1} MiB", bytes as f64 / (1 << 20) as f64)
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();
    match Cli::parse().command {
        Command::Init {
            dir,
            volume,
            store,
            creds,
        } => {
            let config = store.config(volume, Role::Writer)?;
            let s = volume_store(&config, &creds).await?;
            let dir = dir.canonicalize().with_context(|| format!("{}", dir.display()))?;
            let (r, stats) = Replica::init(&dir, config, s).await?;
            println!(
                "Created volume {}: {} entries, {} read, {} uploaded in {} packs.",
                r.config().volume,
                r.tree().len(),
                mib(stats.bytes_read),
                mib(stats.bytes_uploaded),
                stats.packs
            );
        }
        Command::Attach {
            dir,
            volume,
            store,
            creds,
        } => {
            let config = store.config(volume, Role::Follower)?;
            let s = volume_store(&config, &creds).await?;
            let (r, _) = Replica::attach(&dir, config, s).await?;
            println!(
                "Attached volume {} at {} ({} entries, changeset {}).",
                r.config().volume,
                dir.display(),
                r.tree().len(),
                r.applied()
            );
        }
        Command::Push {
            dir,
            watch,
            debounce_ms,
            full_scan_secs,
            creds,
        } => {
            let mut r = open(&dir, &creds).await?;
            if watch {
                println!("Watching {} (Ctrl-C to stop)...", dir.display());
                tokio::select! {
                    res = r.watch_push(Duration::from_millis(debounce_ms), Duration::from_secs(full_scan_secs)) => res?,
                    _ = tokio::signal::ctrl_c() => {}
                }
            } else {
                match r.push(None).await? {
                    Some(s) => println!(
                        "Pushed changeset {}: {} changes, {} files read, {} uploaded.",
                        s.seq,
                        s.ops,
                        s.files_read,
                        mib(s.bytes_uploaded)
                    ),
                    None => println!("Nothing to push."),
                }
            }
        }
        Command::Pull {
            dir,
            follow,
            interval_ms,
            creds,
        } => {
            let mut r = open(&dir, &creds).await?;
            if follow {
                println!("Following {} (Ctrl-C to stop)...", r.config().volume);
                tokio::select! {
                    res = r.follow(Duration::from_millis(interval_ms)) => res?,
                    _ = tokio::signal::ctrl_c() => {}
                }
            } else {
                let n = r.pull().await?;
                println!("Applied {n} changesets; now at {}.", r.applied());
            }
        }
        Command::Status { dir } => {
            let config = Replica::config_of(&dir)?;
            // Status only reads local state; no store access needed.
            let r = Replica::open(&dir, Arc::new(rustfuse_store::MemoryStore::new()))?;
            println!(
                "Volume {} ({:?}) at changeset {}, {} entries.",
                config.volume,
                config.role,
                r.applied(),
                r.tree().len()
            );
            let pending = r.pending()?;
            if pending.is_empty() {
                println!("No local changes.");
            } else {
                println!("{} local changes not pushed:", pending.len());
                for c in pending.iter().take(50) {
                    println!("  {}", c.path().display());
                }
                if pending.len() > 50 {
                    println!("  ...");
                }
            }
        }
        Command::Bench(args) => bench::bench(args).await?,
    }
    Ok(())
}
