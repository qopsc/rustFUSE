//! End-to-end: several machines share a volume through an in-process hub
//! and stay byte-for-byte identical through edits, moves, deletes,
//! conflicts and offline work.
//!
//! Runs against the in-memory store, or against a real S3 endpoint when
//! `RUSTFUSE_TEST_S3_ENDPOINT` is set.

use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use bytes::Bytes;
use rustfuse_store::{ListPage, MemoryStore, ObjectMeta, ObjectStore, PrefixStore, S3Config, S3Store};
use rustfuse_sync::{Config, HubSpec, Replica, STATE_DIR};
use rustfuse_types::ReplicaId;

/// Root store holding `volumes/`.
async fn root_store(run_prefix: &str) -> Arc<dyn ObjectStore> {
    match std::env::var("RUSTFUSE_TEST_S3_ENDPOINT") {
        Ok(endpoint) => {
            let mut cfg =
                S3Config::new(std::env::var("RUSTFUSE_TEST_S3_BUCKET").unwrap_or("rustfuse-test".into()));
            cfg.endpoint = Some(endpoint);
            cfg.prefix = run_prefix.to_owned();
            cfg.access_key =
                Some(std::env::var("RUSTFUSE_TEST_S3_ACCESS_KEY").unwrap_or("rustfsadmin".into()));
            cfg.secret_key =
                Some(std::env::var("RUSTFUSE_TEST_S3_SECRET_KEY").unwrap_or("rustfsadmin".into()));
            let s = S3Store::connect(cfg).await.unwrap();
            s.create_bucket_if_missing().await.unwrap();
            Arc::new(s)
        }
        Err(_) => Arc::new(MemoryStore::new()),
    }
}

fn config(volume: &str, replica: u64) -> Config {
    Config {
        volume: volume.to_owned(),
        bucket: "unused".into(),
        endpoint: None,
        region: "us-east-1".into(),
        prefix: String::new(),
        path_style: true,
        hub: None,
        hub_ca: None,
        via_hub: false,
        replica: ReplicaId(replica),
        seed: [replica as u8; 16],
    }
}

/// A store that can be switched off to simulate losing the network.
struct Flaky {
    inner: Arc<dyn ObjectStore>,
    offline: Arc<AtomicBool>,
}

impl Flaky {
    fn check(&self) -> rustfuse_store::Result<()> {
        if self.offline.load(Ordering::SeqCst) {
            return Err(rustfuse_store::Error::Backend(Box::new(std::io::Error::other(
                "offline",
            ))));
        }
        Ok(())
    }
}

#[async_trait]
impl ObjectStore for Flaky {
    async fn get(&self, key: &str) -> rustfuse_store::Result<Bytes> {
        self.check()?;
        self.inner.get(key).await
    }
    async fn get_range(&self, key: &str, range: std::ops::Range<u64>) -> rustfuse_store::Result<Bytes> {
        self.check()?;
        self.inner.get_range(key, range).await
    }
    async fn put(&self, key: &str, data: Bytes) -> rustfuse_store::Result<()> {
        self.check()?;
        self.inner.put(key, data).await
    }
    async fn head(&self, key: &str) -> rustfuse_store::Result<Option<ObjectMeta>> {
        self.check()?;
        self.inner.head(key).await
    }
    async fn delete(&self, key: &str) -> rustfuse_store::Result<()> {
        self.check()?;
        self.inner.delete(key).await
    }
    async fn list(
        &self,
        prefix: &str,
        after: Option<&str>,
        limit: usize,
    ) -> rustfuse_store::Result<ListPage> {
        self.check()?;
        self.inner.list(prefix, after, limit).await
    }
}

fn pseudo_random(n: usize, seed: u64) -> Vec<u8> {
    let mut x = seed | 1;
    (0..n)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x as u8
        })
        .collect()
}

/// Everything a follower must reproduce: kind, bytes, permission bits,
/// symlink targets and file mtimes. Skips the state dir and excluded paths.
fn describe(root: &Path) -> Vec<String> {
    fn walk(root: &Path, rel: &Path, out: &mut Vec<String>) {
        let mut entries: Vec<_> = fs::read_dir(root.join(rel))
            .unwrap()
            .map(|e| e.unwrap())
            .collect();
        entries.sort_by_key(|e| e.file_name());
        for e in entries {
            let name = e.file_name();
            let r = rel.join(&name);
            if r.starts_with(STATE_DIR)
                || r == Path::new("target")
                || name.to_string_lossy().ends_with(".lock")
                // Each machine's own pointer to its copy of a worktree.
                || (name == "gitdir" && r.parent().and_then(Path::parent).is_some_and(|p| p.ends_with(".git/worktrees")))
            {
                continue;
            }
            let m = fs::symlink_metadata(root.join(&r)).unwrap();
            let mode = m.permissions().mode() & 0o777;
            if m.file_type().is_symlink() {
                out.push(format!(
                    "L {} -> {}",
                    r.display(),
                    fs::read_link(root.join(&r)).unwrap().display()
                ));
            } else if m.is_dir() {
                out.push(format!("D {} {mode:o}", r.display()));
                walk(root, &r, out);
            } else {
                let data = fs::read(root.join(&r)).unwrap();
                let mtime = m
                    .modified()
                    .unwrap()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_nanos();
                out.push(format!(
                    "F {} {mode:o} {} {} {mtime}",
                    r.display(),
                    data.len(),
                    blake3_hex(&data)
                ));
            }
        }
    }
    let mut out = Vec::new();
    walk(root, Path::new(""), &mut out);
    out
}

fn blake3_hex(data: &[u8]) -> String {
    rustfuse_chunk::hash(data)
        .0
        .iter()
        .take(8)
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn assert_same(a: &Path, b: &Path) {
    let (da, db) = (describe(a), describe(b));
    if da != db {
        let only_a: Vec<_> = da.iter().filter(|l| !db.contains(l)).collect();
        let only_b: Vec<_> = db.iter().filter(|l| !da.contains(l)).collect();
        panic!(
            "trees differ\nonly in {}: {only_a:#?}\nonly in {}: {only_b:#?}",
            a.display(),
            b.display()
        );
    }
}

const TIMEOUT: Duration = Duration::from_secs(30);

/// Machines sharing one volume through one hub.
struct Cluster {
    _tmp: tempfile::TempDir,
    dirs: Vec<PathBuf>,
    replicas: Vec<Replica>,
    offline: Vec<Arc<AtomicBool>>,
    hub: HubSpec,
    /// The volume's objects, straight from the bucket.
    volume: Arc<dyn ObjectStore>,
    hub_metrics: Arc<rustfuse_hub::HubMetrics>,
}

impl Cluster {
    /// Machine 0 creates the volume from `populate`; the others attach.
    async fn new(n: usize, populate: impl FnOnce(&Path)) -> Cluster {
        if std::env::var_os("RUSTFUSE_TEST_LOG").is_some() {
            let _ = tracing_subscriber::fmt()
                .with_env_filter(tracing_subscriber::EnvFilter::new("rustfuse_sync=debug"))
                .with_test_writer()
                .try_init();
        }
        let tmp = tempfile::tempdir().unwrap();
        let nanos = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
        let root = root_store(&format!("rustfuse-tests/{nanos}")).await;
        let volume = "vol";
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let hub = HubSpec {
            addr: listener.local_addr().unwrap().to_string(),
            token: "t0ken".into(),
            tls: None,
        };
        let server = rustfuse_hub::Hub::new(root.clone(), hub.token.clone());
        let hub_metrics = server.metrics().clone();
        tokio::spawn(server.serve(listener));

        let mut c = Cluster {
            _tmp: tmp,
            dirs: Vec::new(),
            replicas: Vec::new(),
            offline: Vec::new(),
            hub,
            volume: Arc::new(PrefixStore::new(root.clone(), &format!("volumes/{volume}"))),
            hub_metrics,
        };
        let mut populate = Some(populate);
        for i in 0..n {
            let dir = c._tmp.path().join(format!("m{i}"));
            let offline = Arc::new(AtomicBool::new(false));
            // Odd machines reach the bucket through the hub, like remote
            // ones; every test runs both ways.
            let via_hub = i % 2 == 1;
            let inner: Arc<dyn ObjectStore> = if via_hub {
                Arc::new(
                    rustfuse_hub::HubStore::connect(&c.hub.addr, None, volume, &c.hub.token)
                        .await
                        .unwrap(),
                )
            } else {
                Arc::new(PrefixStore::new(root.clone(), &format!("volumes/{volume}")))
            };
            let store: Arc<dyn ObjectStore> = Arc::new(Flaky {
                inner,
                offline: offline.clone(),
            });
            let mut cfg = config(volume, i as u64 + 1);
            cfg.via_hub = via_hub;
            cfg.hub = Some(c.hub.addr.clone());
            let r = if i == 0 {
                fs::create_dir(&dir).unwrap();
                populate.take().unwrap()(&dir);
                let (mut r, _) = Replica::init(&dir, cfg, store).await.unwrap();
                r.sync_once(&c.hub, TIMEOUT).await.unwrap();
                r
            } else {
                Replica::attach(&dir, cfg, store).await.unwrap()
            };
            c.dirs.push(dir);
            c.replicas.push(r);
            c.offline.push(offline);
        }
        for i in 1..n {
            c.assert_same(0, i);
        }
        c
    }

    fn dir(&self, i: usize) -> &Path {
        &self.dirs[i]
    }

    /// One sync round on machine `i`; returns (sent, received).
    async fn sync(&mut self, i: usize) -> (usize, usize) {
        let s = self.replicas[i].sync_once(&self.hub, TIMEOUT).await.unwrap();
        (s.pushed, s.received)
    }

    /// Syncs everyone until nothing moves, then checks all trees match.
    async fn settle(&mut self) {
        for round in 0.. {
            let mut moved = false;
            for i in 0..self.replicas.len() {
                let (sent, received) = self.sync(i).await;
                moved |= sent > 0 || received > 0;
            }
            if !moved {
                break;
            }
            assert!(round < 10, "machines did not converge");
        }
        for i in 1..self.replicas.len() {
            self.assert_same(0, i);
        }
    }

    fn assert_same(&self, a: usize, b: usize) {
        assert_same(&self.dirs[a], &self.dirs[b]);
    }

    fn set_offline(&self, i: usize, off: bool) {
        self.offline[i].store(off, Ordering::SeqCst);
    }
}

fn names(dir: &Path) -> Vec<String> {
    let mut v: Vec<String> = fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| n != STATE_DIR)
        .collect();
    v.sort();
    v
}

fn sample_tree(root: &Path) {
    fs::create_dir_all(root.join("src/nested/deep")).unwrap();
    fs::create_dir_all(root.join("empty")).unwrap();
    fs::create_dir_all(root.join("target/debug")).unwrap();
    fs::write(root.join("README.md"), "hello\n").unwrap();
    fs::write(root.join("src/main.rs"), "fn main() {}\n").unwrap();
    fs::write(root.join("src/nested/deep/big.bin"), pseudo_random(3 << 20, 5)).unwrap();
    fs::write(root.join("src/nested/medium.txt"), "line\n".repeat(5000)).unwrap();
    fs::write(root.join("run.sh"), "#!/bin/sh\necho hi\n").unwrap();
    fs::set_permissions(root.join("run.sh"), fs::Permissions::from_mode(0o755)).unwrap();
    fs::write(root.join("target/debug/out"), "build output").unwrap();
    fs::write(root.join(".gitignore"), "/target\n").unwrap();
    // A source directory that merely shares a build dir's name must sync.
    fs::create_dir_all(root.join("tests/target")).unwrap();
    fs::write(root.join("tests/target/mod.rs"), "// fixture").unwrap();
    symlink("src/main.rs", root.join("link")).unwrap();
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .output()
        .expect("git not installed");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}

#[tokio::test]
async fn initial_sync_and_exclusions() {
    let c = Cluster::new(2, sample_tree).await;
    let dst = c.dir(1);
    assert!(!dst.join("target").exists(), "gitignored build dir was synced");
    assert!(
        dst.join("tests/target/mod.rs").exists(),
        "tracked dir named target was skipped"
    );
    assert!(dst.join("empty").is_dir());
    assert_eq!(
        fs::read_link(dst.join("link")).unwrap(),
        PathBuf::from("src/main.rs")
    );
}

#[tokio::test]
async fn read_only_directory_keeps_its_contents() {
    let c = Cluster::new(2, |root: &Path| {
        fs::create_dir_all(root.join("ro/sub")).unwrap();
        fs::write(root.join("ro/small"), "tiny").unwrap();
        fs::write(root.join("ro/sub/big"), pseudo_random(200_000, 7)).unwrap();
        fs::set_permissions(root.join("ro/sub"), fs::Permissions::from_mode(0o555)).unwrap();
        fs::set_permissions(root.join("ro"), fs::Permissions::from_mode(0o555)).unwrap();
    })
    .await;
    assert_same(c.dir(0), c.dir(1));
    for i in 0..2 {
        for d in ["ro", "ro/sub"] {
            fs::set_permissions(c.dir(i).join(d), fs::Permissions::from_mode(0o755)).unwrap();
        }
    }
}

#[tokio::test]
async fn edits_moves_and_deletes() {
    let mut c = Cluster::new(2, sample_tree).await;
    assert_eq!(c.sync(0).await, (0, 0), "no-op sync sent something");
    let src = c.dir(0).to_path_buf();

    // Edit in place, append to a large file, chmod.
    fs::write(src.join("README.md"), "hello world\n").unwrap();
    let mut big = fs::read(src.join("src/nested/deep/big.bin")).unwrap();
    big.extend_from_slice(b"appended");
    fs::write(src.join("src/nested/deep/big.bin"), &big).unwrap();
    fs::set_permissions(src.join("src/main.rs"), fs::Permissions::from_mode(0o600)).unwrap();
    c.settle().await;

    // Rename a file, move a directory, delete a file and a directory.
    fs::rename(src.join("README.md"), src.join("README.txt")).unwrap();
    fs::rename(src.join("src/nested"), src.join("moved")).unwrap();
    fs::remove_file(src.join("run.sh")).unwrap();
    fs::remove_dir(src.join("empty")).unwrap();
    c.settle().await;

    // Swap two names through a temp name, and move a file into a new dir.
    fs::write(src.join("a"), "A").unwrap();
    fs::write(src.join("b"), "B").unwrap();
    c.settle().await;
    fs::rename(src.join("a"), src.join("tmp")).unwrap();
    fs::rename(src.join("b"), src.join("a")).unwrap();
    fs::rename(src.join("tmp"), src.join("b")).unwrap();
    fs::create_dir(src.join("fresh")).unwrap();
    fs::rename(src.join("README.txt"), src.join("fresh/README.txt")).unwrap();
    c.settle().await;

    // Replace a directory with a file of the same name, and a file with a symlink.
    fs::remove_dir_all(src.join("moved")).unwrap();
    fs::write(src.join("moved"), "now a file").unwrap();
    fs::remove_file(src.join("a")).unwrap();
    symlink("b", src.join("a")).unwrap();
    c.settle().await;

    // And the other way round: the second machine edits too.
    fs::write(c.dir(1).join("from-b.txt"), "b").unwrap();
    fs::remove_file(c.dir(1).join("moved")).unwrap();
    c.settle().await;
    assert!(src.join("from-b.txt").exists());
}

#[tokio::test]
async fn moves_do_not_reupload() {
    let mut c = Cluster::new(2, sample_tree).await;
    fs::rename(c.dir(0).join("src"), c.dir(0).join("source")).unwrap();
    let stats = c.replicas[0].capture(None).await.unwrap().unwrap();
    assert_eq!(stats.files_read, 0, "moved files were re-read");
    assert_eq!(stats.bytes_uploaded, 0);
    c.settle().await;
}

#[tokio::test]
async fn partial_scan_from_watch_paths() {
    let mut c = Cluster::new(2, sample_tree).await;
    fs::write(c.dir(0).join("src/nested/new.txt"), "new").unwrap();
    fs::remove_file(c.dir(0).join("src/main.rs")).unwrap();
    let paths = vec![PathBuf::from("src/nested/new.txt"), PathBuf::from("src/main.rs")];
    let stats = c.replicas[0].capture(Some(paths)).await.unwrap().unwrap();
    assert_eq!(stats.ops, 2);
    c.settle().await;
}

#[tokio::test]
async fn reopen_keeps_state() {
    let mut c = Cluster::new(2, sample_tree).await;
    fs::write(c.dir(0).join("x"), "x").unwrap();
    c.replicas[0].capture(None).await.unwrap().unwrap();
    // Restart before sending: the outbox survives.
    let store = c.replicas[0].store().clone();
    drop(c.replicas.remove(0));
    c.replicas
        .insert(0, Replica::open(c.dir(0), store.clone()).unwrap());
    assert_eq!(c.replicas[0].outbox_len(), 1);
    c.settle().await;
    assert_eq!(fs::read(c.dir(1).join("x")).unwrap(), b"x");
    assert_eq!(c.sync(1).await, (0, 0));
    // New packs written after a restart must not replace earlier ones: a
    // fresh machine still gets every file intact.
    fs::write(c.dir(0).join("after-restart.bin"), pseudo_random(2 << 20, 11)).unwrap();
    c.settle().await;
    let fresh = c._tmp.path().join("fresh");
    Replica::attach(&fresh, config("vol", 9), store).await.unwrap();
    assert_same(c.dir(0), &fresh);
}

#[tokio::test]
async fn concurrent_edits_to_different_files_merge() {
    let mut c = Cluster::new(2, sample_tree).await;
    fs::write(c.dir(0).join("README.md"), "from a\n").unwrap();
    fs::write(c.dir(1).join("src/main.rs"), "fn main() { /* b */ }\n").unwrap();
    fs::rename(c.dir(1).join("run.sh"), c.dir(1).join("src/run.sh")).unwrap();
    c.sync(0).await;
    c.sync(1).await;
    c.settle().await;
    assert_eq!(
        fs::read_to_string(c.dir(1).join("README.md")).unwrap(),
        "from a\n"
    );
    assert!(c.dir(0).join("src/run.sh").exists());
}

#[tokio::test]
async fn same_file_edited_on_two_machines_keeps_both() {
    let mut c = Cluster::new(2, sample_tree).await;
    fs::write(c.dir(0).join("README.md"), "version a\n").unwrap();
    fs::write(c.dir(1).join("README.md"), "version b\n").unwrap();
    // b captures later, so its version is newer and keeps the name.
    c.replicas[0].capture(None).await.unwrap();
    c.replicas[1].capture(None).await.unwrap();
    c.settle().await;
    let root = names(c.dir(0));
    let copy = root
        .iter()
        .find(|n| n.starts_with("README.conflict-"))
        .expect("no conflict copy");
    assert!(copy.ends_with(".md"));
    assert_eq!(
        fs::read_to_string(c.dir(0).join("README.md")).unwrap(),
        "version b\n"
    );
    assert_eq!(fs::read_to_string(c.dir(0).join(copy)).unwrap(), "version a\n");
}

#[tokio::test]
async fn edit_beats_delete() {
    let mut c = Cluster::new(2, sample_tree).await;
    fs::write(c.dir(0).join("src/main.rs"), "kept\n").unwrap();
    fs::remove_file(c.dir(1).join("src/main.rs")).unwrap();
    fs::remove_dir_all(c.dir(1).join("src/nested")).unwrap();
    fs::write(c.dir(0).join("src/nested/added.txt"), "new").unwrap();
    c.sync(1).await;
    c.settle().await;
    assert_eq!(
        fs::read_to_string(c.dir(1).join("src/main.rs")).unwrap(),
        "kept\n"
    );
    // The deleted directory survives with what was added to it, nothing else.
    assert_eq!(names(&c.dir(1).join("src/nested")), ["added.txt"]);
}

#[tokio::test]
async fn same_directory_created_on_two_machines_merges() {
    let mut c = Cluster::new(2, |_| {}).await;
    for (i, name) in [(0, "a.txt"), (1, "b.txt")] {
        fs::create_dir_all(c.dir(i).join("new/sub")).unwrap();
        fs::write(c.dir(i).join("new/sub").join(name), name).unwrap();
        fs::write(c.dir(i).join("new/same.txt"), "identical").unwrap();
    }
    c.replicas[0].capture(None).await.unwrap();
    c.replicas[1].capture(None).await.unwrap();
    c.settle().await;
    assert_eq!(names(&c.dir(0).join("new")), ["same.txt", "sub"]);
    assert_eq!(names(&c.dir(0).join("new/sub")), ["a.txt", "b.txt"]);
}

#[tokio::test]
async fn offline_work_syncs_later() {
    let mut c = Cluster::new(2, sample_tree).await;
    // Machine 1 loses the network, keeps working, including new large
    // content that cannot be uploaded yet.
    c.set_offline(1, true);
    let dead_hub = HubSpec {
        addr: "127.0.0.1:9".into(),
        token: "x".into(),
        tls: None,
    };
    fs::write(c.dir(1).join("offline.bin"), pseudo_random(1 << 20, 9)).unwrap();
    fs::write(c.dir(1).join("README.md"), "edited offline\n").unwrap();
    let stats = c.replicas[1].capture(None).await.unwrap().unwrap();
    assert!(stats.packs_spooled > 0, "content was not spooled");
    assert!(c.replicas[1].sync_once(&dead_hub, TIMEOUT).await.is_err());
    // Meanwhile machine 0 carries on.
    fs::write(c.dir(0).join("online.txt"), "online").unwrap();
    c.sync(0).await;
    // Back online: everything flows both ways.
    c.set_offline(1, false);
    c.settle().await;
    assert!(c.dir(0).join("offline.bin").exists());
    assert!(c.dir(1).join("online.txt").exists());
}

#[tokio::test]
async fn local_edit_is_not_overwritten_by_remote_change() {
    let mut c = Cluster::new(2, sample_tree).await;
    fs::write(c.dir(0).join("README.md"), "remote\n").unwrap();
    c.sync(0).await;
    // Machine 1 edits the same file without capturing; applying the remote
    // change must capture it first and keep both.
    fs::write(c.dir(1).join("README.md"), "local, unsaved\n").unwrap();
    c.settle().await;
    let all = names(c.dir(1));
    let texts: Vec<String> = all
        .iter()
        .filter(|n| n.starts_with("README"))
        .map(|n| fs::read_to_string(c.dir(1).join(n)).unwrap())
        .collect();
    assert!(texts.contains(&"remote\n".to_string()), "{texts:?}");
    assert!(texts.contains(&"local, unsaved\n".to_string()), "{texts:?}");
}

#[tokio::test]
async fn random_concurrent_changes_converge() {
    // RUSTFUSE_FUZZ=<seeds> runs a longer search locally.
    let seeds: u64 = std::env::var("RUSTFUSE_FUZZ")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(3);
    let steps = if seeds > 3 { 150 } else { 60 };
    let only: Option<u64> = std::env::var("RUSTFUSE_FUZZ_SEED")
        .ok()
        .and_then(|v| v.parse().ok());
    for seed in (0..seeds)
        .map(|i| i * 7919 + 1)
        .filter(|s| only.is_none_or(|o| o == *s))
    {
        eprintln!("seed {seed}");
        let mut c = Cluster::new(3, |root| {
            fs::create_dir_all(root.join("d0")).unwrap();
            fs::write(root.join("d0/f0"), "start").unwrap();
        })
        .await;
        let mut rng = seed;
        let mut next = move |n: u64| {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            rng % n
        };
        for step in 0..steps {
            let m = next(3) as usize;
            let root = c.dir(m).to_path_buf();
            let dir = root.join(format!("d{}", next(3)));
            let file = dir.join(format!("f{}", next(4)));
            let action = next(9);
            if std::env::var("RUSTFUSE_FUZZ_TRACE").is_ok() {
                eprintln!(
                    "step {step}: m{m} action {action} {}",
                    file.strip_prefix(&root).unwrap().display()
                );
            }
            match action {
                0 | 1 => {
                    let _ = fs::create_dir_all(&dir);
                    let len = if next(4) == 0 { 10_000 } else { 20 };
                    if fs::symlink_metadata(&file).is_ok_and(|m| !m.is_file()) {
                        continue;
                    }
                    fs::write(&file, pseudo_random(len, step + seed)).unwrap();
                }
                2 => {
                    let _ = fs::remove_file(&file);
                }
                3 => {
                    let to = root.join(format!("d{}", next(3))).join(format!("f{}", next(4)));
                    if to.parent().unwrap().is_dir() {
                        let _ = fs::rename(&file, &to);
                    }
                }
                4 => {
                    let _ = fs::remove_dir(&dir);
                }
                5 => {
                    let _ = fs::create_dir_all(dir.join("sub"));
                }
                6 => {
                    // Rename a whole directory (into a fresh name only).
                    let to = root.join(format!("d{}", next(5)));
                    if !to.exists() {
                        let _ = fs::rename(&dir, &to);
                    }
                }
                7 => {
                    if fs::symlink_metadata(&file).is_ok_and(|m| m.is_file()) {
                        let mode = if next(2) == 0 { 0o755 } else { 0o644 };
                        fs::set_permissions(&file, fs::Permissions::from_mode(mode)).unwrap();
                    }
                }
                _ => {
                    if dir.is_dir() && fs::symlink_metadata(&file).is_err() {
                        symlink(format!("f{}", next(4)), &file).unwrap();
                    }
                }
            }
            let then = next(4);
            if std::env::var("RUSTFUSE_FUZZ_TRACE").is_ok() && then < 2 {
                eprintln!("  {} m{m}", if then == 0 { "capture" } else { "sync" });
            }
            match then {
                0 => {
                    c.replicas[m].capture(None).await.unwrap();
                }
                1 => {
                    c.sync(m).await;
                }
                _ => {}
            }
        }
        c.settle().await;
    }
}

#[tokio::test]
async fn git_repository_with_worktree() {
    let mut c = Cluster::new(2, |root| {
        git(root, &["init", "-q", "-b", "main"]);
        fs::write(root.join("a.txt"), "one\n").unwrap();
        git(root, &["add", "."]);
        git(root, &["commit", "-q", "-m", "first"]);
    })
    .await;
    let (src, dst) = (c.dir(0).to_path_buf(), c.dir(1).to_path_buf());
    assert_eq!(git(&dst, &["log", "--format=%s"]), "first\n");

    // Commits, branches, a worktree inside the volume and a gc (packs refs
    // and objects) all replicate into a repository git considers healthy.
    for i in 0..5 {
        fs::write(src.join("a.txt"), format!("change {i}\n")).unwrap();
        git(&src, &["commit", "-q", "-am", &format!("change {i}")]);
    }
    git(&src, &["branch", "feature"]);
    git(&src, &["worktree", "add", "-q", "wt", "feature"]);
    fs::write(src.join("wt/b.txt"), "on feature\n").unwrap();
    git(&src.join("wt"), &["add", "b.txt"]);
    git(&src.join("wt"), &["commit", "-q", "-m", "feature work"]);
    c.settle().await;
    git(&src, &["gc", "-q"]);
    c.settle().await;

    git(&dst, &["fsck", "--strict"]);
    assert_eq!(
        git(&dst, &["log", "--format=%s", "feature"]),
        git(&src, &["log", "--format=%s", "feature"])
    );
    assert_eq!(git(&dst.join("wt"), &["status", "--porcelain"]), "");
}

/// Files under `dir` (recursively) whose name marks a conflict copy.
fn conflict_copies(dir: &Path) -> Vec<String> {
    describe(dir)
        .into_iter()
        .filter(|l| l.contains(".conflict-"))
        .collect()
}

fn git_repo(root: &Path) {
    git(root, &["init", "-q", "-b", "main"]);
    fs::write(root.join("a.txt"), "one\n").unwrap();
    fs::write(root.join("shared.txt"), "base\n").unwrap();
    git(root, &["add", "."]);
    git(root, &["commit", "-q", "-m", "first"]);
}

#[tokio::test]
async fn git_commits_on_two_machines_offline() {
    let mut c = Cluster::new(2, git_repo).await;
    let (a, b) = (c.dir(0).to_path_buf(), c.dir(1).to_path_buf());
    // Both commit to main without syncing: a changes a.txt and shared.txt,
    // b adds b.txt and changes shared.txt too. Each starts a branch.
    fs::write(a.join("a.txt"), "from a\n").unwrap();
    fs::write(a.join("shared.txt"), "a's version\n").unwrap();
    git(&a, &["commit", "-q", "-am", "commit on a"]);
    git(&a, &["branch", "topic-a"]);
    fs::write(b.join("b.txt"), "from b\n").unwrap();
    fs::write(b.join("shared.txt"), "b's version\n").unwrap();
    git(&b, &["add", "."]);
    git(&b, &["commit", "-q", "-m", "commit on b"]);
    git(&b, &["branch", "topic-b"]);
    c.replicas[0].capture(None).await.unwrap();
    c.replicas[1].capture(None).await.unwrap();
    c.settle().await;

    let main = git(&a, &["rev-parse", "main"]);
    for dir in [&a, &b] {
        git(dir, &["fsck", "--strict"]);
        // main is one side; the other is a branch to merge, and nothing
        // else is left over: the worktree matches main exactly.
        let branches = git(dir, &["branch", "--format=%(refname:short)"]);
        let lines: Vec<&str> = branches.lines().collect();
        assert_eq!(lines.len(), 4, "{branches}");
        assert!(
            branches.contains("topic-a") && branches.contains("topic-b"),
            "{branches}"
        );
        let other = lines
            .iter()
            .find(|l| l.starts_with("main.conflict-"))
            .expect("conflict branch");
        let logs = git(dir, &["log", "--format=%s", "main", other]);
        assert!(
            logs.contains("commit on a") && logs.contains("commit on b"),
            "{logs}"
        );
        assert_eq!(git(dir, &["status", "--porcelain"]), "", "worktree not clean");
        assert_eq!(git(dir, &["rev-parse", "main"]), main);
        assert_eq!(
            conflict_copies(dir),
            Vec::<String>::new()
                .into_iter()
                .chain(
                    describe(dir)
                        .into_iter()
                        .filter(|l| l.contains("refs/heads/main.conflict-"))
                )
                .collect::<Vec<_>>(),
            "leftover conflict copies"
        );
        // Both machines' history stays in the reflog.
        let reflog = fs::read_to_string(dir.join(".git/logs/HEAD")).unwrap();
        assert!(
            reflog.contains("commit on a") && reflog.contains("commit on b"),
            "{reflog}"
        );
        // Settings that keep the synced index valid on every machine.
        assert_eq!(git(dir, &["config", "core.checkStat"]), "minimal\n");
    }
    // Merging the conflict branch retires it, and leaves the worktree as
    // the user left it, even an edit back to the other side's version.
    let other = git(
        &a,
        &["branch", "--list", "main.conflict-*", "--format=%(refname:short)"],
    );
    let other = other.trim();
    git(&a, &["merge", "-q", "-X", "ours", "-m", "merge", other]);
    fs::write(a.join("shared.txt"), "a's version\n").unwrap();
    c.replicas[0].capture(None).await.unwrap();
    c.settle().await;
    for dir in [&a, &b] {
        assert_eq!(git(dir, &["status", "--porcelain"]), " M shared.txt\n");
        assert_eq!(
            fs::read_to_string(dir.join("shared.txt")).unwrap(),
            "a's version\n"
        );
        assert_eq!(fs::read_to_string(dir.join("a.txt")).unwrap(), "from a\n");
        assert!(dir.join("b.txt").exists());
        let branches = git(dir, &["branch", "--format=%(refname:short)"]);
        assert!(!branches.contains("conflict"), "{branches}");
        assert_eq!(conflict_copies(dir), Vec::<String>::new());
    }
}

#[tokio::test]
async fn git_refs_fast_forward() {
    // Three commits; two refs that machines move concurrently, each time
    // one machine ahead of the other (as after fetching at different
    // times). The descendant wins whichever write the log orders last.
    let mut c = Cluster::new(2, |root| {
        git_repo(root);
        for i in 0..3 {
            fs::write(root.join("a.txt"), format!("v{i}\n")).unwrap();
            git(root, &["commit", "-q", "-am", &format!("c{i}")]);
        }
        git(root, &["update-ref", "refs/remotes/origin/main", "HEAD~3"]);
        git(root, &["update-ref", "refs/remotes/origin/dev", "HEAD~3"]);
    })
    .await;
    let (a, b) = (c.dir(0).to_path_buf(), c.dir(1).to_path_buf());
    git(&a, &["update-ref", "refs/remotes/origin/main", "HEAD~1"]);
    git(&a, &["update-ref", "refs/remotes/origin/dev", "HEAD"]);
    git(&b, &["update-ref", "refs/remotes/origin/main", "HEAD"]);
    git(&b, &["update-ref", "refs/remotes/origin/dev", "HEAD~1"]);
    c.replicas[0].capture(None).await.unwrap();
    c.replicas[1].capture(None).await.unwrap();
    c.settle().await;
    let head = git(&a, &["rev-parse", "HEAD"]);
    for dir in [&a, &b] {
        assert_eq!(git(dir, &["rev-parse", "origin/main"]), head);
        assert_eq!(git(dir, &["rev-parse", "origin/dev"]), head);
        assert_eq!(conflict_copies(dir), Vec::<String>::new());
    }

    // Same with packed refs: both machines pack after moving refs.
    git(&a, &["update-ref", "refs/remotes/origin/main", "HEAD~2"]);
    git(&a, &["tag", "only-a"]);
    git(&a, &["pack-refs", "--all"]);
    git(&b, &["update-ref", "refs/remotes/origin/dev", "HEAD~2"]);
    git(&b, &["tag", "only-b"]);
    git(&b, &["pack-refs", "--all"]);
    c.replicas[0].capture(None).await.unwrap();
    c.replicas[1].capture(None).await.unwrap();
    c.settle().await;
    for dir in [&a, &b] {
        git(dir, &["fsck", "--strict"]);
        let tags = git(dir, &["tag"]);
        assert!(tags.contains("only-a") && tags.contains("only-b"), "{tags}");
        assert_eq!(conflict_copies(dir), Vec::<String>::new());
    }
}

#[tokio::test]
async fn git_worktree_links_work_on_every_machine() {
    let mut c = Cluster::new(2, git_repo).await;
    let (a, b) = (c.dir(0).to_path_buf(), c.dir(1).to_path_buf());
    git(&a, &["worktree", "add", "-q", "-b", "feature", "wt"]);
    c.settle().await;
    // The link inside the worktree is relative, so it is right everywhere.
    assert_eq!(
        fs::read_to_string(b.join("wt/.git")).unwrap(),
        "gitdir: ../.git/worktrees/wt\n"
    );
    for dir in [&a, &b] {
        let dir = dir.canonicalize().unwrap();
        let list = git(&dir, &["worktree", "list", "--porcelain"]);
        assert!(
            list.contains(&format!("worktree {}\n", dir.join("wt").display())),
            "{list}"
        );
        assert_eq!(
            git(&dir.join("wt"), &["rev-parse", "--show-toplevel"]).trim(),
            dir.join("wt").display().to_string()
        );
    }
    // Work in the worktree on the second machine comes back.
    fs::write(b.join("wt/f.txt"), "feature\n").unwrap();
    git(&b.join("wt"), &["add", "f.txt"]);
    git(&b.join("wt"), &["commit", "-q", "-m", "on b"]);
    c.replicas[1].capture(None).await.unwrap();
    c.settle().await;
    assert_eq!(git(&a.join("wt"), &["log", "-1", "--format=%s"]), "on b\n");
    assert_eq!(git(&a.join("wt"), &["status", "--porcelain"]), "");
    git(&a, &["worktree", "prune", "--dry-run"]);
    assert_eq!(git(&b, &["worktree", "prune", "-n", "-v"]), "");

    // Removing it on one machine removes it everywhere.
    git(&a, &["worktree", "remove", "wt"]);
    c.replicas[0].capture(None).await.unwrap();
    c.settle().await;
    assert!(!b.join("wt").exists());
    assert!(!b.join(".git/worktrees/wt").exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn running_daemons_propagate_changes() {
    let c = Cluster::new(2, sample_tree).await;
    let Cluster {
        _tmp,
        dirs,
        replicas,
        hub,
        ..
    } = c;
    let mut tasks = Vec::new();
    for mut r in replicas {
        let hub = hub.clone();
        tasks.push(tokio::spawn(async move {
            r.run(&hub, Duration::from_millis(50), Duration::from_secs(3600))
                .await
        }));
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    fs::write(dirs[0].join("src/live.txt"), "seen live").unwrap();
    fs::write(dirs[1].join("from-b.txt"), "b").unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while !(dirs[1].join("src/live.txt").exists() && dirs[0].join("from-b.txt").exists()) {
        assert!(std::time::Instant::now() < deadline, "changes did not propagate");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    tokio::time::sleep(Duration::from_millis(500)).await;
    for t in &tasks {
        t.abort();
    }
    assert_same(&dirs[0], &dirs[1]);
}

#[tokio::test(flavor = "multi_thread")]
async fn fsmonitor_hook_reports_changes() {
    use rustfuse_sync::fsmonitor::hook;
    fn answer(out: Vec<u8>) -> (String, Vec<String>) {
        let mut parts: Vec<String> = out
            .split(|&b| b == 0)
            .map(|p| String::from_utf8(p.to_vec()).unwrap())
            .collect();
        assert_eq!(parts.pop().as_deref(), Some(""));
        let token = parts.remove(0);
        (token, parts)
    }
    let c = Cluster::new(1, |root: &Path| {
        fs::create_dir_all(root.join("proj/src")).unwrap();
        fs::write(root.join("proj/src/lib.rs"), "fn main() {}").unwrap();
        git_repo(&root.join("proj"));
    })
    .await;
    let Cluster {
        _tmp,
        dirs,
        replicas,
        hub,
        ..
    } = c;
    let proj = dirs[0].join("proj");
    // Without a daemon, git is told to check everything.
    assert_eq!(answer(hook(2, "", &proj.join("src"))).1, ["/"]);

    let mut r = replicas.into_iter().next().unwrap();
    let task = tokio::spawn(async move {
        r.run(&hub, Duration::from_millis(50), Duration::from_secs(3600))
            .await
    });
    let sock = dirs[0].join(".rustfuse").join(rustfuse_sync::fsmonitor::SOCKET);
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while !sock.exists() {
        assert!(std::time::Instant::now() < deadline, "daemon did not start");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let ask = |token: String| {
        let proj = proj.clone();
        tokio::task::spawn_blocking(move || answer(hook(2, &token, &proj)))
    };
    let (t0, all) = ask(String::new()).await.unwrap();
    assert_eq!(all, ["/"]);

    // A change made just before the query is always in the answer.
    fs::write(proj.join("src/lib.rs"), "fn main() { edited() }").unwrap();
    fs::write(proj.join("new.txt"), "new").unwrap();
    fs::write(dirs[0].join("outside.txt"), "not in the repo").unwrap();
    let (t1, paths) = ask(t0).await.unwrap();
    assert_eq!(paths, ["new.txt", "new.txt/", "src/lib.rs", "src/lib.rs/"]);

    // git writing its own files changes nothing it needs to hear about.
    git(&proj, &["add", "-A"]);
    let (t2, paths) = ask(t1.clone()).await.unwrap();
    assert_eq!((t2, paths.len()), (t1, 0));
    task.abort();
}

#[tokio::test]
async fn git_cleanup_handles_odd_names_and_deletes() {
    let mut c = Cluster::new(2, |root: &Path| {
        git(root, &["init", "-q", "-b", "main"]);
        fs::write(root.join("with space.txt"), "base\n").unwrap();
        fs::write(root.join("gone file.txt"), "base\n").unwrap();
        fs::write(root.join("-dash.txt"), "base\n").unwrap();
        git(root, &["add", "."]);
        git(root, &["commit", "-q", "-m", "first"]);
    })
    .await;
    let (a, b) = (c.dir(0).to_path_buf(), c.dir(1).to_path_buf());
    fs::write(a.join("with space.txt"), "a\n").unwrap();
    fs::write(a.join("-dash.txt"), "a\n").unwrap();
    git(&a, &["rm", "-q", "gone file.txt"]);
    git(&a, &["commit", "-q", "-am", "on a"]);
    fs::write(b.join("with space.txt"), "b\n").unwrap();
    fs::create_dir(b.join("dir with space")).unwrap();
    fs::write(b.join("dir with space/new.txt"), "b\n").unwrap();
    fs::write(b.join("gone file.txt"), "b\n").unwrap();
    git(&b, &["add", "."]);
    git(&b, &["commit", "-q", "-m", "on b"]);
    c.replicas[0].capture(None).await.unwrap();
    c.replicas[1].capture(None).await.unwrap();
    c.settle().await;
    let main = git(&a, &["rev-parse", "main"]);
    for dir in [&a, &b] {
        assert_eq!(git(dir, &["rev-parse", "main"]), main);
        assert_eq!(
            git(dir, &["status", "--porcelain"]),
            "",
            "worktree not clean in {}",
            dir.display()
        );
        let copies: Vec<String> = conflict_copies(dir)
            .into_iter()
            .filter(|c| !c.contains(".git/"))
            .collect();
        assert_eq!(copies, Vec::<String>::new(), "leftover conflict copies");
    }
}

#[tokio::test]
async fn git_cleanup_is_retried_after_a_failure() {
    let mut c = Cluster::new(2, |root: &Path| {
        git(root, &["init", "-q", "-b", "main"]);
        fs::write(root.join("a.txt"), "base\n").unwrap();
        fs::write(root.join("b.txt"), "base\n").unwrap();
        git(root, &["add", "."]);
        git(root, &["commit", "-q", "-m", "first"]);
    })
    .await;
    let (a, b) = (c.dir(0).to_path_buf(), c.dir(1).to_path_buf());
    fs::write(a.join("a.txt"), "a\n").unwrap();
    git(&a, &["commit", "-q", "-am", "on a"]);
    fs::write(b.join("b.txt"), "b\n").unwrap();
    git(&b, &["commit", "-q", "-am", "on b"]);
    // A git command left its lock behind: cleaning up fails for now.
    for dir in [&a, &b] {
        fs::write(dir.join(".git/index.lock"), "").unwrap();
    }
    c.settle().await;
    for dir in [&a, &b] {
        fs::remove_file(dir.join(".git/index.lock")).unwrap();
    }
    c.settle().await;
    let main = git(&a, &["rev-parse", "main"]);
    for dir in [&a, &b] {
        assert_eq!(git(dir, &["rev-parse", "main"]), main);
        assert_eq!(
            git(dir, &["status", "--porcelain"]),
            "",
            "worktree not clean in {}",
            dir.display()
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn large_files_stream_and_reuse_local_chunks() {
    let size = 80 << 20;
    let mut c = Cluster::new(2, |root: &Path| {
        fs::write(root.join("huge.bin"), pseudo_random(size, 11)).unwrap();
    })
    .await;
    let src = c.dir(0).to_path_buf();
    // Overwrite a stretch in the middle: the other machine rebuilds the
    // file from its own copy plus the changed chunks.
    let mut data = fs::read(src.join("huge.bin")).unwrap();
    data[40 << 20..(40 << 20) + (1 << 20)].copy_from_slice(&pseudo_random(1 << 20, 12));
    fs::write(src.join("huge.bin"), &data).unwrap();
    c.settle().await;
    // Moved and edited in the same round, and grown past a window.
    fs::create_dir(src.join("sub")).unwrap();
    fs::rename(src.join("huge.bin"), src.join("sub/huge.bin")).unwrap();
    data.extend_from_slice(&pseudo_random(70 << 20, 13));
    data[..4096].copy_from_slice(&[7; 4096]);
    fs::write(src.join("sub/huge.bin"), &data).unwrap();
    fs::set_permissions(src.join("sub/huge.bin"), fs::Permissions::from_mode(0o600)).unwrap();
    c.settle().await;
    assert_eq!(fs::read(c.dir(1).join("sub/huge.bin")).unwrap(), data);
}

/// Runs cleanup as if `days` had passed (plus `extra`).
async fn gc_at(c: &Cluster, days: u64, extra: Duration) -> rustfuse_sync::gc::GcStats {
    let opts = rustfuse_sync::gc::GcOptions {
        now: SystemTime::now() + Duration::from_secs(days * 24 * 3600) + extra,
        ..Default::default()
    };
    rustfuse_sync::gc::collect(c.volume.clone(), &opts).await.unwrap()
}

async fn pack_count(c: &Cluster) -> usize {
    c.volume.list("pack/", None, 10_000).await.unwrap().objects.len()
}

#[tokio::test(flavor = "multi_thread")]
async fn cleanup_deletes_unused_packs_in_two_steps() {
    let mut c = Cluster::new(3, sample_tree).await;
    c.settle().await;
    // Content that is created, synced and deleted again, in its own packs.
    fs::write(c.dir(0).join("scratch.bin"), pseudo_random(3 << 20, 21)).unwrap();
    c.settle().await;
    fs::remove_file(c.dir(0).join("scratch.bin")).unwrap();
    c.settle().await;
    // Machine 2 lags behind: it misses a file that comes and goes.
    fs::write(c.dir(0).join("brief.bin"), pseudo_random(2 << 20, 22)).unwrap();
    c.sync(0).await;
    fs::remove_file(c.dir(0).join("brief.bin")).unwrap();
    c.sync(0).await;
    c.sync(1).await;
    let before = pack_count(&c).await;

    // Recent content stays, used or not.
    let st = gc_at(&c, 0, Duration::ZERO).await;
    assert_eq!((st.marked, st.deleted), (0, 0));
    // Old and unused: marked first, deleted only on a later run.
    let st = gc_at(&c, 30, Duration::ZERO).await;
    assert_eq!(st.marked, 2, "{st:?}");
    assert_eq!(st.deleted, 0);
    assert_eq!(pack_count(&c).await, before);
    let st = gc_at(&c, 30, Duration::from_secs(600)).await;
    assert_eq!((st.deleted, st.waiting), (0, 2), "too early: {st:?}");
    let st = gc_at(&c, 30, Duration::from_secs(7200)).await;
    assert_eq!(st.deleted, 2, "{st:?}");
    assert_eq!(pack_count(&c).await, before - 2);
    assert_eq!(gc_at(&c, 31, Duration::ZERO).await.marked, 0);

    // Everyone still converges, and a new machine attaches.
    c.settle().await;
    let late = c._tmp.path().join("late");
    let mut cfg = config("vol", 99);
    cfg.hub = Some(c.hub.addr.clone());
    Replica::attach(&late, cfg, c.volume.clone()).await.unwrap();
    assert_same(c.dir(0), &late);
}

#[tokio::test(flavor = "multi_thread")]
async fn unsent_changes_survive_cleanup_of_their_content() {
    // Machine 1 goes through the hub, machine 2 straight to the bucket.
    let mut c = Cluster::new(3, sample_tree).await;
    let a = pseudo_random(3 << 20, 31);
    let b = pseudo_random(3 << 20, 32);
    fs::write(c.dir(0).join("a.bin"), &a).unwrap();
    fs::write(c.dir(0).join("b.bin"), &b).unwrap();
    c.settle().await;
    fs::remove_file(c.dir(0).join("a.bin")).unwrap();
    fs::remove_file(c.dir(0).join("b.bin")).unwrap();
    c.sync(0).await;

    // Offline for a month, machines 1 and 2 keep copies of the content the
    // others deleted, or move it out of the way: their changes only point
    // at the old packs.
    for i in [1, 2] {
        fs::copy(c.dir(i).join("a.bin"), c.dir(i).join(format!("keep-a{i}.bin"))).unwrap();
    }
    fs::rename(c.dir(1).join("b.bin"), c.dir(1).join("keep-b1.bin")).unwrap();
    fs::copy(c.dir(2).join("b.bin"), c.dir(2).join("keep-b2.bin")).unwrap();
    // Cleanup marks the old packs; machine 1 sends after the deletion,
    // machine 2 while the packs are only marked.
    gc_at(&c, 30, Duration::ZERO).await;
    let (sent, _) = c.sync(2).await;
    assert!(sent > 0);
    let st = gc_at(&c, 30, Duration::from_secs(7200)).await;
    assert!(st.deleted > 0, "{st:?}");
    c.sync(1).await;
    c.settle().await;
    for i in 0..3 {
        assert_eq!(fs::read(c.dir(i).join("keep-a1.bin")).unwrap(), a);
        assert_eq!(fs::read(c.dir(i).join("keep-a2.bin")).unwrap(), a);
        assert_eq!(fs::read(c.dir(i).join("keep-b1.bin")).unwrap(), b);
        assert_eq!(fs::read(c.dir(i).join("keep-b2.bin")).unwrap(), b);
    }
    // Nothing anyone uses is left in a deleted or doomed pack.
    let st = gc_at(&c, 30, Duration::from_secs(4 * 3600)).await;
    assert_eq!(st.revived, 0, "{st:?}");
    let late = c._tmp.path().join("late");
    let mut cfg = config("vol", 99);
    cfg.hub = Some(c.hub.addr.clone());
    Replica::attach(&late, cfg, c.volume.clone()).await.unwrap();
    assert_same(c.dir(0), &late);
}

async fn pack_bytes(c: &Cluster) -> u64 {
    let page = c.volume.list("pack/", None, 10_000).await.unwrap();
    page.objects.iter().map(|o| o.size).sum()
}

#[tokio::test(flavor = "multi_thread")]
async fn compaction_moves_used_chunks_and_old_references_still_work() {
    let keep = pseudo_random(1 << 20, 41);
    let (k, d) = (keep.clone(), pseudo_random(10 << 20, 42));
    // Both files land in the same pack.
    let mut c = Cluster::new(3, move |root: &Path| {
        sample_tree(root);
        fs::write(root.join("keep.bin"), &k).unwrap();
        fs::write(root.join("drop.bin"), &d).unwrap();
    })
    .await;
    fs::remove_file(c.dir(0).join("drop.bin")).unwrap();
    c.sync(0).await;
    c.sync(1).await;
    // Machine 2 lags. Machine 0 copies the file it keeps while offline:
    // the copy reuses the stored chunks, so its change points into the
    // old pack.
    fs::copy(c.dir(0).join("keep.bin"), c.dir(0).join("keep-copy.bin")).unwrap();
    let before = pack_bytes(&c).await;

    let st = gc_at(&c, 30, Duration::ZERO).await;
    assert_eq!(st.compacted, 1, "{st:?}");
    assert!(st.compacted_to < st.compacted_from / 2, "{st:?}");
    let st = gc_at(&c, 30, Duration::from_secs(7200)).await;
    assert_eq!(st.deleted, 1, "{st:?}");
    assert!(pack_bytes(&c).await < before - (8 << 20));
    // A restarted machine 0 (its pack checks start over) sends the copy.
    let store = c.replicas[0].store().clone();
    let dir = c.dir(0).to_path_buf();
    drop(c.replicas.remove(0));
    c.replicas.insert(0, Replica::open(&dir, store).unwrap());

    // The snapshot and the log still name the old pack; everyone reads
    // through the forwarding table.
    c.settle().await;
    for i in 0..3 {
        assert_eq!(fs::read(c.dir(i).join("keep.bin")).unwrap(), keep);
        assert_eq!(fs::read(c.dir(i).join("keep-copy.bin")).unwrap(), keep);
        assert!(!c.dir(i).join("drop.bin").exists());
    }
    let late = c._tmp.path().join("late");
    let mut cfg = config("vol", 99);
    cfg.hub = Some(c.hub.addr.clone());
    Replica::attach(&late, cfg, c.volume.clone()).await.unwrap();
    assert_same(c.dir(0), &late);

    // Settled: nothing more to do, and the forwarding table stays while
    // anything names the old pack.
    let st = gc_at(&c, 31, Duration::ZERO).await;
    assert_eq!(
        (st.compacted, st.marked, st.deleted, st.forwards_deleted),
        (0, 0, 0, 0),
        "{st:?}"
    );
}

#[tokio::test]
async fn metrics_count_sync_activity() {
    let mut c = Cluster::new(2, sample_tree).await;
    fs::write(c.dir(0).join("new.txt"), "from machine 0").unwrap();
    fs::write(c.dir(1).join("other.bin"), pseudo_random(300_000, 7)).unwrap();
    c.settle().await;

    let hub = &c.hub_metrics;
    let head = c.replicas[0].applied();
    assert_eq!(hub.head.get("vol").get(), head as i64);
    assert_eq!(hub.appended_total.get("vol").get(), head);
    assert!(hub.connections_total.get("log").get() >= 4);
    // Machine 1 reads and writes its content through the hub.
    assert!(hub.store_requests_total.get("put").get() >= 1);
    assert!(hub.store_bytes_total.get("in").get() > 0);
    assert!(hub.store_bytes_total.get("out").get() > 0);
    assert_eq!(hub.refused_total.get(), 0);
    let text = hub.render();
    assert!(
        text.contains(&format!("rustfuse_hub_log_head{{volume=\"vol\"}} {head}\n")),
        "{text}"
    );
    assert!(
        text.contains("rustfuse_hub_store_seconds_count{op=\"put\"}"),
        "{text}"
    );

    for (i, r) in c.replicas.iter().enumerate() {
        let m = r.metrics();
        assert_eq!(m.applied.get(), head as i64, "machine {i}");
        assert_eq!(m.hub_head.get(), head as i64, "machine {i}");
        assert_eq!(m.outbox.get(), 0, "machine {i}");
        assert!(m.pushed_total.get() >= 1, "machine {i}");
        assert!(m.received_total.get() >= 1, "machine {i}");
        assert!(m.captures_total.get() >= 1, "machine {i}");
        assert!(m.render().contains(&format!("rustfuse_applied_seq {head}\n")));
    }
}

/// Drops all fetched content it may, except `busy`'s.
async fn evict(h: &Arc<rustfuse_sync::lazy::Hydrator>, busy: Option<u64>) -> rustfuse_sync::lazy::Evicted {
    let h = h.clone();
    tokio::task::spawn_blocking(move || h.evict(0, |ino| Some(ino) == busy))
        .await
        .unwrap()
        .unwrap()
}

#[tokio::test]
async fn lazy_directory_fetches_on_demand() {
    let mut c = Cluster::new(1, |root| {
        sample_tree(root);
        fs::create_dir_all(root.join("repo/.git/objects")).unwrap();
        fs::write(root.join("repo/.git/objects/pack"), pseudo_random(100_000, 9)).unwrap();
    })
    .await;
    let lazy = c._tmp.path().join("lazy");
    let mut cfg = config("vol", 9);
    cfg.hub = Some(c.hub.addr.clone());
    let mut r = Replica::attach_with(&lazy, cfg, c.volume.clone(), true)
        .await
        .unwrap();
    assert!(rustfuse_sync::lazy::is_lazy(&lazy));
    let src = c.dir(0).to_path_buf();
    let big = "src/nested/deep/big.bin";

    // Placeholders: right size, no content yet. Git data and small files
    // are there in full.
    let real = fs::read(src.join(big)).unwrap();
    let held = fs::read(lazy.join(big)).unwrap();
    assert_eq!(held.len(), real.len());
    assert!(held.iter().all(|&b| b == 0));
    assert_eq!(
        fs::read(lazy.join("repo/.git/objects/pack")).unwrap(),
        fs::read(src.join("repo/.git/objects/pack")).unwrap()
    );
    assert_eq!(fs::read(lazy.join("README.md")).unwrap(), b"hello\n");
    assert_eq!(
        fs::metadata(lazy.join(big)).unwrap().modified().unwrap(),
        fs::metadata(src.join(big)).unwrap().modified().unwrap()
    );

    // A placeholder whose metadata changes still syncs as its content.
    fs::set_permissions(lazy.join(big), fs::Permissions::from_mode(0o600)).unwrap();
    r.capture(None).await.unwrap();
    r.sync_once(&c.hub, TIMEOUT).await.unwrap();
    c.sync(0).await;
    assert_eq!(fs::read(src.join(big)).unwrap(), real);
    assert_eq!(
        fs::metadata(src.join(big)).unwrap().permissions().mode() & 0o777,
        0o600
    );

    // First use fetches the content, once.
    let h = rustfuse_sync::lazy::Hydrator::new(&lazy, c.volume.clone()).unwrap();
    assert!(h.hydrate(&lazy.join(big)).await.unwrap());
    assert!(!h.hydrate(&lazy.join(big)).await.unwrap());
    assert!(!h.hydrate(&lazy.join("README.md")).await.unwrap());
    assert_eq!(fs::read(lazy.join(big)).unwrap(), real);
    assert_eq!(
        fs::metadata(lazy.join(big)).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let s = r.sync_once(&c.hub, TIMEOUT).await.unwrap();
    assert_eq!(s.pushed, 0, "fetching is not an edit");

    // A change from elsewhere arrives as a new placeholder.
    let medium = "src/nested/medium.txt";
    fs::write(src.join(medium), "other\n".repeat(4000)).unwrap();
    c.sync(0).await;
    r.sync_once(&c.hub, TIMEOUT).await.unwrap();
    assert!(fs::read(lazy.join(medium)).unwrap().iter().all(|&b| b == 0));
    assert!(h.hydrate(&lazy.join(medium)).await.unwrap());
    assert_eq!(
        fs::read(lazy.join(medium)).unwrap(),
        fs::read(src.join(medium)).unwrap()
    );

    // Fetched content can be dropped again, but not right after use, nor
    // while the file is open.
    let h = Arc::new(h);
    let e = evict(&h, None).await;
    assert_eq!((e.held, e.files), (real.len() as u64 + 4000 * 6, 0));
    let h = Arc::new(rustfuse_sync::lazy::Hydrator::new(&lazy, c.volume.clone()).unwrap());
    let busy = fs::metadata(lazy.join(medium)).unwrap().ino();
    let e = evict(&h, Some(busy)).await;
    assert_eq!((e.files, e.bytes), (1, real.len() as u64));
    assert!(fs::read(lazy.join(big)).unwrap().iter().all(|&b| b == 0));
    assert_eq!(fs::metadata(lazy.join(big)).unwrap().len(), real.len() as u64);
    let s = r.sync_once(&c.hub, TIMEOUT).await.unwrap();
    assert_eq!(s.pushed, 0, "dropping fetched content is not an edit");
    assert!(h.hydrate(&lazy.join(big)).await.unwrap());
    assert_eq!(fs::read(lazy.join(big)).unwrap(), real);

    // An edited file is the user's: never emptied.
    fs::write(lazy.join(medium), "mine\n").unwrap();
    let h = Arc::new(rustfuse_sync::lazy::Hydrator::new(&lazy, c.volume.clone()).unwrap());
    let e = evict(&h, None).await;
    assert_eq!((e.files, e.bytes), (1, real.len() as u64));
    assert_eq!(fs::read(lazy.join(medium)).unwrap(), b"mine\n");

    // Records of replaced placeholders go away. Written to without the
    // mount, a placeholder of a new size counts as what was written.
    fs::write(lazy.join(big), "replaced\n").unwrap();
    r.sync_once(&c.hub, TIMEOUT).await.unwrap();
    c.sync(0).await;
    assert_eq!(fs::read(src.join(big)).unwrap(), b"replaced\n");
    drop(r);
    Replica::open(&lazy, c.volume.clone()).unwrap();
    assert_eq!(
        fs::read_dir(lazy.join(STATE_DIR).join("lazy")).unwrap().count(),
        0
    );
}
