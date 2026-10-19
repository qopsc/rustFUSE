//! End-to-end: a writer pushes a directory, a follower materializes it, and
//! both stay byte-for-byte identical through edits, moves and deletes.
//!
//! Runs against the in-memory store, or against a real S3 endpoint when
//! `RUSTFUSE_TEST_S3_ENDPOINT` is set.

use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use rustfuse_store::{MemoryStore, ObjectStore, S3Config, S3Store};
use rustfuse_sync::{Config, Replica, Role, STATE_DIR};
use rustfuse_types::ReplicaId;

async fn store(volume_prefix: &str) -> Arc<dyn ObjectStore> {
    match std::env::var("RUSTFUSE_TEST_S3_ENDPOINT") {
        Ok(endpoint) => {
            let mut cfg =
                S3Config::new(std::env::var("RUSTFUSE_TEST_S3_BUCKET").unwrap_or("rustfuse-test".into()));
            cfg.endpoint = Some(endpoint);
            cfg.prefix = volume_prefix.to_owned();
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

fn config(replica: u64, role: Role) -> Config {
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
    Config {
        volume: format!("test-{nanos}"),
        bucket: "unused".into(),
        endpoint: None,
        region: "us-east-1".into(),
        prefix: String::new(),
        path_style: true,
        replica: ReplicaId(replica),
        seed: [replica as u8; 16],
        role,
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
        panic!("trees differ\nonly in source: {only_a:#?}\nonly in follower: {only_b:#?}");
    }
}

struct Pair {
    _tmp: tempfile::TempDir,
    src: PathBuf,
    dst: PathBuf,
    writer: Replica,
    follower: Replica,
}

impl Pair {
    async fn new(populate: impl FnOnce(&Path)) -> Pair {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        let dst = tmp.path().join("dst");
        fs::create_dir(&src).unwrap();
        populate(&src);
        let w = config(1, Role::Writer);
        let mut f = config(2, Role::Follower);
        f.volume = w.volume.clone();
        let s = store(&w.volume).await;
        let (writer, _) = Replica::init(&src, w, s.clone()).await.unwrap();
        let (follower, _) = Replica::attach(&dst, f, s).await.unwrap();
        assert_same(&src, &dst);
        Pair {
            _tmp: tmp,
            src,
            dst,
            writer,
            follower,
        }
    }

    async fn sync(&mut self) -> bool {
        let pushed = self.writer.push(None).await.unwrap().is_some();
        self.follower.pull().await.unwrap();
        assert_same(&self.src, &self.dst);
        pushed
    }
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

#[tokio::test]
async fn initial_sync_and_exclusions() {
    let p = Pair::new(sample_tree).await;
    assert!(!p.dst.join("target").exists(), "gitignored build dir was synced");
    assert!(
        p.dst.join("tests/target/mod.rs").exists(),
        "tracked dir named target was skipped"
    );
    assert!(p.dst.join("empty").is_dir());
    assert_eq!(
        fs::read_link(p.dst.join("link")).unwrap(),
        PathBuf::from("src/main.rs")
    );
}

#[tokio::test]
async fn edits_moves_and_deletes() {
    let mut p = Pair::new(sample_tree).await;
    assert!(!p.sync().await, "no-op push produced a changeset");

    // Edit in place, append to a large file, chmod.
    fs::write(p.src.join("README.md"), "hello world\n").unwrap();
    let mut big = fs::read(p.src.join("src/nested/deep/big.bin")).unwrap();
    big.extend_from_slice(b"appended");
    fs::write(p.src.join("src/nested/deep/big.bin"), &big).unwrap();
    fs::set_permissions(p.src.join("src/main.rs"), fs::Permissions::from_mode(0o600)).unwrap();
    assert!(p.sync().await);

    // Rename a file, move a directory, delete a file and a directory.
    fs::rename(p.src.join("README.md"), p.src.join("README.txt")).unwrap();
    fs::rename(p.src.join("src/nested"), p.src.join("moved")).unwrap();
    fs::remove_file(p.src.join("run.sh")).unwrap();
    fs::remove_dir(p.src.join("empty")).unwrap();
    assert!(p.sync().await);

    // Swap two names through a temp name, and move a file into a new dir.
    fs::write(p.src.join("a"), "A").unwrap();
    fs::write(p.src.join("b"), "B").unwrap();
    assert!(p.sync().await);
    fs::rename(p.src.join("a"), p.src.join("tmp")).unwrap();
    fs::rename(p.src.join("b"), p.src.join("a")).unwrap();
    fs::rename(p.src.join("tmp"), p.src.join("b")).unwrap();
    fs::create_dir(p.src.join("fresh")).unwrap();
    fs::rename(p.src.join("README.txt"), p.src.join("fresh/README.txt")).unwrap();
    assert!(p.sync().await);

    // Replace a directory with a file of the same name, and a file with a symlink.
    fs::remove_dir_all(p.src.join("moved")).unwrap();
    fs::write(p.src.join("moved"), "now a file").unwrap();
    fs::remove_file(p.src.join("a")).unwrap();
    symlink("b", p.src.join("a")).unwrap();
    assert!(p.sync().await);
}

#[tokio::test]
async fn moves_do_not_reupload() {
    let mut p = Pair::new(sample_tree).await;
    fs::rename(p.src.join("src"), p.src.join("source")).unwrap();
    let stats = p.writer.push(None).await.unwrap().unwrap();
    assert_eq!(stats.files_read, 0, "moved files were re-read");
    assert_eq!(stats.bytes_uploaded, 0);
    p.follower.pull().await.unwrap();
    assert_same(&p.src, &p.dst);
}

#[tokio::test]
async fn partial_scan_from_watch_paths() {
    let mut p = Pair::new(sample_tree).await;
    fs::write(p.src.join("src/nested/new.txt"), "new").unwrap();
    fs::remove_file(p.src.join("src/main.rs")).unwrap();
    let dirs = vec![PathBuf::from("src/nested/new.txt"), PathBuf::from("src/main.rs")];
    let stats = p.writer.push(Some(dirs)).await.unwrap().unwrap();
    assert_eq!(stats.ops, 2);
    p.follower.pull().await.unwrap();
    assert_same(&p.src, &p.dst);
}

#[tokio::test]
async fn reopen_and_follower_cannot_push() {
    let mut p = Pair::new(sample_tree).await;
    let store = p.writer_store();
    drop(p.writer);
    let mut writer = Replica::open(&p.src, store.clone()).unwrap();
    assert!(writer.push(None).await.unwrap().is_none());
    fs::write(p.src.join("x"), "x").unwrap();
    assert!(writer.push(None).await.unwrap().is_some());
    assert!(matches!(
        p.follower.push(None).await,
        Err(rustfuse_sync::Error::NotWriter)
    ));
    p.follower.pull().await.unwrap();
    assert_same(&p.src, &p.dst);
}

impl Pair {
    fn writer_store(&self) -> Arc<dyn ObjectStore> {
        self.writer.store().clone()
    }
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
async fn git_repository_with_worktree() {
    let mut p = Pair::new(|root| {
        git(root, &["init", "-q", "-b", "main"]);
        fs::write(root.join("a.txt"), "one\n").unwrap();
        git(root, &["add", "."]);
        git(root, &["commit", "-q", "-m", "first"]);
    })
    .await;
    assert_eq!(git(&p.dst, &["log", "--format=%s"]), "first\n");

    // Commits, branches, a worktree inside the volume and a gc (packs refs
    // and objects) all replicate into a repository git considers healthy.
    for i in 0..5 {
        fs::write(p.src.join("a.txt"), format!("change {i}\n")).unwrap();
        git(&p.src, &["commit", "-q", "-am", &format!("change {i}")]);
    }
    git(&p.src, &["branch", "feature"]);
    git(&p.src, &["worktree", "add", "-q", "wt", "feature"]);
    fs::write(p.src.join("wt/b.txt"), "on feature\n").unwrap();
    git(&p.src.join("wt"), &["add", "b.txt"]);
    git(&p.src.join("wt"), &["commit", "-q", "-m", "feature work"]);
    p.sync().await;
    git(&p.src, &["gc", "-q"]);
    p.sync().await;

    git(&p.dst, &["fsck", "--strict"]);
    assert_eq!(
        git(&p.dst, &["log", "--format=%s", "feature"]),
        git(&p.src, &["log", "--format=%s", "feature"])
    );
    assert_eq!(git(&p.dst.join("wt"), &["status", "--porcelain"]), "");
    assert_eq!(
        git(&p.dst, &["worktree", "list", "--porcelain"]).lines().count(),
        git(&p.src, &["worktree", "list", "--porcelain"]).lines().count()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn watch_pushes_changes() {
    let mut p = Pair::new(sample_tree).await;
    let src = p.src.clone();
    let editor = tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        fs::write(src.join("src/watched.txt"), "seen by the watcher").unwrap();
        fs::rename(src.join("README.md"), src.join("src/README.md")).unwrap();
    });
    let watch = p.writer.watch_push(
        std::time::Duration::from_millis(100),
        std::time::Duration::from_secs(3600),
    );
    // The watcher runs until cancelled; give it time to see the edits.
    let _ = tokio::time::timeout(std::time::Duration::from_millis(1500), watch).await;
    editor.await.unwrap();
    p.follower.pull().await.unwrap();
    assert!(
        p.dst.join("src/watched.txt").exists(),
        "watcher did not push the new file"
    );
    assert_same(&p.src, &p.dst);
}
