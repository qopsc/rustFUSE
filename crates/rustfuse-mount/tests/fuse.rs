//! Mounts a directory through FUSE and works on it like a user would.
//! Skipped where FUSE mounts are not possible (no /dev/fuse, not allowed).

#![cfg(target_os = "linux")]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::sync::Arc;

use rustfuse_mount::{Backing, mount_fuse};

#[test]
fn files_written_through_the_mount_land_in_the_directory() {
    let dir = tempfile::tempdir().unwrap();
    let mnt = tempfile::tempdir().unwrap();
    fs::create_dir(dir.path().join(".rustfuse")).unwrap();
    fs::write(dir.path().join("old.txt"), "old").unwrap();
    let backing = Arc::new(Backing::new(dir.path(), &[".rustfuse"]).unwrap());
    let m = match mount_fuse(backing, mnt.path(), false) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("skipping: cannot mount FUSE here: {e}");
            return;
        }
    };
    let at = |p: &str| mnt.path().join(p);
    assert_eq!(fs::read_to_string(at("old.txt")).unwrap(), "old");
    let names: Vec<_> = fs::read_dir(mnt.path())
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    assert_eq!(names, vec!["old.txt"], "state folder is hidden");

    fs::create_dir_all(at("a/b")).unwrap();
    fs::write(at("a/b/new.txt"), "new").unwrap();
    fs::rename(at("a/b/new.txt"), at("a/renamed.txt")).unwrap();
    fs::set_permissions(at("a/renamed.txt"), fs::Permissions::from_mode(0o600)).unwrap();
    std::os::unix::fs::symlink("renamed.txt", at("a/link")).unwrap();
    fs::remove_file(at("old.txt")).unwrap();
    // Lock files the way git takes them.
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(at("a/index.lock"))
        .unwrap();
    assert!(
        fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(at("a/index.lock"))
            .is_err()
    );
    assert!(fs::hard_link(at("a/renamed.txt"), at("a/hard")).is_err());

    let real = |p: &str| dir.path().join(p);
    assert_eq!(fs::read_to_string(real("a/renamed.txt")).unwrap(), "new");
    assert_eq!(
        fs::metadata(real("a/renamed.txt")).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert_eq!(
        fs::read_link(real("a/link")).unwrap().to_str(),
        Some("renamed.txt")
    );
    assert!(!real("old.txt").exists() && !real("a/b/new.txt").exists());

    // A change made underneath (by the sync engine) shows in the mount.
    fs::write(real("a/remote.txt"), "from elsewhere").unwrap();
    assert_eq!(fs::read_to_string(at("a/remote.txt")).unwrap(), "from elsewhere");
    m.unmount().unwrap();
}
