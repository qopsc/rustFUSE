use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand};
use rustfuse_store::{ObjectStore, S3Config, S3Store};
use rustfuse_sync::{Config, HubSpec, Replica};
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
    /// Run the hub that orders changes for every volume in a bucket.
    Hub {
        /// Address to listen on.
        #[arg(long, default_value_t = format!("0.0.0.0:{}", rustfuse_hub::DEFAULT_PORT))]
        listen: String,
        #[command(flatten)]
        store: StoreArgs,
        #[command(flatten)]
        creds: Credentials,
        #[command(flatten)]
        token: Token,
    },
    /// Create a volume from an existing directory and upload it.
    Init {
        dir: PathBuf,
        /// Volume name, unique within the bucket and prefix.
        #[arg(long)]
        volume: String,
        #[command(flatten)]
        hub: HubArgs,
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
        hub: HubArgs,
        #[command(flatten)]
        store: StoreArgs,
        #[command(flatten)]
        creds: Credentials,
    },
    /// Keep a directory in sync: send local changes and apply everyone
    /// else's as they happen. Keeps working offline.
    Sync {
        dir: PathBuf,
        /// Sync once and exit instead of running continuously.
        #[arg(long)]
        once: bool,
        /// Quiet period before capturing a burst of changes.
        #[arg(long, default_value_t = 200)]
        debounce_ms: u64,
        /// Full rescan interval, to catch missed file events.
        #[arg(long, default_value_t = 600)]
        full_scan_secs: u64,
        #[command(flatten)]
        creds: Credentials,
        #[command(flatten)]
        token: Token,
    },
    /// Show the volume, sync position and local changes not yet captured.
    Status { dir: PathBuf },
    /// List conflict copies kept next to the files they conflicted with,
    /// and git branches that diverged on two machines.
    Conflicts { dir: PathBuf },
    /// Measure raw object-store throughput and latency.
    Bench(bench::BenchArgs),
    /// Make `git status` in a repository inside a synced directory ask the
    /// running `rustfuse sync` what changed instead of checking every file.
    /// Needs `rustfuse` on the PATH of every machine that uses the repo.
    GitFsmonitor {
        /// The repository (or one of its linked worktrees).
        repo: PathBuf,
        /// Turn it off again.
        #[arg(long)]
        off: bool,
    },
    /// The hook git runs (`core.fsmonitor`); not meant to be run by hand.
    #[command(hide = true)]
    Fsmonitor {
        version: u32,
        #[arg(default_value = "")]
        token: String,
    },
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

/// Shared secret between the hub and machines; never written to disk.
#[derive(Args)]
struct Token {
    #[arg(long = "hub-token", env = "RUSTFUSE_HUB_TOKEN", hide_env_values = true)]
    hub_token: Option<String>,
}

impl Token {
    fn get(&self) -> Result<String> {
        match &self.hub_token {
            Some(t) if !t.is_empty() => Ok(t.clone()),
            _ => bail!("set the hub token with --hub-token or RUSTFUSE_HUB_TOKEN"),
        }
    }
}

#[derive(Args)]
struct HubArgs {
    /// Hub address, host:port.
    #[arg(long, env = "RUSTFUSE_HUB")]
    hub: String,
    #[command(flatten)]
    token: Token,
}

impl HubArgs {
    fn spec(&self) -> Result<HubSpec> {
        Ok(HubSpec {
            addr: with_port(&self.hub),
            token: self.token.get()?,
        })
    }
}

fn with_port(addr: &str) -> String {
    if addr
        .rsplit_once(':')
        .is_some_and(|(_, p)| p.parse::<u16>().is_ok())
    {
        addr.to_owned()
    } else {
        format!("{addr}:{}", rustfuse_hub::DEFAULT_PORT)
    }
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

    fn config(&self, volume: String, hub: &HubArgs) -> Result<Config> {
        if !rustfuse_hub::valid_volume_name(&volume) {
            bail!("volume names may only use letters, digits, '-', '_' and '.'");
        }
        let mut seed = [0u8; 16];
        getrandom::fill(&mut seed).map_err(|e| anyhow::anyhow!("no randomness: {e}"))?;
        Ok(Config {
            volume,
            bucket: self.bucket.clone(),
            endpoint: self.endpoint.clone(),
            region: self.region.clone(),
            prefix: self.prefix.clone(),
            path_style: !self.virtual_hosted,
            hub: Some(with_port(&hub.hub)),
            replica: ReplicaId(getrandom::u64().map_err(|e| anyhow::anyhow!("no randomness: {e}"))?),
            seed,
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

fn mib(bytes: u64) -> String {
    format!("{:.1} MiB", bytes as f64 / (1 << 20) as f64)
}

const SYNC_TIMEOUT: Duration = Duration::from_secs(120);

/// One sync round after init/attach; failing it is not fatal, since `sync`
/// catches up later.
async fn first_sync(r: &mut Replica, hub: &HubSpec) {
    match r.sync_once(hub, SYNC_TIMEOUT).await {
        Ok(_) => println!("In sync at changeset {}.", r.applied()),
        Err(e) => println!("Could not reach the hub ({e}); run `rustfuse sync` to send changes later."),
    }
}

fn describe_changes(dir: &Path, r: &Replica) -> Result<()> {
    let pending = r.pending_changes()?;
    if pending.is_empty() && r.outbox_len() == 0 {
        println!("Everything is synced.");
        return Ok(());
    }
    if r.outbox_len() > 0 {
        println!("{} changesets waiting to be sent to the hub.", r.outbox_len());
    }
    if !pending.is_empty() {
        println!("{} changes in {} not yet captured:", pending.len(), dir.display());
        for c in pending.iter().take(50) {
            println!("  {}", c.path().display());
        }
        if pending.len() > 50 {
            println!("  ...");
        }
    }
    Ok(())
}

/// Conflict copies under `root` (relative paths, sorted), skipping what
/// does not sync.
fn find_conflicts(root: &Path) -> Result<Vec<PathBuf>> {
    let ignore = rustfuse_sync::Ignore::load(root).context("reading ignore rules")?;
    let mut found = Vec::new();
    let mut stack = vec![PathBuf::new()];
    while let Some(rel) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(root.join(&rel)) else {
            continue;
        };
        for e in rd.flatten() {
            let p = rel.join(e.file_name());
            let is_dir = e.file_type().is_ok_and(|t| t.is_dir());
            if ignore.is_ignored(&p, is_dir) {
                continue;
            }
            if rustfuse_sync::original_name(e.file_name().as_encoded_bytes()).is_some() {
                found.push(p.clone());
            }
            if is_dir {
                stack.push(p);
            }
        }
    }
    found.sort();
    Ok(found)
}

/// git's fsmonitor hook value; the config syncs to every machine, so it
/// names the binary rather than a machine-specific path.
const FSMONITOR_HOOK: &str = "rustfuse fsmonitor";

fn git_fsmonitor(repo: &Path, off: bool) -> Result<()> {
    let settings: &[(&str, &str)] = &[
        ("core.fsmonitor", FSMONITOR_HOOK),
        ("core.fsmonitorHookVersion", "2"),
        ("core.untrackedCache", "true"),
    ];
    for (key, value) in settings {
        let mut cmd = std::process::Command::new("git");
        cmd.arg("-C").arg(repo).arg("config");
        if off {
            cmd.args(["--unset", key]);
        } else {
            cmd.args([key, value]);
        }
        let out = cmd.output().context("running git")?;
        // Exit code 5 from --unset: the key was not set.
        if !out.status.success() && !(off && out.status.code() == Some(5)) {
            bail!(
                "git config {key}: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
    }
    if off {
        println!("git status in {} checks every file again.", repo.display());
    } else {
        println!(
            "git status in {} now asks `rustfuse sync` what changed; without a running sync it checks every file as before.",
            repo.display()
        );
    }
    Ok(())
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    // The hook runs on every `git status`: answer without starting a runtime.
    if let Command::Fsmonitor { version, token } = &cli.command {
        let cwd = std::env::current_dir()?;
        let out = rustfuse_sync::fsmonitor::hook(*version, token, &cwd);
        std::io::Write::write_all(&mut std::io::stdout().lock(), &out)?;
        return Ok(());
    }
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(run(cli))
}

async fn run(cli: Cli) -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();
    match cli.command {
        Command::Hub {
            listen,
            store,
            creds,
            token,
        } => {
            let root: Arc<dyn ObjectStore> = Arc::new(store.connect(&creds, &store.prefix).await?);
            let listener = tokio::net::TcpListener::bind(&listen)
                .await
                .with_context(|| format!("listen on {listen}"))?;
            println!("Hub listening on {}.", listener.local_addr()?);
            let hub = rustfuse_hub::Hub::new(root, token.get()?);
            tokio::select! {
                res = hub.serve(listener) => res?,
                _ = tokio::signal::ctrl_c() => {}
            }
        }
        Command::Init {
            dir,
            volume,
            hub,
            store,
            creds,
        } => {
            let spec = hub.spec()?;
            let config = store.config(volume, &hub)?;
            let s = volume_store(&config, &creds).await?;
            let dir = dir.canonicalize().with_context(|| format!("{}", dir.display()))?;
            let (mut r, stats) = Replica::init(&dir, config, s).await?;
            println!(
                "Created volume {}: {} entries, {} read, {} uploaded in {} packs.",
                r.config().volume,
                r.tree().len(),
                mib(stats.bytes_read),
                mib(stats.bytes_uploaded),
                stats.packs
            );
            first_sync(&mut r, &spec).await;
        }
        Command::Attach {
            dir,
            volume,
            hub,
            store,
            creds,
        } => {
            let spec = hub.spec()?;
            let config = store.config(volume, &hub)?;
            let s = volume_store(&config, &creds).await?;
            let mut r = Replica::attach(&dir, config, s).await?;
            println!(
                "Attached volume {} at {} ({} entries).",
                r.config().volume,
                dir.display(),
                r.tree().len(),
            );
            first_sync(&mut r, &spec).await;
        }
        Command::Sync {
            dir,
            once,
            debounce_ms,
            full_scan_secs,
            creds,
            token,
        } => {
            let config = Replica::config_of(&dir)?;
            let Some(addr) = config.hub.clone() else {
                bail!("{} has no hub configured", dir.display());
            };
            let spec = HubSpec {
                addr,
                token: token.get()?,
            };
            let mut r = Replica::open(&dir, volume_store(&config, &creds).await?)?;
            if once {
                let s = r.sync_once(&spec, SYNC_TIMEOUT).await?;
                println!(
                    "Sent {} changesets, applied {}; now at changeset {}.",
                    s.pushed,
                    s.received,
                    r.applied()
                );
            } else {
                println!("Syncing {} with {} (Ctrl-C to stop)...", dir.display(), spec.addr);
                tokio::select! {
                    res = r.run(&spec, Duration::from_millis(debounce_ms), Duration::from_secs(full_scan_secs)) => res?,
                    _ = tokio::signal::ctrl_c() => {}
                }
            }
        }
        Command::Status { dir } => {
            let config = Replica::config_of(&dir)?;
            // Status only reads local state; no store access needed.
            let r = Replica::open(&dir, Arc::new(rustfuse_store::MemoryStore::new()))?;
            println!(
                "Volume {} via hub {}, at changeset {}, {} entries.",
                config.volume,
                config.hub.as_deref().unwrap_or("(none)"),
                r.applied(),
                r.tree().len()
            );
            describe_changes(&dir, &r)?;
        }
        Command::Conflicts { dir } => {
            // Reads the disk, not the index, so it works while `sync` runs.
            if !dir.join(rustfuse_sync::STATE_DIR).is_dir() {
                bail!("{} is not a rustfuse directory", dir.display());
            }
            let found = find_conflicts(&dir)?;
            let mut any = false;
            for p in found {
                // Branches that diverged on two machines; other copies
                // inside .git are handled by the git pass.
                let s = p.to_string_lossy();
                if let Some((repo, branch)) = s
                    .split_once(".git/refs/heads/")
                    .or_else(|| s.strip_prefix(".git/refs/heads/").map(|b| ("", b)))
                {
                    let repo = if repo.is_empty() {
                        ".".into()
                    } else {
                        repo.trim_end_matches('/').to_owned()
                    };
                    println!(
                        "branch {branch} in {repo}: diverged on two machines; merge it with `git merge {branch}`"
                    );
                    any = true;
                } else if !p.components().any(|c| c.as_os_str() == ".git") {
                    println!("{}", p.display());
                    any = true;
                }
            }
            if !any {
                println!("No conflicts.");
            }
        }
        Command::Bench(args) => bench::bench(args).await?,
        Command::GitFsmonitor { repo, off } => git_fsmonitor(&repo, off)?,
        Command::Fsmonitor { .. } => unreachable!("handled before the runtime starts"),
    }
    Ok(())
}
