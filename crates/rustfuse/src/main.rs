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
        /// Serve TLS with this certificate chain (PEM); machines then use
        /// `--hub tls://host`.
        #[arg(long, env = "RUSTFUSE_HUB_TLS_CERT", requires = "tls_key")]
        tls_cert: Option<PathBuf>,
        /// Private key (PEM) for `--tls-cert`.
        #[arg(long, env = "RUSTFUSE_HUB_TLS_KEY", requires = "tls_cert")]
        tls_key: Option<PathBuf>,
        /// Serve Prometheus metrics over plain HTTP at this address
        /// (`GET /metrics`), e.g. 127.0.0.1:9420.
        #[arg(long, env = "RUSTFUSE_HUB_METRICS")]
        metrics: Option<String>,
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
        /// Leave file content in the bucket until a mount (`rustfuse mount`
        /// or `rustfuse nfs`) first opens the file; git data is fetched in
        /// full. Use the directory only through a mount: read directly,
        /// files not fetched yet look empty.
        #[arg(long)]
        lazy: bool,
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
        #[arg(long, conflicts_with = "metrics")]
        once: bool,
        /// Answer the macOS File Provider extension on this Unix socket
        /// (used by the rustfuse Mac app; `docs/design.md` §2.2).
        #[arg(long, conflicts_with = "once")]
        provider_socket: Option<PathBuf>,
        #[command(flatten)]
        opts: SyncOpts,
    },
    /// Mount a synced directory read/write somewhere else (FUSE: Linux, or
    /// macOS with macFUSE and a `macfuse` build)
    /// and keep it in sync. Writes through the mount sync like edits in
    /// the directory itself.
    Mount {
        /// The synced directory (from `init` or `attach`).
        dir: PathBuf,
        /// Where to mount it; an existing, empty directory.
        mountpoint: PathBuf,
        /// Let other users use the mount (non-root users need
        /// `user_allow_other` in /etc/fuse.conf).
        #[arg(long)]
        allow_other: bool,
        /// macOS: use macFUSE's FSKit backend, which needs no kernel
        /// extension (macOS 15.4 or later; mount under /Volumes).
        #[arg(long)]
        fskit: bool,
        /// Only serve the directory; a `rustfuse sync` runs elsewhere.
        #[arg(long)]
        no_sync: bool,
        /// For a directory attached with `--lazy`: keep at most this much
        /// fetched content (e.g. 20G), dropping files not opened lately
        /// back to placeholders. Without it, fetched files stay.
        #[arg(long, value_parser = parse_size)]
        cache_size: Option<u64>,
        #[command(flatten)]
        opts: SyncOpts,
    },
    /// Serve a synced directory read/write over NFSv3 and keep it in sync;
    /// mount it with the system's NFS client (macOS needs nothing extra).
    /// There is no authentication: keep it on loopback or a trusted network.
    Nfs {
        /// The synced directory (from `init` or `attach`).
        dir: PathBuf,
        /// Address to serve NFS on.
        #[arg(long, default_value = "127.0.0.1:20490")]
        listen: String,
        /// Only serve the directory; a `rustfuse sync` runs elsewhere.
        #[arg(long)]
        no_sync: bool,
        /// For a directory attached with `--lazy`: keep at most this much
        /// fetched content (e.g. 20G), dropping files not opened lately
        /// back to placeholders. Without it, fetched files stay.
        #[arg(long, value_parser = parse_size)]
        cache_size: Option<u64>,
        #[command(flatten)]
        opts: SyncOpts,
    },
    /// Show the volume, sync position and local changes not yet captured.
    Status { dir: PathBuf },
    /// List conflict copies kept next to the files they conflicted with,
    /// and git branches that diverged on two machines.
    Conflicts { dir: PathBuf },
    /// Delete stored content no machine can need any more. Run it next to
    /// the hub (it needs the bucket), e.g. daily; a pack is deleted on a
    /// run at least an hour after the run that found it unused.
    Gc {
        #[arg(long)]
        volume: String,
        /// Keep content used by changes from this many days back, and
        /// packs written within it.
        #[arg(long, default_value_t = 14)]
        retention_days: u64,
        /// Minutes between marking a pack and deleting it; machines trust a
        /// pack check for five, so keep this well above that.
        #[arg(long, default_value_t = 60)]
        delay_minutes: u64,
        /// Only report what would be deleted.
        #[arg(long)]
        dry_run: bool,
        #[command(flatten)]
        store: StoreArgs,
        #[command(flatten)]
        creds: Credentials,
    },
    /// Manage per-machine hub keys: each machine gets its own token, which
    /// can be limited to some volumes and revoked on its own. Run it where
    /// the bucket is reachable, like the hub.
    Key {
        #[command(subcommand)]
        action: KeyAction,
        #[command(flatten)]
        store: StoreArgs,
        #[command(flatten)]
        creds: Credentials,
    },
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

#[derive(Subcommand)]
enum KeyAction {
    /// Create a key and print its token (shown only once).
    Create {
        /// What the key is for, e.g. the machine's name.
        #[arg(long)]
        name: String,
        /// Limit the key to this volume; repeat for more. Default: all.
        #[arg(long = "volume")]
        volumes: Vec<String>,
    },
    /// List keys.
    List,
    /// Revoke a key; hubs stop accepting it within 30 seconds, open
    /// connections included.
    Revoke { id: String },
}

#[derive(Args)]
struct StoreArgs {
    /// Bucket name. Leave it out on a machine to read and write the bucket
    /// through the hub, which then needs no S3 credentials or route to the
    /// bucket here.
    #[arg(long, env = "RUSTFUSE_BUCKET")]
    bucket: Option<String>,
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

/// How a running sync behaves.
#[derive(Args)]
struct SyncOpts {
    /// Quiet period before capturing a burst of changes.
    #[arg(long, default_value_t = 200)]
    debounce_ms: u64,
    /// Full rescan interval, to catch missed file events.
    #[arg(long, default_value_t = 600)]
    full_scan_secs: u64,
    /// Serve Prometheus metrics over plain HTTP at this address
    /// (`GET /metrics`), e.g. 127.0.0.1:9421. Not with `--once`.
    #[arg(long, env = "RUSTFUSE_METRICS")]
    metrics: Option<String>,
    #[command(flatten)]
    creds: Credentials,
    #[command(flatten)]
    token: Token,
}

/// Shared secret between the hub and machines, or a machine's own key from
/// `rustfuse key create`; never written to disk. A hub started without one
/// accepts only per-machine keys.
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
    /// Hub address: host[:port], or tls://host[:port] if the hub has a
    /// certificate. List several hubs of the same bucket comma-separated
    /// (tls://a,b) to fail over between them.
    #[arg(long, env = "RUSTFUSE_HUB")]
    hub: String,
    /// CA certificates (PEM) to trust for the hub instead of the system's;
    /// remembered for later syncs.
    #[arg(long, env = "RUSTFUSE_HUB_CA")]
    hub_ca: Option<PathBuf>,
    #[command(flatten)]
    token: Token,
}

impl HubArgs {
    fn spec(&self) -> Result<HubSpec> {
        hub_spec(&with_port(&self.hub), self.ca()?.as_deref(), self.token.get()?)
    }

    /// The CA path, made absolute so later syncs from anywhere find it.
    fn ca(&self) -> Result<Option<PathBuf>> {
        self.hub_ca
            .as_ref()
            .map(|p| p.canonicalize().with_context(|| format!("{}", p.display())))
            .transpose()
    }
}

/// Connection settings for a hub address as stored in the config.
fn hub_spec(hub: &str, ca: Option<&Path>, token: String) -> Result<HubSpec> {
    let (addr, tls) = match hub.strip_prefix(rustfuse_hub::tls::SCHEME) {
        Some(addr) => (addr, Some(rustfuse_hub::tls::client_config(ca)?)),
        None => {
            if ca.is_some() {
                bail!("--hub-ca needs a tls:// hub address");
            }
            (hub, None)
        }
    };
    Ok(HubSpec {
        addr: addr.to_owned(),
        token,
        tls,
    })
}

/// Adds the default port to each address of a (comma-separated) list.
fn with_port(addr: &str) -> String {
    if let Some(rest) = addr.strip_prefix(rustfuse_hub::tls::SCHEME) {
        return format!("{}{}", rustfuse_hub::tls::SCHEME, with_port(rest));
    }
    if addr.contains(',') {
        return addr
            .split(',')
            .map(|a| with_port(a.trim().trim_start_matches(rustfuse_hub::tls::SCHEME)))
            .collect::<Vec<_>>()
            .join(",");
    }
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
        let Some(bucket) = &self.bucket else {
            bail!("set the bucket with --bucket or RUSTFUSE_BUCKET");
        };
        connect(
            bucket,
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
        let via_hub = self.bucket.is_none();
        Ok(Config {
            volume,
            bucket: self.bucket.clone().unwrap_or_default(),
            endpoint: if via_hub { None } else { self.endpoint.clone() },
            region: self.region.clone(),
            prefix: if via_hub {
                String::new()
            } else {
                self.prefix.clone()
            },
            path_style: !self.virtual_hosted,
            hub: Some(with_port(&hub.hub)),
            hub_ca: hub.ca()?,
            via_hub,
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

/// The volume's objects: straight from the bucket, or through the hub.
async fn volume_store(config: &Config, creds: &Credentials, hub: &HubSpec) -> Result<Arc<dyn ObjectStore>> {
    if config.via_hub {
        let s = rustfuse_hub::HubStore::connect(&hub.addr, hub.tls.clone(), &config.volume, &hub.token)
            .await
            .with_context(|| format!("open storage through the hub at {}", hub.addr))?;
        return Ok(Arc::new(s));
    }
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

/// Opens a synced directory and the hub it syncs with.
async fn open_replica(dir: &Path, opts: &SyncOpts) -> Result<(Replica, HubSpec)> {
    let config = Replica::config_of(dir)?;
    let Some(addr) = config.hub.clone() else {
        bail!("{} has no hub configured", dir.display());
    };
    let spec = hub_spec(&addr, config.hub_ca.as_deref(), opts.token.get()?)?;
    let r = Replica::open(dir, volume_store(&config, &opts.creds, &spec).await?)?;
    Ok((r, spec))
}

/// Starts syncing an opened directory (and its metrics endpoint); the
/// returned future syncs until an error stops it.
async fn start_sync(
    dir: &Path,
    mut r: Replica,
    spec: HubSpec,
    opts: &SyncOpts,
    calls: Option<tokio::sync::mpsc::Receiver<rustfuse_sync::provider::Call>>,
) -> Result<impl std::future::Future<Output = Result<()>> + use<>> {
    if let Some(addr) = &opts.metrics {
        let m = r.metrics();
        serve_metrics(addr, move || m.render()).await?;
    }
    println!("Syncing {} with {} (Ctrl-C to stop)...", dir.display(), spec.addr);
    let (debounce, full_scan) = (
        Duration::from_millis(opts.debounce_ms),
        Duration::from_secs(opts.full_scan_secs),
    );
    Ok(async move { Ok(r.run_serving(&spec, debounce, full_scan, calls).await?) })
}

/// What a mount serves for `dir`, and the sync to run beside it (none with
/// `no_sync`). A lazy directory fetches content as the mount opens files.
async fn prepare_serving(
    dir: &Path,
    no_sync: bool,
    cache_size: Option<u64>,
    opts: &SyncOpts,
) -> Result<(
    Arc<rustfuse_mount::Backing>,
    Option<impl std::future::Future<Output = Result<()>> + use<>>,
)> {
    if !dir.join(rustfuse_sync::STATE_DIR).is_dir() {
        bail!(
            "{} is not a rustfuse directory (run `rustfuse init` or `rustfuse attach`)",
            dir.display()
        );
    }
    let mut backing = rustfuse_mount::Backing::new(dir, &[rustfuse_sync::STATE_DIR])?;
    if no_sync {
        if rustfuse_sync::lazy::is_lazy(dir) || cache_size.is_some() {
            bail!(
                "{} fetches files on first use; serve it with its sync (drop --no-sync)",
                dir.display()
            );
        }
        return Ok((Arc::new(backing), None));
    }
    let (r, spec) = open_replica(dir, opts).await?;
    let mut hydrator = None;
    if rustfuse_sync::lazy::is_lazy(dir) {
        let h = Arc::new(rustfuse_sync::lazy::Hydrator::new(dir, r.store().clone())?);
        hydrator = Some(h.clone());
        let rt = tokio::runtime::Handle::current();
        backing = backing.with_fetch(Arc::new(move |path: &Path| {
            let fetch = || rt.block_on(h.hydrate(path));
            // FUSE calls from its own threads, NFS from the runtime's.
            let res = if tokio::runtime::Handle::try_current().is_ok() {
                tokio::task::block_in_place(fetch)
            } else {
                fetch()
            };
            res.map(drop).map_err(|e| {
                tracing::warn!("cannot fetch {}: {e}", path.display());
                std::io::Error::other(e)
            })
        }));
    }
    let backing = Arc::new(backing);
    match (cache_size, hydrator) {
        (Some(budget), Some(h)) => {
            tokio::spawn(keep_within(budget, h, backing.clone()));
        }
        (Some(_), None) => bail!(
            "--cache-size needs a directory attached with --lazy; {} holds every file",
            dir.display()
        ),
        _ => {}
    }
    let sync = start_sync(dir, r, spec, opts, None).await?;
    Ok((backing, Some(sync)))
}

/// Keeps fetched content in a lazy directory within `budget` bytes.
async fn keep_within(
    budget: u64,
    h: Arc<rustfuse_sync::lazy::Hydrator>,
    backing: Arc<rustfuse_mount::Backing>,
) {
    let mut every = tokio::time::interval(Duration::from_secs(30));
    loop {
        every.tick().await;
        let (h, b) = (h.clone(), backing.clone());
        match tokio::task::spawn_blocking(move || h.evict(budget, |ino| b.in_use(ino))).await {
            Ok(Ok(e)) if e.files > 0 => tracing::info!(
                files = e.files,
                mib = e.bytes >> 20,
                held_mib = (e.held - e.bytes) >> 20,
                "dropped fetched content to stay within the cache size"
            ),
            Ok(Err(e)) => tracing::warn!("cannot drop fetched content: {e}"),
            _ => {}
        }
    }
}

/// Parses a size like `500M`, `10G` or `1T` (powers of 1024), or bytes.
fn parse_size(s: &str) -> Result<u64, String> {
    let s = s.trim();
    let (num, unit) = match s.find(|c: char| !c.is_ascii_digit() && c != '.') {
        Some(i) => s.split_at(i),
        None => (s, ""),
    };
    let shift = match unit.trim().to_ascii_uppercase().trim_end_matches(['B', 'I']) {
        "" => 0,
        "K" => 10,
        "M" => 20,
        "G" => 30,
        "T" => 40,
        _ => return Err(format!("unknown unit in {s:?}; use K, M, G or T")),
    };
    let n: f64 = num.parse().map_err(|_| format!("not a size: {s:?}"))?;
    Ok((n * (1u64 << shift) as f64) as u64)
}

/// Runs `sync` (if any) until `serve` ends or Ctrl-C.
async fn serve_until(
    sync: Option<impl std::future::Future<Output = Result<()>>>,
    serve: impl std::future::Future<Output = Result<()>>,
) -> Result<()> {
    let sync = async {
        match sync {
            Some(s) => s.await,
            None => std::future::pending().await,
        }
    };
    tokio::select! {
        res = sync => res,
        res = serve => res,
        _ = tokio::signal::ctrl_c() => Ok(()),
    }
}

async fn mount(
    dir: &Path,
    mountpoint: &Path,
    fuse: &rustfuse_mount::FuseOpts,
    no_sync: bool,
    cache_size: Option<u64>,
    opts: &SyncOpts,
) -> Result<()> {
    if !rustfuse_mount::FUSE {
        bail!(
            "this build of rustfuse has no FUSE support. Use `rustfuse nfs` and the system's NFS client, \
             or install macFUSE (https://macfuse.github.io) and build with `--features macfuse`"
        );
    }
    let (backing, sync) = prepare_serving(dir, no_sync, cache_size, opts).await?;
    let mut fuse = fuse.clone();
    if fuse.volname.is_none() {
        fuse.volname = dir.file_name().map(|n| n.to_string_lossy().into_owned());
    }
    let m = rustfuse_mount::mount_fuse(backing, mountpoint, &fuse)
        .with_context(|| format!("mount at {}", mountpoint.display()))?;
    println!(
        "Mounted {} at {} (Ctrl-C to unmount).",
        dir.display(),
        mountpoint.display()
    );
    let res = serve_until(sync, std::future::pending()).await;
    m.unmount()?;
    res
}

async fn serve_nfs(
    dir: &Path,
    listen: &str,
    no_sync: bool,
    cache_size: Option<u64>,
    opts: &SyncOpts,
) -> Result<()> {
    let (backing, sync) = prepare_serving(dir, no_sync, cache_size, opts).await?;
    let server = rustfuse_mount::NfsServer::bind(backing, listen)
        .await
        .with_context(|| format!("listen on {listen}"))?;
    let (ip, port) = (server.ip(), server.port());
    if !ip.is_loopback() {
        tracing::warn!(
            "NFS has no authentication: anyone who can reach {ip}:{port} can read and write the volume"
        );
    }
    println!(
        "Serving {} over NFS at {ip}:{port}. Mount it with:",
        dir.display()
    );
    let host = if ip.is_unspecified() {
        "<this-host>".to_owned()
    } else {
        ip.to_string()
    };
    println!("  Linux: mount -t nfs -o vers=3,tcp,port={port},mountport={port},nolock {host}:/ <mountpoint>");
    println!(
        "  macOS: mount -t nfs -o vers=3,tcp,port={port},mountport={port},locallocks {host}:/ <mountpoint>"
    );
    serve_until(sync, async { Ok(server.serve().await?) }).await
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

/// Starts serving `/metrics` in the background.
async fn serve_metrics<F>(addr: &str, render: F) -> Result<()>
where
    F: Fn() -> String + Send + Sync + 'static,
{
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("listen on {addr} for metrics"))?;
    println!("Metrics at http://{}/metrics.", listener.local_addr()?);
    tokio::spawn(async move {
        if let Err(e) = rustfuse_hub::metrics::serve(listener, render).await {
            tracing::warn!("metrics endpoint stopped: {e}");
        }
    });
    Ok(())
}

async fn run(cli: Cli) -> Result<()> {
    let mut filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    // The NFS server warns about every request it does not implement
    // (COMMIT after FILE_SYNC writes, LINK); the client copes with both.
    if !std::env::var("RUST_LOG").is_ok_and(|v| v.contains("nfsserve")) {
        filter = filter.add_directive("nfsserve=error".parse().expect("valid directive"));
    }
    tracing_subscriber::fmt().with_env_filter(filter).init();
    match cli.command {
        Command::Hub {
            listen,
            store,
            creds,
            token,
            tls_cert,
            tls_key,
            metrics,
        } => {
            let tls = match (&tls_cert, &tls_key) {
                (Some(cert), Some(key)) => Some(rustfuse_hub::tls::server_config(cert, key)?),
                _ => None,
            };
            let root: Arc<dyn ObjectStore> = Arc::new(store.connect(&creds, &store.prefix).await?);
            let listener = tokio::net::TcpListener::bind(&listen)
                .await
                .with_context(|| format!("listen on {listen}"))?;
            println!(
                "Hub listening on {}{}.",
                listener.local_addr()?,
                if tls.is_some() { " with TLS" } else { "" }
            );
            // Without a shared token the hub accepts only per-machine keys.
            if token.hub_token.as_deref().is_none_or(str::is_empty) {
                println!("No shared token set: accepting only per-machine keys (rustfuse key create).");
            }
            let hub = rustfuse_hub::Hub::new(root, token.hub_token.clone().unwrap_or_default());
            if let Some(addr) = metrics {
                let m = hub.metrics().clone();
                serve_metrics(&addr, move || m.render()).await?;
            }
            tokio::select! {
                res = hub.serve_with(listener, tls) => res?,
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
            let s = volume_store(&config, &creds, &spec).await?;
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
            lazy,
            hub,
            store,
            creds,
        } => {
            let spec = hub.spec()?;
            let config = store.config(volume, &hub)?;
            let s = volume_store(&config, &creds, &spec).await?;
            let mut r = Replica::attach_with(&dir, config, s, lazy).await?;
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
            provider_socket,
            opts,
        } => {
            if once {
                let (mut r, spec) = open_replica(&dir, &opts).await?;
                let s = r.sync_once(&spec, SYNC_TIMEOUT).await?;
                println!(
                    "Sent {} changesets, applied {}; now at changeset {}.",
                    s.pushed,
                    s.received,
                    r.applied()
                );
            } else {
                let (r, spec) = open_replica(&dir, &opts).await?;
                let mut calls = None;
                let mut provider = None;
                if let Some(sock) = provider_socket {
                    let hydrator = if rustfuse_sync::lazy::is_lazy(&dir) {
                        Some(Arc::new(rustfuse_sync::lazy::Hydrator::new(
                            &dir,
                            r.store().clone(),
                        )?))
                    } else {
                        None
                    };
                    let (tx, rx) = tokio::sync::mpsc::channel(64);
                    calls = Some(rx);
                    println!("Answering the File Provider on {}.", sock.display());
                    provider = Some(rustfuse_sync::provider::serve(sock, tx, hydrator));
                }
                let provider = async move {
                    match provider {
                        Some(p) => p.await.context("File Provider socket"),
                        None => std::future::pending().await,
                    }
                };
                let sync = start_sync(&dir, r, spec, &opts, calls).await?;
                tokio::select! {
                    res = sync => res?,
                    res = provider => res?,
                    _ = tokio::signal::ctrl_c() => {}
                }
            }
        }
        Command::Mount {
            dir,
            mountpoint,
            allow_other,
            fskit,
            no_sync,
            cache_size,
            opts,
        } => {
            let fuse = rustfuse_mount::FuseOpts {
                allow_other,
                fskit,
                volname: None,
            };
            mount(&dir, &mountpoint, &fuse, no_sync, cache_size, &opts).await?
        }
        Command::Nfs {
            dir,
            listen,
            no_sync,
            cache_size,
            opts,
        } => serve_nfs(&dir, &listen, no_sync, cache_size, &opts).await?,
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
        Command::Key { action, store, creds } => {
            let root = store.connect(&creds, &store.prefix).await?;
            match action {
                KeyAction::Create { name, volumes } => {
                    let (k, token) = rustfuse_hub::keys::create(&root, &name, volumes).await?;
                    println!("Created key {} for {}.", k.id, k.name);
                    println!("Token (shown only once; set it as RUSTFUSE_HUB_TOKEN on that machine):");
                    println!("{token}");
                }
                KeyAction::List => {
                    let keys = rustfuse_hub::keys::list(&root).await?;
                    if keys.is_empty() {
                        println!("No keys.");
                    }
                    for k in keys {
                        let vols = if k.volumes.is_empty() {
                            "all volumes".to_owned()
                        } else {
                            k.volumes.join(", ")
                        };
                        let created = std::time::UNIX_EPOCH + Duration::from_millis(k.created);
                        let days = created.elapsed().map_or(0, |d| d.as_secs() / 86_400);
                        println!("{}  {}  ({vols}; created {days} days ago)", k.id, k.name);
                    }
                }
                KeyAction::Revoke { id } => {
                    if rustfuse_hub::keys::revoke(&root, &id).await? {
                        println!("Revoked key {id}; hubs refuse it within 30 seconds.");
                    } else {
                        bail!("no key {id}");
                    }
                }
            }
        }
        Command::Gc {
            volume,
            retention_days,
            delay_minutes,
            dry_run,
            store,
            creds,
        } => {
            if !rustfuse_hub::valid_volume_name(&volume) {
                bail!("invalid volume name");
            }
            let base = store.prefix.trim_matches('/');
            let prefix = if base.is_empty() {
                format!("volumes/{volume}")
            } else {
                format!("{base}/volumes/{volume}")
            };
            let s: Arc<dyn ObjectStore> = Arc::new(store.connect(&creds, &prefix).await?);
            let opts = rustfuse_sync::gc::GcOptions {
                retention: Duration::from_secs(retention_days * 24 * 3600),
                delay: Duration::from_secs(delay_minutes * 60),
                dry_run,
                ..Default::default()
            };
            let st = rustfuse_sync::gc::collect(s, &opts).await?;
            let (marked, compacted) = if dry_run {
                ("would mark", "would compact")
            } else {
                ("marked", "compacted")
            };
            println!(
                "{} packs ({}), {} in use.",
                st.packs,
                mib(st.pack_bytes),
                st.used_packs
            );
            println!(
                "{marked} {} unused and {compacted} {} sparse ({} -> {}) for deletion in a later run.",
                st.marked,
                st.compacted,
                mib(st.compacted_from),
                mib(st.compacted_to)
            );
            println!(
                "{} {} marked earlier ({}); {} still waiting, {} in use again; {} old snapshots, {} forwarding tables.",
                if dry_run { "Would delete" } else { "Deleted" },
                st.deleted,
                mib(st.deleted_bytes),
                st.waiting,
                st.revived,
                st.snapshots_deleted,
                st.forwards_deleted,
            );
        }
        Command::Bench(args) => bench::bench(args).await?,
        Command::GitFsmonitor { repo, off } => git_fsmonitor(&repo, off)?,
        Command::Fsmonitor { .. } => unreachable!("handled before the runtime starts"),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn cli_is_consistent() {
        use clap::CommandFactory;
        super::Cli::command().debug_assert();
    }

    #[test]
    fn sizes() {
        use super::parse_size;
        assert_eq!(parse_size("4096"), Ok(4096));
        assert_eq!(parse_size("500M"), Ok(500 << 20));
        assert_eq!(parse_size("1.5g"), Ok(3 << 29));
        assert_eq!(parse_size("2TiB"), Ok(2 << 40));
        assert!(parse_size("5X").is_err() && parse_size("G").is_err());
    }
}
