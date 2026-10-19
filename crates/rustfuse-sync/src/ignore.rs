//! Paths that stay local and are never synced (`docs/design.md` §6).
//!
//! In priority order:
//! 1. Built-in rules: rustfuse's own state and git's transient lock files.
//! 2. `.rustfuseignore` at the root (gitignore syntax; `!pattern` re-includes
//!    something git ignores).
//! 3. Directories excluded by the repository's own `.gitignore` files and
//!    `.git/info/exclude`. Build output (`target/`, `node_modules/`, …) is
//!    gitignored in practice, so it stays local without a hard-coded list
//!    that could hide tracked files that happen to share those names. Only
//!    directories: ignored-but-tracked files such as `Cargo.lock` are common.
//!    Git never applies these rules inside `.git/`, and neither do we.

use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};
use std::sync::Mutex;

use ignore::Match;
use ignore::gitignore::{Gitignore, GitignoreBuilder};

pub const DEFAULT_PATTERNS: &[&str] = &[
    "/.rustfuse/",
    ".rustfuse-tmp-*",
    ".DS_Store",
    "**/.git/**/*.lock",
    "**/.git/gc.pid",
    "**/.git/fsmonitor--daemon/",
    // Each machine points a linked worktree's admin dir at its own path.
    "**/.git/worktrees/*/gitdir",
];

/// Per-volume exclusion file in the synced directory's root.
pub const IGNORE_FILE: &str = ".rustfuseignore";

pub struct Ignore {
    root: PathBuf,
    own: Gitignore,
    exclude: Gitignore,
    /// `.gitignore` matcher of each directory seen (None: no such file).
    dirs: Mutex<HashMap<PathBuf, Option<Gitignore>>>,
}

fn decide(m: Match<&ignore::gitignore::Glob>) -> Option<bool> {
    match m {
        Match::Ignore(_) => Some(true),
        Match::Whitelist(_) => Some(false),
        Match::None => None,
    }
}

impl Ignore {
    pub fn load(root: &Path) -> Result<Ignore, ignore::Error> {
        let mut b = GitignoreBuilder::new(root);
        for p in DEFAULT_PATTERNS {
            b.add_line(None, p)?;
        }
        let file = root.join(IGNORE_FILE);
        if file.exists()
            && let Some(e) = b.add(&file)
        {
            return Err(e);
        }
        let own = b.build()?;

        let mut x = GitignoreBuilder::new(root);
        let exclude_file = root.join(".git/info/exclude");
        if exclude_file.is_file() {
            // A malformed exclude file is git's problem; skip it.
            let _ = x.add(&exclude_file);
        }
        Ok(Ignore {
            root: root.to_owned(),
            own,
            exclude: x.build()?,
            dirs: Mutex::new(HashMap::new()),
        })
    }

    /// Forgets cached `.gitignore` rules (call when one changes).
    pub fn invalidate(&self) {
        self.dirs.lock().unwrap().clear();
    }

    fn dir_matcher(&self, rel_dir: &Path) -> Option<Gitignore> {
        let mut cache = self.dirs.lock().unwrap();
        cache
            .entry(rel_dir.to_owned())
            .or_insert_with(|| {
                let abs = self.root.join(rel_dir);
                let file = abs.join(".gitignore");
                if !file.is_file() {
                    return None;
                }
                let mut b = GitignoreBuilder::new(&abs);
                let _ = b.add(&file);
                b.build().ok()
            })
            .clone()
    }

    /// Whether a path relative to the root is excluded.
    pub fn is_ignored(&self, rel: &Path, is_dir: bool) -> bool {
        if let Some(d) = decide(self.own.matched_path_or_any_parents(rel, is_dir)) {
            return d;
        }
        if rel.components().any(|c| c == Component::Normal(".git".as_ref())) {
            return false;
        }
        // Git's rules only exclude directories. Repositories commonly track
        // files their .gitignore matches (e.g. Cargo.lock), and losing a
        // tracked file is far worse than syncing a stray log file.
        if is_dir {
            self.git_ignores_dir(rel)
        } else {
            rel.parent()
                .is_some_and(|p| !p.as_os_str().is_empty() && self.git_ignores_dir(p))
        }
    }

    fn git_ignores_dir(&self, rel: &Path) -> bool {
        let is_dir = true;
        let abs = self.root.join(rel);
        let mut dir = rel.parent();
        while let Some(d) = dir {
            if let Some(m) = self.dir_matcher(d)
                && let Some(decision) = decide(m.matched_path_or_any_parents(&abs, is_dir))
            {
                return decision;
            }
            dir = d.parent();
        }
        decide(self.exclude.matched_path_or_any_parents(rel, is_dir)).unwrap_or(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn builtin_rules() {
        let dir = tempfile::tempdir().unwrap();
        let ig = Ignore::load(dir.path()).unwrap();
        assert!(ig.is_ignored(Path::new(".rustfuse"), true));
        assert!(ig.is_ignored(Path::new(".git/index.lock"), false));
        assert!(ig.is_ignored(Path::new(".git/refs/heads/main.lock"), false));
        assert!(ig.is_ignored(Path::new(".git/worktrees/wt/index.lock"), false));
        assert!(ig.is_ignored(Path::new("repos/a/.git/index.lock"), false));
        assert!(ig.is_ignored(Path::new(".git/worktrees/wt/gitdir"), false));
        assert!(!ig.is_ignored(Path::new(".git/worktrees/wt/HEAD"), false));
        assert!(!ig.is_ignored(Path::new("Cargo.lock"), false));
        assert!(!ig.is_ignored(Path::new(".git/index"), false));
        // No hard-coded build dirs: a tracked `target` directory syncs.
        assert!(!ig.is_ignored(Path::new("tests/target/mod.rs"), false));
    }

    #[test]
    fn honours_gitignore_hierarchy() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("sub/deeper")).unwrap();
        fs::create_dir_all(root.join(".git/info")).unwrap();
        fs::write(root.join(".gitignore"), "/target\n*.log\nnode_modules/\n").unwrap();
        fs::write(root.join("sub/.gitignore"), "!keep.log\nlocal/\n").unwrap();
        fs::write(root.join(".git/info/exclude"), "scratch.txt\n").unwrap();
        let ig = Ignore::load(root).unwrap();
        assert!(ig.is_ignored(Path::new("target"), true));
        assert!(
            !ig.is_ignored(Path::new("sub/target"), true),
            "anchored pattern leaked"
        );
        assert!(ig.is_ignored(Path::new("a/b/node_modules"), true));
        // Files matching .gitignore still sync (they may be tracked).
        assert!(!ig.is_ignored(Path::new("x.log"), false));
        assert!(!ig.is_ignored(Path::new("sub/deeper/other.log"), false));
        assert!(ig.is_ignored(Path::new("sub/local"), true));
        assert!(ig.is_ignored(Path::new("sub/local/file"), false));
        assert!(ig.is_ignored(Path::new("target/debug/x"), false));
        fs::write(root.join(".git/info/exclude"), "scratch/\n").unwrap();
        let ig = Ignore::load(root).unwrap();
        assert!(ig.is_ignored(Path::new("scratch"), true));
        assert!(
            !ig.is_ignored(Path::new(".git/logs/HEAD"), false),
            "gitignore applied inside .git"
        );
        assert!(!ig.is_ignored(Path::new("src/main.rs"), false));
    }

    #[test]
    fn rustfuseignore_overrides_git() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join(".gitignore"), "build/\n").unwrap();
        fs::write(dir.path().join(IGNORE_FILE), "!build/\n*.tmp\n").unwrap();
        let ig = Ignore::load(dir.path()).unwrap();
        assert!(!ig.is_ignored(Path::new("build"), true));
        assert!(ig.is_ignored(Path::new("a.tmp"), false));
    }
}
