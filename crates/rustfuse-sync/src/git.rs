//! Git-aware tidying after conflicts (`docs/design.md` §7.1).
//!
//! The log resolves every conflict with the generic rules, which may leave
//! conflict copies inside `.git/`. After each sync round every machine runs
//! this pass on its own disk; each fix is a deterministic function of what
//! the log produced, so machines doing it at the same time write the same
//! bytes and their changes merge cleanly.
//!
//! - Object, index and other cache copies are dropped (objects are
//!   content-addressed; the rest git rebuilds).
//! - A ref and its conflict copy are merged by ancestry: if one commit
//!   contains the other the descendant wins and the copy goes away;
//!   otherwise the copy stays as a branch (`main.conflict-…`) to merge.
//! - `packed-refs` is merged per ref with the same rule, reflogs by union.
//! - When a checked-out branch was resolved, worktree files that still hold
//!   the losing commit's version are brought back to the winner's, so
//!   `git status` is clean. Only files exactly matching a commit are
//!   touched, so no uncommitted work is lost.
//! - Linked worktrees: the `.git` file in a worktree is made relative so it
//!   works on every machine, and the per-machine back pointer
//!   (`.git/worktrees/<id>/gitdir`, not synced) is written locally.
//! - New repositories get `core.checkStat=minimal` and
//!   `core.trustctime=false`, so an index synced from another machine stays
//!   valid instead of making `git status` rehash every file.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::Write;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Output, Stdio};

use rustfuse_types::Content;
use tracing::{debug, info, warn};

use crate::STATE_DIR;
use crate::tree::Tree;

const HANDLED_FILE: &str = "git-handled";

/// Copies of these (relative to a git dir) are dropped: caches and
/// transient files git rewrites on its own.
const DISPOSABLE: &[&str] = &[
    "FETCH_HEAD",
    "ORIG_HEAD",
    "AUTO_MERGE",
    "COMMIT_EDITMSG",
    "gc.log",
];

/// The original name of a conflict copy (`a.conflict-1a2b3c4d-20261019-155306.txt`
/// → `a.txt`), or `None` if `name` is not one.
pub fn original_name(name: &[u8]) -> Option<Vec<u8>> {
    const TAG: &[u8] = b".conflict-";
    // `.conflict-` + 8 hex + `-` + 8 digits + `-` + 6 digits
    const LEN: usize = 10 + 8 + 1 + 8 + 1 + 6;
    let start = name.windows(TAG.len()).rposition(|w| w == TAG)?;
    let tag = name.get(start..start + LEN)?;
    let rest = &tag[TAG.len()..];
    let ok = rest[..8].iter().all(u8::is_ascii_hexdigit)
        && rest[8] == b'-'
        && rest[9..17].iter().all(u8::is_ascii_digit)
        && rest[17] == b'-'
        && rest[18..24].iter().all(u8::is_ascii_digit);
    if !ok || start == 0 {
        return None;
    }
    let mut out = name[..start].to_vec();
    out.extend_from_slice(&name[start + LEN..]);
    Some(out)
}

/// What the tidy pass looks at, taken from the tree so the pass itself can
/// run on a blocking thread.
#[derive(Debug, Default)]
pub struct Plan {
    /// Git dirs (`.git` directories), relative to the root, each with the
    /// conflict copies inside it (relative to the git dir).
    repos: BTreeMap<PathBuf, Vec<PathBuf>>,
    /// `.git` files of linked worktrees and their content.
    pointers: Vec<(PathBuf, Option<Vec<u8>>)>,
}

impl Plan {
    pub fn from_tree(tree: &Tree) -> Plan {
        let mut plan = Plan::default();
        let mut copies = Vec::new();
        for (&id, node) in tree.iter() {
            let e = &node.entry;
            if e.name == b".git" {
                match &e.content {
                    Content::Dir => {
                        if let Some(p) = tree.path(id) {
                            plan.repos.entry(p).or_default();
                        }
                    }
                    Content::Inline(data) if data.starts_with(b"gitdir: ") => {
                        if let Some(p) = tree.path(id) {
                            plan.pointers.push((p, Some(data.clone())));
                        }
                    }
                    // A long path does not fit inline; read it from disk.
                    Content::File { size, .. } if *size < 4096 => {
                        if let Some(p) = tree.path(id) {
                            plan.pointers.push((p, None));
                        }
                    }
                    _ => {}
                }
            } else if original_name(&e.name).is_some()
                && let Some(p) = tree.path(id)
            {
                copies.push(p);
            }
        }
        for p in copies {
            let comps: Vec<Component> = p.components().collect();
            let Some(i) = comps.iter().rposition(|c| c.as_os_str() == ".git") else {
                continue;
            };
            let gitdir: PathBuf = comps[..=i].iter().collect();
            let inner: PathBuf = comps[i + 1..].iter().collect();
            if let Some(list) = plan.repos.get_mut(&gitdir) {
                list.push(inner);
            }
        }
        plan.pointers.sort();
        plan
    }

    pub fn is_empty(&self) -> bool {
        self.repos.is_empty() && self.pointers.is_empty()
    }
}

/// Per-machine state of the tidy pass.
pub struct GitTidy {
    root: PathBuf,
    /// Diverged ref copies already handled (relative to the root): their
    /// worktrees were cleaned once and are left alone from then on.
    handled: BTreeSet<PathBuf>,
    configured: HashSet<PathBuf>,
    unavailable: bool,
}

/// A ref resolved in favour of `target` over `other`: worktrees on that
/// ref may still hold files from `other`.
struct Resolved {
    gitdir: PathBuf,
    /// `refs/heads/…`, or `None` for a worktree's own HEAD.
    refname: Option<String>,
    /// For a HEAD: the worktree's admin dir (the git dir itself for the
    /// main worktree).
    admin: PathBuf,
    target: String,
    other: String,
}

impl GitTidy {
    pub fn load(root: &Path) -> GitTidy {
        let handled = fs::read(root.join(STATE_DIR).join(HANDLED_FILE))
            .map(|data| {
                data.split(|&b| b == b'\n')
                    .filter(|l| !l.is_empty())
                    .map(|l| PathBuf::from(OsStr::from_bytes(l)))
                    .collect()
            })
            .unwrap_or_default();
        GitTidy {
            root: root.to_owned(),
            handled,
            configured: HashSet::new(),
            unavailable: false,
        }
    }

    /// Runs the pass. Returns whether anything on disk changed.
    pub fn run(&mut self, plan: Plan) -> bool {
        if self.unavailable || plan.is_empty() {
            return false;
        }
        if Command::new("git").arg("--version").output().is_err() {
            warn!("git is not installed; git-aware conflict handling is off");
            self.unavailable = true;
            return false;
        }
        let mut changed = false;
        for (file, data) in &plan.pointers {
            let data = match data {
                Some(d) => d.clone(),
                None => match fs::read(self.root.join(file)) {
                    Ok(d) if d.starts_with(b"gitdir: ") => d,
                    _ => continue,
                },
            };
            changed |= self.link_worktree(file, &data);
        }
        let mut live = BTreeSet::new();
        for (gitdir, copies) in &plan.repos {
            let g = self.root.join(gitdir);
            if !g.is_dir() {
                continue;
            }
            changed |= self.configure(&g);
            let mut resolved = Vec::new();
            // Refs first: updating refs appends to reflogs, merged last.
            let mut copies = copies.clone();
            copies.sort_by_key(|c| c.starts_with("logs"));
            for inner in copies {
                let rel = gitdir.join(&inner);
                let before = resolved.len();
                match self.tidy_copy(&g, &inner, &mut resolved) {
                    Ok(Outcome::Changed) => {
                        changed = true;
                        if self.handled.contains(&rel) {
                            // A conflict branch the user has since merged:
                            // the worktree is theirs, leave it alone.
                            resolved.truncate(before);
                        }
                    }
                    Ok(Outcome::Kept) => {
                        live.insert(rel);
                    }
                    Ok(Outcome::Diverged) => {
                        live.insert(rel.clone());
                        if self.handled.insert(rel.clone()) {
                            info!(
                                branch = %rel.display(),
                                "a branch moved on two machines at once; the other side is kept as a branch to merge"
                            );
                        } else {
                            // Already cleaned up once; do not touch again.
                            resolved.pop();
                        }
                    }
                    Err(e) => {
                        debug!(copy = %rel.display(), "git conflict not resolved yet: {e}");
                        live.insert(rel);
                    }
                }
            }
            for r in resolved {
                match self.clean_worktrees(&r) {
                    Ok(c) => changed |= c,
                    Err(e) => {
                        warn!(gitdir = %r.gitdir.display(), "cleaning worktree after a ref conflict: {e}")
                    }
                }
            }
        }
        let before = self.handled.len();
        self.handled.retain(|p| live.contains(p));
        if self.handled.len() != before || changed {
            self.save_handled();
        }
        changed
    }

    fn save_handled(&self) {
        let mut data = Vec::new();
        for p in &self.handled {
            data.extend_from_slice(p.as_os_str().as_bytes());
            data.push(b'\n');
        }
        let path = self.root.join(STATE_DIR).join(HANDLED_FILE);
        if let Err(e) = write_atomic(&path, &data) {
            warn!("saving {}: {e}", path.display());
        }
    }

    // ---- linked worktrees ----------------------------------------------------

    /// Makes a worktree's `.git` file relative and writes this machine's
    /// back pointer for it.
    fn link_worktree(&self, file: &Path, data: &[u8]) -> bool {
        let wt = self.root.join(file.parent().unwrap_or(Path::new("")));
        let raw = data.strip_prefix(b"gitdir: ").unwrap_or(data);
        let target = Path::new(OsStr::from_bytes(raw.trim_ascii_end()));
        let mut changed = false;
        let admin = if target.is_absolute() {
            let Ok(inside) = target.strip_prefix(&self.root) else {
                return false; // points outside the volume; nothing to do
            };
            let admin = self.root.join(inside);
            let Some(rel) = relative(&wt, &admin) else {
                return false;
            };
            let mut content = b"gitdir: ".to_vec();
            content.extend_from_slice(rel.as_os_str().as_bytes());
            content.push(b'\n');
            match write_atomic(&wt.join(".git"), &content) {
                Ok(()) => {
                    info!(worktree = %wt.display(), "made worktree link relative");
                    changed = true;
                }
                Err(e) => warn!("rewriting {}: {e}", wt.join(".git").display()),
            }
            admin
        } else {
            wt.join(target)
        };
        // Only linked worktrees have a back pointer (submodules do not).
        if admin.parent().and_then(Path::file_name) != Some(OsStr::new("worktrees")) || !admin.is_dir() {
            return changed;
        }
        let back = admin.join("gitdir");
        let mut want = wt.join(".git").into_os_string().into_vec();
        want.push(b'\n');
        if fs::read(&back).ok().as_deref() != Some(&want[..]) {
            match write_atomic(&back, &want) {
                Ok(()) => debug!(worktree = %wt.display(), "wrote worktree back pointer"),
                Err(e) => warn!("writing {}: {e}", back.display()),
            }
        }
        changed
    }

    /// Sets the stat options that keep a synced index valid, once per
    /// repository and only if the user has not chosen otherwise.
    fn configure(&mut self, g: &Path) -> bool {
        if !self.configured.insert(g.to_owned()) {
            return false;
        }
        let mut changed = false;
        for (key, value) in [("core.checkStat", "minimal"), ("core.trustctime", "false")] {
            let got = git_in(At::GitDir(g), &["config", "--local", "--get", key]);
            if matches!(&got, Ok(o) if o.status.code() == Some(1)) {
                match git_ok(git_in(At::GitDir(g), &["config", "--local", key, value])) {
                    Ok(_) => changed = true,
                    Err(e) => warn!(gitdir = %g.display(), "setting {key}: {e}"),
                }
            }
        }
        if changed {
            info!(gitdir = %g.display(), "set core.checkStat=minimal and core.trustctime=false so the index stays valid across machines");
        }
        changed
    }

    // ---- conflict copies -----------------------------------------------------

    fn tidy_copy(&self, g: &Path, inner: &Path, resolved: &mut Vec<Resolved>) -> Result<Outcome, String> {
        let name = inner.file_name().ok_or("no name")?;
        let orig_name = original_name(name.as_bytes()).ok_or("not a conflict copy")?;
        let orig = inner.with_file_name(OsStr::from_bytes(&orig_name));
        let copy_path = g.join(inner);
        let orig_path = g.join(&orig);
        let first = inner.components().next().map(|c| c.as_os_str().to_owned());
        let first = first.as_deref().and_then(OsStr::to_str).unwrap_or("");
        let orig_str = orig.to_str().unwrap_or("");
        let in_worktree_admin = first == "worktrees";

        let base_name = orig.file_name().and_then(OsStr::to_str).unwrap_or("");
        if base_name == "index"
            && (orig.components().count() == 1 || (in_worktree_admin && orig.components().count() == 3))
        {
            return self.merge_index(g, &orig_path, &copy_path);
        }
        let disposable = first == "objects"
            || DISPOSABLE.contains(&orig_str)
            || (in_worktree_admin
                && orig
                    .file_name()
                    .and_then(OsStr::to_str)
                    .is_some_and(|n| DISPOSABLE.contains(&n)));
        if disposable {
            remove(&copy_path)?;
            return Ok(Outcome::Changed);
        }
        if first == "logs"
            || (in_worktree_admin && orig.components().nth(2).is_some_and(|c| c.as_os_str() == "logs"))
        {
            return self.merge_reflog(&orig_path, &copy_path);
        }
        if orig_str == "packed-refs" {
            return self.merge_packed_refs(g, &copy_path, name.as_bytes());
        }
        let is_head = orig.file_name() == Some(OsStr::new("HEAD"))
            && (orig.components().count() == 1 || (in_worktree_admin && orig.components().count() == 3));
        if first == "refs" || is_head {
            return self.merge_ref(g, &orig, &copy_path, is_head, resolved);
        }
        Ok(Outcome::Kept)
    }

    /// A loose ref (or a HEAD) and its conflict copy.
    fn merge_ref(
        &self,
        g: &Path,
        orig: &Path,
        copy_path: &Path,
        is_head: bool,
        resolved: &mut Vec<Resolved>,
    ) -> Result<Outcome, String> {
        let orig_path = g.join(orig);
        let Ok(w) = fs::read(&orig_path) else {
            // The ref itself is gone; the copy lives on as a branch.
            return Ok(Outcome::Kept);
        };
        let l = fs::read(copy_path).map_err(|e| e.to_string())?;
        let (w, l) = (parse_ref(&w), parse_ref(&l));
        if w == l {
            remove(copy_path)?;
            return Ok(Outcome::Changed);
        }
        // Commands for a linked worktree's HEAD run in its admin dir.
        let admin = if is_head {
            orig_path.parent().unwrap().to_owned()
        } else {
            g.to_owned()
        };
        let refname = (!is_head).then(|| path_to_ref(orig));
        let resolve = |v: &RefValue| -> Result<String, String> {
            let spec = match v {
                RefValue::Oid(o) => o.clone(),
                RefValue::Symbolic(s) => s.clone(),
            };
            rev_parse(At::GitDir(&admin), &format!("{spec}^{{commit}}"))
                .ok_or_else(|| format!("cannot resolve {spec}"))
        };
        let (wc, lc) = (resolve(&w)?, resolve(&l)?);
        let done = |target: String, other: String, resolved: &mut Vec<Resolved>| {
            if target != other {
                resolved.push(Resolved {
                    gitdir: g.to_owned(),
                    refname: refname.clone(),
                    admin: admin.clone(),
                    target,
                    other,
                });
            }
        };
        match (&w, &l) {
            (RefValue::Oid(_), RefValue::Oid(_)) => {
                if is_ancestor(At::GitDir(&admin), &lc, &wc)? {
                    remove(copy_path)?;
                    done(wc, lc, resolved);
                    Ok(Outcome::Changed)
                } else if is_ancestor(At::GitDir(&admin), &wc, &lc)? {
                    let name = refname.clone().unwrap_or_else(|| "HEAD".into());
                    let mut args = vec!["update-ref", "-m", "rustfuse: fast-forward after a sync conflict"];
                    if is_head {
                        args.push("--no-deref");
                    }
                    args.extend([name.as_str(), lc.as_str(), wc.as_str()]);
                    git_ok(git_in(At::GitDir(&admin), &args))?;
                    remove(copy_path)?;
                    info!(gitdir = %g.display(), r#ref = name, "fast-forwarded after a sync conflict");
                    done(lc, wc, resolved);
                    Ok(Outcome::Changed)
                } else {
                    done(wc, lc, resolved);
                    Ok(Outcome::Diverged)
                }
            }
            // HEAD moved to different branches: the later checkout stands.
            _ => {
                remove(copy_path)?;
                done(wc, lc, resolved);
                Ok(Outcome::Changed)
            }
        }
    }

    fn merge_packed_refs(&self, g: &Path, copy_path: &Path, copy_name: &[u8]) -> Result<Outcome, String> {
        let orig_path = g.join("packed-refs");
        let w = fs::read(&orig_path).map_err(|e| e.to_string())?;
        let l = fs::read(copy_path).map_err(|e| e.to_string())?;
        let (wh, mut wrefs) = parse_packed(&w);
        let (lh, lrefs) = parse_packed(&l);
        let tag = &copy_name[b"packed-refs".len()..];
        let mut diverged = Vec::new();
        for (name, lv) in lrefs {
            match wrefs.get(&name) {
                None => {
                    wrefs.insert(name, lv);
                }
                Some(wv) if wv.oid == lv.oid => {}
                Some(wv) => {
                    if is_ancestor(At::GitDir(g), &lv.oid, &wv.oid)? {
                    } else if is_ancestor(At::GitDir(g), &wv.oid, &lv.oid)? {
                        wrefs.insert(name, lv);
                    } else {
                        diverged.push((name, lv.oid));
                    }
                }
            }
        }
        let mut out = Vec::new();
        if let Some(h) = wh.or(lh) {
            out.extend_from_slice(&h);
            out.push(b'\n');
        }
        for (name, v) in &wrefs {
            out.extend_from_slice(format!("{} {}\n", v.oid, name).as_bytes());
            if let Some(p) = &v.peeled {
                out.extend_from_slice(format!("^{p}\n").as_bytes());
            }
        }
        // Same lock protocol as git.
        let lock = g.join("packed-refs.lock");
        let mut f = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&lock)
            .map_err(|e| format!("packed-refs is locked: {e}"))?;
        let res = (|| {
            f.write_all(&out)?;
            f.sync_all()?;
            if fs::read(&orig_path)? != w {
                return Err(std::io::Error::other("packed-refs changed meanwhile"));
            }
            fs::rename(&lock, &orig_path)
        })();
        if let Err(e) = res {
            let _ = fs::remove_file(&lock);
            return Err(e.to_string());
        }
        for (name, oid) in diverged {
            let mut path = g.join(&name).into_os_string().into_vec();
            path.extend_from_slice(tag);
            let path = PathBuf::from(OsString::from_vec(path));
            if !path.exists() {
                if let Some(dir) = path.parent() {
                    fs::create_dir_all(dir).map_err(|e| e.to_string())?;
                }
                write_atomic(&path, format!("{oid}\n").as_bytes()).map_err(|e| e.to_string())?;
            }
        }
        remove(copy_path)?;
        info!(gitdir = %g.display(), "merged packed-refs from two machines");
        Ok(Outcome::Changed)
    }

    /// Two machines rewrote the index, often just refreshing stat data
    /// (`git status`). Keep the one that agrees with HEAD most, so a stale
    /// refresh never undoes a commit, checkout or merge made elsewhere.
    fn merge_index(&self, g: &Path, orig_path: &Path, copy_path: &Path) -> Result<Outcome, String> {
        let admin = orig_path.parent().unwrap_or(g);
        let wt = if admin == g {
            g.parent().map(Path::to_owned)
        } else {
            fs::read(admin.join("gitdir")).ok().and_then(|b| {
                Path::new(OsStr::from_bytes(b.trim_ascii_end()))
                    .parent()
                    .map(Path::to_owned)
            })
        };
        if let Some(wt) = wt.filter(|w| w.is_dir())
            && let Ok(head) = git_ok(git_in(
                At::Worktree(&wt),
                &["ls-tree", "-r", "-z", "--full-tree", "HEAD"],
            ))
        {
            let head: HashSet<Vec<u8>> = head
                .stdout
                .split(|&b| b == 0)
                .filter(|l| !l.is_empty())
                .map(|l| {
                    // "mode type oid\tpath" -> "mode oid\tpath"
                    let mut out = Vec::new();
                    let mut it = l.splitn(3, |&b| b == b' ');
                    out.extend_from_slice(it.next().unwrap_or_default());
                    it.next();
                    out.push(b' ');
                    out.extend_from_slice(it.next().unwrap_or_default());
                    out
                })
                .collect();
            let distance = |index: &Path| -> usize {
                let out = Command::new("git")
                    .args(["ls-files", "-s", "-z"])
                    .current_dir(&wt)
                    .env("GIT_INDEX_FILE", index)
                    .env_remove("GIT_DIR")
                    .env_remove("GIT_WORK_TREE")
                    .stdin(Stdio::null())
                    .output();
                let Ok(out) = out.map_err(|e| e.to_string()).and_then(|o| {
                    if o.status.success() {
                        Ok(o)
                    } else {
                        Err(String::new())
                    }
                }) else {
                    return usize::MAX;
                };
                let entries: HashSet<Vec<u8>> = out
                    .stdout
                    .split(|&b| b == 0)
                    .filter(|l| !l.is_empty())
                    .map(|l| {
                        // "mode oid stage\tpath": unmerged entries never match.
                        let tab = l.iter().position(|&b| b == b'\t').unwrap_or(l.len());
                        let (meta, path) = l.split_at(tab);
                        let mut parts = meta.split(|&b| b == b' ');
                        let (mode, oid, stage) = (parts.next(), parts.next(), parts.next());
                        let mut out = Vec::new();
                        out.extend_from_slice(mode.unwrap_or_default());
                        out.push(b' ');
                        out.extend_from_slice(oid.unwrap_or_default());
                        if stage != Some(b"0") {
                            out.extend_from_slice(b" unmerged");
                        }
                        out.extend_from_slice(path);
                        out
                    })
                    .collect();
                entries.symmetric_difference(&head).count()
            };
            if distance(copy_path) < distance(orig_path) {
                fs::rename(copy_path, orig_path).map_err(|e| e.to_string())?;
                debug!(index = %orig_path.display(), "kept the other machine's index");
                return Ok(Outcome::Changed);
            }
        }
        remove(copy_path)?;
        Ok(Outcome::Changed)
    }

    /// Reflogs only grow: keep every entry of both, in time order.
    fn merge_reflog(&self, orig_path: &Path, copy_path: &Path) -> Result<Outcome, String> {
        let l = fs::read(copy_path).map_err(|e| e.to_string())?;
        let w = match fs::read(orig_path) {
            Ok(w) => w,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(e) => return Err(e.to_string()),
        };
        let mut lines: Vec<(i64, &[u8])> = w
            .split(|&b| b == b'\n')
            .chain(l.split(|&b| b == b'\n'))
            .filter(|l| !l.is_empty())
            .map(|l| (reflog_time(l), l))
            .collect();
        lines.sort();
        lines.dedup();
        let mut out = Vec::new();
        for (_, l) in lines {
            out.extend_from_slice(l);
            out.push(b'\n');
        }
        if out != w {
            write_atomic(orig_path, &out).map_err(|e| e.to_string())?;
        }
        remove(copy_path)?;
        Ok(Outcome::Changed)
    }

    // ---- worktrees after a ref conflict --------------------------------------

    /// Worktrees (directory and admin dir) that have the resolved ref
    /// checked out at its new value.
    fn worktrees_of(&self, r: &Resolved) -> Vec<PathBuf> {
        let mut admins = vec![r.gitdir.clone()];
        if let Ok(rd) = fs::read_dir(r.gitdir.join("worktrees")) {
            admins.extend(rd.flatten().map(|e| e.path()));
        }
        let mut out = Vec::new();
        for admin in admins {
            let on_ref = match &r.refname {
                None => admin == r.admin,
                Some(name) => fs::read(admin.join("HEAD"))
                    .is_ok_and(|h| parse_ref(&h) == RefValue::Symbolic(name.clone())),
            };
            // Never in the middle of the user's merge, rebase or the like.
            let busy = [
                "MERGE_HEAD",
                "CHERRY_PICK_HEAD",
                "REVERT_HEAD",
                "rebase-merge",
                "rebase-apply",
            ]
            .iter()
            .any(|f| admin.join(f).exists());
            if !on_ref || busy {
                continue;
            }
            let wt = if admin == r.gitdir {
                match r.gitdir.parent() {
                    Some(p) => p.to_owned(),
                    None => continue,
                }
            } else {
                match fs::read(admin.join("gitdir")) {
                    Ok(b) => match Path::new(OsStr::from_bytes(b.trim_ascii_end())).parent() {
                        Some(p) => p.to_owned(),
                        None => continue,
                    },
                    Err(_) => continue,
                }
            };
            // Only if the worktree really is at the winning commit now.
            if wt.starts_with(&self.root)
                && rev_parse(At::Worktree(&wt), "HEAD").as_deref() == Some(&r.target)
            {
                out.push(wt);
            }
        }
        out
    }

    /// Puts worktree files that hold `other`'s committed version back to
    /// `target`'s, and drops conflict copies that only repeat a committed
    /// version. Each step is one git command for all paths, so a large
    /// diverged commit costs a handful of processes, not several per file.
    fn clean_worktrees(&self, r: &Resolved) -> Result<bool, String> {
        let mut changed = false;
        for wt in self.worktrees_of(r) {
            let out = git_ok(git_in(
                At::Worktree(&wt),
                &[
                    "diff",
                    "--raw",
                    "-z",
                    "--no-renames",
                    "--no-abbrev",
                    &r.other,
                    &r.target,
                    "--",
                ],
            ))?;
            let diffs = parse_raw_diff(&out.stdout);
            if diffs.is_empty() {
                continue;
            }
            let wanted: HashSet<&[u8]> = diffs.iter().map(|d| d.path.as_slice()).collect();
            let index = index_blobs(&wt, &wanted)?;
            let paths: Vec<&[u8]> = diffs.iter().map(|d| d.path.as_slice()).collect();
            let work = worktree_blobs(&wt, &paths);
            let (mut checkout, mut unstage, mut reset) = (Vec::new(), Vec::new(), Vec::new());
            let mut copies = Vec::new();
            for (d, w) in diffs.iter().zip(&work) {
                let (x, t) = (&d.from, &d.to);
                if w == x && w != t {
                    if t.is_some() {
                        checkout.push(d.path.as_slice());
                    } else {
                        remove(&wt.join(OsStr::from_bytes(&d.path)))?;
                        unstage.push(d.path.as_slice());
                    }
                } else if x != t && index.get(d.path.as_slice()) == x.as_ref() {
                    reset.push(d.path.as_slice());
                }
                copies.push((d.path.as_slice(), [t.clone(), x.clone()]));
            }
            if !checkout.is_empty() {
                git_ok(git_paths(&wt, &["checkout", &r.target], &checkout))?;
            }
            if !unstage.is_empty() {
                git_ok(git_paths(
                    &wt,
                    &["rm", "--cached", "-q", "--ignore-unmatch"],
                    &unstage,
                ))?;
            }
            if !reset.is_empty() {
                git_ok(git_paths(&wt, &["reset", "-q", &r.target], &reset))?;
            }
            let restored = checkout.len() + unstage.len();
            changed |= restored + reset.len() > 0;
            changed |= drop_redundant_copies(&wt, &copies);
            if restored > 0 {
                info!(
                    worktree = %wt.display(),
                    files = restored,
                    "restored files to the winning commit after a sync conflict"
                );
            }
        }
        Ok(changed)
    }
}

/// One path of `git diff --raw`: its blob before and after (`None` where
/// the path does not exist on that side).
struct RawDiff {
    path: Vec<u8>,
    from: Option<String>,
    to: Option<String>,
}

fn parse_raw_diff(out: &[u8]) -> Vec<RawDiff> {
    let blob = |b: &[u8]| {
        let s = String::from_utf8_lossy(b).into_owned();
        (!s.bytes().all(|c| c == b'0')).then_some(s)
    };
    let mut diffs = Vec::new();
    let mut fields = out.split(|&b| b == 0);
    // Each entry is ":<mode> <mode> <blob> <blob> <status>" then the path.
    while let (Some(meta), Some(path)) = (fields.next(), fields.next()) {
        let parts: Vec<&[u8]> = meta.split(|&b| b == b' ').collect();
        if parts.len() < 5 || path.is_empty() {
            continue;
        }
        diffs.push(RawDiff {
            path: path.to_vec(),
            from: blob(parts[2]),
            to: blob(parts[3]),
        });
    }
    diffs
}

/// Blob ids in the index (stage 0, or the first stage listed) for the
/// wanted paths.
fn index_blobs(
    wt: &Path,
    wanted: &HashSet<&[u8]>,
) -> Result<std::collections::HashMap<Vec<u8>, String>, String> {
    let out = git_ok(git_in(At::Worktree(wt), &["ls-files", "-s", "-z"]))?;
    let mut map = std::collections::HashMap::new();
    for line in out.stdout.split(|&b| b == 0) {
        let Some(tab) = line.iter().position(|&b| b == b'\t') else {
            continue;
        };
        let (meta, path) = (&line[..tab], &line[tab + 1..]);
        if !wanted.contains(path) || map.contains_key(path) {
            continue;
        }
        if let Some(blob) = meta.split(|&b| b == b' ').nth(1) {
            map.insert(path.to_vec(), String::from_utf8_lossy(blob).into_owned());
        }
    }
    Ok(map)
}

/// The blob ids git would record for these worktree files, in order;
/// regular files are hashed by one `git hash-object` for all of them.
fn worktree_blobs(wt: &Path, paths: &[&[u8]]) -> Vec<Option<String>> {
    let mut out = vec![None; paths.len()];
    let mut batch = Vec::new();
    for (i, p) in paths.iter().enumerate() {
        let path = OsStr::from_bytes(p);
        match fs::symlink_metadata(wt.join(path)) {
            // One path per line: names with a newline go one at a time.
            Ok(m) if m.is_file() && !p.contains(&b'\n') => batch.push(i),
            Ok(_) => out[i] = worktree_blob(wt, path),
            Err(_) => {}
        }
    }
    if batch.is_empty() {
        return out;
    }
    let mut input = Vec::new();
    for &i in &batch {
        input.extend_from_slice(paths[i]);
        input.push(b'\n');
    }
    let hashed = git_stdin(At::Worktree(wt), &["hash-object", "--stdin-paths"], &input)
        .ok()
        .filter(|o| o.status.success())
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .map(str::to_owned)
                .collect::<Vec<_>>()
        });
    match hashed {
        Some(lines) if lines.len() == batch.len() => {
            for (&i, h) in batch.iter().zip(lines) {
                out[i] = Some(h);
            }
        }
        // A file vanished or could not be read: fall back to one at a time.
        _ => {
            for &i in &batch {
                out[i] = worktree_blob(wt, OsStr::from_bytes(paths[i]));
            }
        }
    }
    out
}

enum Outcome {
    Changed,
    Kept,
    Diverged,
}

#[derive(Debug, PartialEq, Eq)]
enum RefValue {
    Oid(String),
    Symbolic(String),
}

fn parse_ref(data: &[u8]) -> RefValue {
    let s = String::from_utf8_lossy(data).trim().to_owned();
    match s.strip_prefix("ref: ") {
        Some(r) => RefValue::Symbolic(r.trim().to_owned()),
        None => RefValue::Oid(s),
    }
}

fn path_to_ref(p: &Path) -> String {
    p.components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join("/")
}

struct Packed {
    oid: String,
    peeled: Option<String>,
}

/// The header line and the refs of a `packed-refs` file.
fn parse_packed(data: &[u8]) -> (Option<Vec<u8>>, BTreeMap<String, Packed>) {
    let mut header = None;
    let mut refs: BTreeMap<String, Packed> = BTreeMap::new();
    let mut last: Option<String> = None;
    for line in data.split(|&b| b == b'\n').filter(|l| !l.is_empty()) {
        if line.starts_with(b"#") {
            header = Some(line.to_vec());
        } else if let Some(p) = line.strip_prefix(b"^") {
            if let Some(name) = &last
                && let Some(r) = refs.get_mut(name)
            {
                r.peeled = Some(String::from_utf8_lossy(p).into_owned());
            }
        } else if let Some(sp) = line.iter().position(|&b| b == b' ') {
            let name = String::from_utf8_lossy(&line[sp + 1..]).into_owned();
            refs.insert(
                name.clone(),
                Packed {
                    oid: String::from_utf8_lossy(&line[..sp]).into_owned(),
                    peeled: None,
                },
            );
            last = Some(name);
        }
    }
    (header, refs)
}

/// Seconds since the epoch of a reflog entry
/// (`old new Name <email> 1700000000 +0000\tmessage`).
fn reflog_time(line: &[u8]) -> i64 {
    let head = line.split(|&b| b == b'\t').next().unwrap_or(line);
    let head = String::from_utf8_lossy(head);
    let mut it = head.rsplit(' ');
    it.next();
    it.next().and_then(|t| t.parse().ok()).unwrap_or(0)
}

/// `to` relative to the directory `from`.
fn relative(from: &Path, to: &Path) -> Option<PathBuf> {
    let a: Vec<Component> = from.components().collect();
    let b: Vec<Component> = to.components().collect();
    let common = a.iter().zip(&b).take_while(|(x, y)| x == y).count();
    if common == 0 {
        return None;
    }
    let mut out = PathBuf::new();
    for _ in common..a.len() {
        out.push("..");
    }
    for c in &b[common..] {
        out.push(c);
    }
    Some(out)
}

fn write_atomic(path: &Path, data: &[u8]) -> std::io::Result<()> {
    let dir = path.parent().unwrap_or(Path::new("."));
    let tmp = dir.join(format!(".rustfuse-tmp-git-{}", std::process::id()));
    fs::write(&tmp, data)?;
    fs::rename(&tmp, path)
}

fn remove(path: &Path) -> Result<(), String> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!("{}: {e}", path.display())),
    }
}

// ---- running git ---------------------------------------------------------------

/// Where a git command runs: on a git dir (or a worktree's admin dir), or
/// in a worktree.
#[derive(Clone, Copy)]
enum At<'a> {
    GitDir(&'a Path),
    Worktree(&'a Path),
}

fn git_command<I, S>(at: At, args: I) -> Command
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let mut cmd = Command::new("git");
    match at {
        At::GitDir(g) => {
            let mut flag = OsString::from("--git-dir=");
            flag.push(g);
            cmd.arg(flag).current_dir(g);
        }
        At::Worktree(w) => {
            cmd.current_dir(w);
        }
    }
    cmd.args(args)
        // Paths are file names, never patterns.
        .env("GIT_LITERAL_PATHSPECS", "1")
        .env("LC_ALL", "C")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE");
    cmd
}

fn git_args<I, S>(at: At, args: I) -> std::io::Result<Output>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    git_command(at, args).stdin(Stdio::null()).output()
}

fn git_in(at: At, args: &[&str]) -> std::io::Result<Output> {
    git_args(at, args)
}

/// Runs git with `input` on stdin.
fn git_stdin(at: At, args: &[&str], input: &[u8]) -> std::io::Result<Output> {
    let mut cmd = git_command(at, args);
    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let mut stdin = child.stdin.take().expect("piped");
    let input = input.to_vec();
    // Written from another thread so a full stdout pipe cannot deadlock.
    let writer = std::thread::spawn(move || stdin.write_all(&input));
    let out = child.wait_with_output()?;
    writer.join().expect("stdin writer panicked")?;
    Ok(out)
}

/// Runs a git command on many paths at once, passed NUL-separated on stdin.
fn git_paths(wt: &Path, args: &[&str], paths: &[&[u8]]) -> std::io::Result<Output> {
    let mut args = args.to_vec();
    args.extend(["--pathspec-from-file=-", "--pathspec-file-nul"]);
    let mut input = Vec::new();
    for p in paths {
        input.extend_from_slice(p);
        input.push(0);
    }
    git_stdin(At::Worktree(wt), &args, &input)
}

fn git_ok(out: std::io::Result<Output>) -> Result<Output, String> {
    let out = out.map_err(|e| e.to_string())?;
    if out.status.success() {
        Ok(out)
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_owned())
    }
}

fn rev_parse(at: At, spec: &str) -> Option<String> {
    let out = git_ok(git_in(at, &["rev-parse", "-q", "--verify", spec])).ok()?;
    Some(String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

/// Whether `a` is an ancestor of (or equal to) `b`; an error if git cannot
/// tell yet (objects still missing).
fn is_ancestor(at: At, a: &str, b: &str) -> Result<bool, String> {
    let out = git_in(at, &["merge-base", "--is-ancestor", a, b]).map_err(|e| e.to_string())?;
    match out.status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => Err(String::from_utf8_lossy(&out.stderr).trim().to_owned()),
    }
}

/// The blob id git would record for a worktree file.
fn worktree_blob(wt: &Path, path: &OsStr) -> Option<String> {
    let full = wt.join(path);
    let meta = fs::symlink_metadata(&full).ok()?;
    let out = if meta.file_type().is_symlink() {
        let target = fs::read_link(&full).ok()?;
        let mut child = Command::new("git")
            .args(["hash-object", "--stdin"])
            .current_dir(wt)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .ok()?;
        child
            .stdin
            .take()?
            .write_all(target.as_os_str().as_bytes())
            .ok()?;
        child.wait_with_output().ok()?
    } else if meta.is_file() {
        git_args(
            At::Worktree(wt),
            [OsStr::new("hash-object"), OsStr::new("--"), path],
        )
        .ok()?
    } else {
        return Some("(directory)".into());
    };
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

/// A path's blob in the winning and the other commit.
type Versions = [Option<String>; 2];

/// Removes conflict copies next to each path whose content is one of that
/// path's committed versions. Reads each directory once.
fn drop_redundant_copies(wt: &Path, items: &[(&[u8], Versions)]) -> bool {
    let mut by_dir: BTreeMap<&Path, Vec<(&OsStr, &Versions)>> = BTreeMap::new();
    for (p, versions) in items {
        let path = Path::new(OsStr::from_bytes(p));
        if let (Some(dir), Some(name)) = (path.parent(), path.file_name()) {
            by_dir.entry(dir).or_default().push((name, versions));
        }
    }
    let mut candidates: Vec<(Vec<u8>, &Versions)> = Vec::new();
    for (dir, names) in by_dir {
        let Ok(rd) = fs::read_dir(wt.join(dir)) else {
            continue;
        };
        for e in rd.flatten() {
            let n = e.file_name();
            let Some(orig) = original_name(n.as_bytes()) else {
                continue;
            };
            if let Some((_, versions)) = names.iter().find(|(name, _)| name.as_bytes() == orig.as_slice()) {
                candidates.push((dir.join(&n).into_os_string().into_vec(), versions));
            }
        }
    }
    let paths: Vec<&[u8]> = candidates.iter().map(|(p, _)| p.as_slice()).collect();
    let blobs = worktree_blobs(wt, &paths);
    let mut removed = 0usize;
    for ((rel, versions), blob) in candidates.iter().zip(blobs) {
        if blob.is_some()
            && versions.contains(&blob)
            && fs::remove_file(wt.join(OsStr::from_bytes(rel))).is_ok()
        {
            debug!(copy = %wt.join(OsStr::from_bytes(rel)).display(), "removed conflict copy");
            removed += 1;
        }
    }
    if removed > 0 {
        info!(
            worktree = %wt.display(),
            copies = removed,
            "removed conflict copies already kept in git history"
        );
    }
    removed > 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tree::conflict_name;
    use rustfuse_types::{Hlc, ReplicaId, Version};

    #[test]
    fn original_names() {
        let v = Version {
            time: Hlc(1_760_000_000_000_000_000),
            replica: ReplicaId(0xabcdef12),
        };
        for name in ["main", "a.txt", ".gitignore", "v1.0", "packed-refs", "x.tar.gz"] {
            let c = conflict_name(name.as_bytes(), v);
            assert_eq!(original_name(&c).as_deref(), Some(name.as_bytes()), "{name}");
        }
        assert_eq!(original_name(b"main"), None);
        assert_eq!(original_name(b"a.conflict-zz.txt"), None);
    }

    #[test]
    fn reflog_union_is_ordered() {
        let a = b"0 1 T <t@e> 100 +0000\tcommit: a\n0 2 T <t@e> 300 +0000\tcommit: c\n";
        assert_eq!(
            reflog_time(&a[..a.iter().position(|&b| b == b'\n').unwrap()]),
            100
        );
    }

    #[test]
    fn relative_paths() {
        assert_eq!(
            relative(Path::new("/v/repo/wt"), Path::new("/v/repo/.git/worktrees/wt")),
            Some(PathBuf::from("../.git/worktrees/wt"))
        );
        assert_eq!(
            relative(Path::new("/v/wt"), Path::new("/v/repo/.git/worktrees/wt")),
            Some(PathBuf::from("../repo/.git/worktrees/wt"))
        );
    }

    #[test]
    fn packed_refs_parse() {
        let (h, refs) = parse_packed(
            b"# pack-refs with: peeled fully-peeled sorted \naaa refs/heads/main\nbbb refs/tags/v1\n^ccc\n",
        );
        assert!(h.is_some());
        assert_eq!(refs["refs/heads/main"].oid, "aaa");
        assert_eq!(refs["refs/tags/v1"].peeled.as_deref(), Some("ccc"));
    }
}
