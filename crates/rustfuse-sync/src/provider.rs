//! The daemon side of the macOS File Provider (`docs/design.md` §2.2): a
//! small request/response protocol the File Provider extension uses to see
//! and change the synced directory, served over a Unix socket.
//!
//! The extension is a view over the replica, like the mounts: every change
//! goes to the directory and is captured as usual, so conflict rules, the
//! git pass and offline work are unchanged. Requests that read or change
//! the tree run inside the sync loop ([`Replica::run_serving`]); fetching
//! content runs beside it through the [`Hydrator`].
//!
//! Items are named by the node id the volume gives them, which survives
//! renames, edits and restarts. Entries the volume does not hold (ignored,
//! machine-local paths such as `target/`) are named by their path instead.
//!
//! Wire format: one JSON request per line, answered by one JSON line.

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rustfuse_types::{NodeId, Versions};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, warn};

use crate::lazy::Hydrator;
use crate::tree::Tree;
use crate::{Replica, STATE_DIR};

/// The root directory's item id.
pub const ROOT_ID: &str = "root";

/// Most items one listing returns.
const PAGE: usize = 500;

/// A file, directory or symlink as the extension sees it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Item {
    pub id: String,
    pub parent: String,
    pub name: String,
    pub kind: Kind,
    pub size: u64,
    /// Permission bits.
    pub mode: u32,
    pub mtime_ns: i64,
    /// Symlink target.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    /// Changes whenever the content does.
    pub content_version: String,
    /// Changes whenever the name, place or permissions do.
    pub meta_version: String,
    /// Whether the content is on this machine (not a placeholder).
    pub downloaded: bool,
    /// Whether the volume holds it; machine-local entries are not synced.
    pub synced: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    File,
    Dir,
    Symlink,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Request {
    /// One item.
    Item { id: String },
    /// A directory's children, by name, after `after`.
    List {
        id: String,
        #[serde(default)]
        after: Option<String>,
    },
    /// The current position of the change journal.
    Anchor,
    /// What changed since `anchor`.
    Changes { anchor: String },
    /// The item's content on disk, fetched first if it is a placeholder.
    Fetch { id: String },
    /// A new item in `parent`. Files take their content from `source`
    /// (moved into place), symlinks their target from `target`.
    Create {
        parent: String,
        name: String,
        kind: Kind,
        #[serde(default)]
        source: Option<PathBuf>,
        #[serde(default)]
        target: Option<String>,
        #[serde(default)]
        mode: Option<u32>,
        #[serde(default)]
        mtime_ns: Option<i64>,
    },
    /// Changes an item: moves it, replaces its content with `source`, or
    /// sets its permissions or time. Unset fields stay.
    Modify {
        id: String,
        #[serde(default)]
        parent: Option<String>,
        #[serde(default)]
        name: Option<String>,
        #[serde(default)]
        source: Option<PathBuf>,
        #[serde(default)]
        mode: Option<u32>,
        #[serde(default)]
        mtime_ns: Option<i64>,
    },
    /// Deletes an item; a directory only if empty unless `recursive`.
    Delete {
        id: String,
        #[serde(default)]
        recursive: bool,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum Response {
    Item {
        item: Item,
    },
    List {
        items: Vec<Item>,
        /// Pass as `after` for the next page; absent on the last.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        next: Option<String>,
    },
    Anchor {
        anchor: String,
    },
    Changes {
        updated: Vec<Item>,
        deleted: Vec<String>,
        anchor: String,
    },
    Path {
        path: PathBuf,
    },
    Done,
    Error {
        code: ErrorCode,
        message: String,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    NoSuchItem,
    Exists,
    NotEmpty,
    /// The anchor is from an earlier run: enumerate everything again.
    AnchorExpired,
    BadRequest,
    Io,
}

fn error(code: ErrorCode, message: impl Into<String>) -> Response {
    Response::Error {
        code,
        message: message.into(),
    }
}

fn io_error(e: std::io::Error) -> Response {
    let code = match e.kind() {
        std::io::ErrorKind::NotFound => ErrorCode::NoSuchItem,
        std::io::ErrorKind::AlreadyExists => ErrorCode::Exists,
        std::io::ErrorKind::DirectoryNotEmpty => ErrorCode::NotEmpty,
        _ => ErrorCode::Io,
    };
    error(code, e.to_string())
}

/// Nodes in the order they changed on disk during this run.
pub(crate) struct ChangeLog {
    /// Random per run: anchors from an earlier run have expired.
    epoch: u64,
    seq: u64,
    by_seq: BTreeMap<u64, NodeId>,
    by_node: HashMap<NodeId, u64>,
}

impl ChangeLog {
    pub(crate) fn new() -> ChangeLog {
        let mut b = [0u8; 8];
        let _ = getrandom::fill(&mut b);
        ChangeLog {
            epoch: u64::from_le_bytes(b),
            seq: 0,
            by_seq: BTreeMap::new(),
            by_node: HashMap::new(),
        }
    }

    pub(crate) fn record(&mut self, ids: impl IntoIterator<Item = NodeId>) {
        for id in ids {
            self.seq += 1;
            if let Some(old) = self.by_node.insert(id, self.seq) {
                self.by_seq.remove(&old);
            }
            self.by_seq.insert(self.seq, id);
        }
    }

    fn anchor(&self) -> String {
        format!("{:016x}-{}", self.epoch, self.seq)
    }

    /// Nodes changed after `anchor`, or `None` if it is not from this run.
    fn since(&self, anchor: &str) -> Option<Vec<NodeId>> {
        let (epoch, seq) = anchor.split_once('-')?;
        if u64::from_str_radix(epoch, 16).ok()? != self.epoch {
            return None;
        }
        let seq: u64 = seq.parse().ok()?;
        (seq <= self.seq).then(|| self.by_seq.range(seq + 1..).map(|(_, id)| *id).collect())
    }
}

/// Where an id points.
enum Target {
    Root,
    Node(NodeId),
    /// A machine-local entry, by path.
    Local(PathBuf),
}

fn parse_id(id: &str) -> Option<Target> {
    if id == ROOT_ID {
        return Some(Target::Root);
    }
    if let Some(hex) = id.strip_prefix('n') {
        return u128::from_str_radix(hex, 16)
            .ok()
            .map(|n| Target::Node(NodeId(n)));
    }
    let hex = id.strip_prefix('p')?;
    if hex.len() % 2 != 0 {
        return None;
    }
    let bytes: Option<Vec<u8>> = (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).ok())
        .collect();
    Some(Target::Local(PathBuf::from(std::ffi::OsStr::from_bytes(&bytes?))))
}

fn version_str(v: rustfuse_types::Version) -> String {
    format!("{:x}.{:x}", v.time.0, v.replica.0)
}

/// The id of whatever is at `rel` (relative to the root).
fn id_of(tree: &Tree, rel: &Path) -> String {
    if rel.as_os_str().is_empty() {
        return ROOT_ID.into();
    }
    match tree.resolve(rel) {
        Some(n) if n != NodeId::ROOT => format!("n{:032x}", n.0),
        _ => {
            let hex: String = rel
                .as_os_str()
                .as_bytes()
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect();
            format!("p{hex}")
        }
    }
}

fn mtime_ns(m: &fs::Metadata) -> i64 {
    m.mtime()
        .saturating_mul(1_000_000_000)
        .saturating_add(m.mtime_nsec())
}

fn time_of(ns: i64) -> SystemTime {
    if ns >= 0 {
        UNIX_EPOCH + Duration::from_nanos(ns as u64)
    } else {
        UNIX_EPOCH - Duration::from_nanos(ns.unsigned_abs())
    }
}

impl Replica {
    /// Path (relative to the root) an id points at, if it exists.
    fn rel_of(&self, id: &str) -> Option<PathBuf> {
        match parse_id(id)? {
            Target::Root => Some(PathBuf::new()),
            Target::Node(n) => self.tree().path(n),
            // Plain names only: never outside the directory or into its state.
            Target::Local(p) => {
                let plain = p
                    .components()
                    .all(|c| matches!(c, std::path::Component::Normal(_)));
                (plain && !p.as_os_str().is_empty() && !p.starts_with(STATE_DIR)).then_some(p)
            }
        }
    }

    /// The item at `rel` as it is on disk.
    fn item_at(&self, rel: &Path) -> std::io::Result<Item> {
        let full = self.root_dir().join(rel);
        let m = fs::symlink_metadata(&full)?;
        let tree = self.tree();
        let node = tree.resolve(rel).filter(|&n| n != NodeId::ROOT);
        let t = m.file_type();
        let (kind, target) = if t.is_dir() {
            (Kind::Dir, None)
        } else if t.is_symlink() {
            let to = fs::read_link(&full)?;
            (Kind::Symlink, Some(to.to_string_lossy().into_owned()))
        } else {
            (Kind::File, None)
        };
        let versions: Option<Versions> = node.and_then(|n| tree.node(n)).map(|n| n.versions);
        let size = if kind == Kind::Dir { 0 } else { m.len() };
        let downloaded = kind != Kind::File || !crate::lazy::is_placeholder(&full);
        let name = rel
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let parent = rel
            .parent()
            .map(|p| id_of(tree, p))
            .unwrap_or_else(|| ROOT_ID.into());
        let data = versions.map(|v| version_str(v.data)).unwrap_or_default();
        let (loc, meta) = versions
            .map(|v| (version_str(v.loc), version_str(v.meta)))
            .unwrap_or_default();
        Ok(Item {
            id: id_of(tree, rel),
            parent: parent.clone(),
            name: name.clone(),
            kind,
            size,
            mode: m.mode() & 0o7777,
            mtime_ns: mtime_ns(&m),
            target,
            content_version: format!("{data}/{size}/{}", mtime_ns(&m)),
            meta_version: format!("{loc}/{meta}/{:o}/{parent}/{name}", m.mode() & 0o7777),
            downloaded,
            synced: node.is_some(),
        })
    }

    /// Answers one request. `Fetch` only resolves the path here; the
    /// caller fetches content outside the sync loop.
    pub async fn provide(&mut self, req: Request) -> Response {
        match req {
            Request::Item { id } => match self.rel_of(&id) {
                Some(rel) => self
                    .item_at(&rel)
                    .map_or_else(io_error, |item| Response::Item { item }),
                None => error(ErrorCode::NoSuchItem, id),
            },
            Request::List { id, after } => self.list(&id, after),
            Request::Anchor => Response::Anchor {
                anchor: self.change_log().anchor(),
            },
            Request::Changes { anchor } => self.changes(&anchor),
            Request::Fetch { id } => match self.rel_of(&id) {
                Some(rel) => Response::Path {
                    path: self.root_dir().join(rel),
                },
                None => error(ErrorCode::NoSuchItem, id),
            },
            Request::Create {
                parent,
                name,
                kind,
                source,
                target,
                mode,
                mtime_ns,
            } => {
                self.create(&parent, &name, kind, source, target, mode, mtime_ns)
                    .await
            }
            Request::Modify {
                id,
                parent,
                name,
                source,
                mode,
                mtime_ns,
            } => self.modify(&id, parent, name, source, mode, mtime_ns).await,
            Request::Delete { id, recursive } => self.delete(&id, recursive).await,
        }
    }

    fn list(&self, id: &str, after: Option<String>) -> Response {
        let Some(rel) = self.rel_of(id) else {
            return error(ErrorCode::NoSuchItem, id);
        };
        let dir = self.root_dir().join(&rel);
        let rd = match fs::read_dir(&dir) {
            Ok(rd) => rd,
            Err(e) => return io_error(e),
        };
        let mut names: Vec<_> = rd
            .flatten()
            .map(|e| e.file_name())
            .filter(|n| !(rel.as_os_str().is_empty() && n == STATE_DIR))
            .filter(|n| match &after {
                Some(a) => n.as_bytes() > a.as_bytes(),
                None => true,
            })
            .collect();
        names.sort();
        let more = names.len() > PAGE;
        names.truncate(PAGE);
        let items: Vec<Item> = names
            .iter()
            .filter_map(|n| self.item_at(&rel.join(n)).ok())
            .collect();
        let next = more
            .then(|| names.last().map(|n| n.to_string_lossy().into_owned()))
            .flatten();
        Response::List { items, next }
    }

    fn changes(&self, anchor: &str) -> Response {
        let log = self.change_log();
        let Some(ids) = log.since(anchor) else {
            return error(ErrorCode::AnchorExpired, "enumerate again");
        };
        let mut updated = Vec::new();
        let mut deleted = Vec::new();
        for id in ids {
            let item = self.tree().path(id).and_then(|rel| self.item_at(&rel).ok());
            match item {
                Some(item) => updated.push(item),
                None => deleted.push(format!("n{:032x}", id.0)),
            }
        }
        Response::Changes {
            updated,
            deleted,
            anchor: log.anchor(),
        }
    }

    /// Captures the given paths, then answers with the item at `rel`.
    async fn settle(&mut self, touched: Vec<PathBuf>, rel: Option<&Path>) -> Response {
        if let Err(e) = self.capture(Some(touched)).await {
            warn!("capturing a File Provider change: {e}");
        }
        match rel {
            Some(rel) => self
                .item_at(rel)
                .map_or_else(io_error, |item| Response::Item { item }),
            None => Response::Done,
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn create(
        &mut self,
        parent: &str,
        name: &str,
        kind: Kind,
        source: Option<PathBuf>,
        target: Option<String>,
        mode: Option<u32>,
        mtime_ns: Option<i64>,
    ) -> Response {
        let Some(dir) = self.rel_of(parent) else {
            return error(ErrorCode::NoSuchItem, parent);
        };
        if !valid_name(name) {
            return error(ErrorCode::BadRequest, format!("bad name {name:?}"));
        }
        if dir.as_os_str().is_empty() && name == STATE_DIR {
            return error(ErrorCode::BadRequest, "reserved name");
        }
        let rel = dir.join(name);
        let full = self.root_dir().join(&rel);
        if fs::symlink_metadata(&full).is_ok() {
            return error(ErrorCode::Exists, rel.display().to_string());
        }
        let made = match kind {
            Kind::Dir => fs::create_dir(&full),
            Kind::Symlink => match target {
                Some(t) => std::os::unix::fs::symlink(t, &full),
                None => return error(ErrorCode::BadRequest, "a symlink needs a target"),
            },
            Kind::File => match source {
                Some(src) => place(&src, &full),
                None => fs::File::create_new(&full).map(drop),
            },
        };
        if let Err(e) = made.and_then(|()| set_attrs(&full, kind, mode, mtime_ns)) {
            return io_error(e);
        }
        debug!(path = %rel.display(), "created through the File Provider");
        self.settle(vec![rel.clone()], Some(&rel)).await
    }

    async fn modify(
        &mut self,
        id: &str,
        parent: Option<String>,
        name: Option<String>,
        source: Option<PathBuf>,
        mode: Option<u32>,
        mtime_ns: Option<i64>,
    ) -> Response {
        let Some(old) = self.rel_of(id) else {
            return error(ErrorCode::NoSuchItem, id);
        };
        if old.as_os_str().is_empty() {
            return error(ErrorCode::BadRequest, "the root cannot change");
        }
        let mut rel = old.clone();
        if parent.is_some() || name.is_some() {
            let dir = match &parent {
                Some(p) => match self.rel_of(p) {
                    Some(d) => d,
                    None => return error(ErrorCode::NoSuchItem, p.clone()),
                },
                None => old.parent().map(Path::to_path_buf).unwrap_or_default(),
            };
            let name =
                name.unwrap_or_else(|| old.file_name().unwrap_or_default().to_string_lossy().into_owned());
            if !valid_name(&name) || (dir.as_os_str().is_empty() && name == STATE_DIR) {
                return error(ErrorCode::BadRequest, format!("bad name {name:?}"));
            }
            rel = dir.join(name);
            if rel != old {
                let (from, to) = (self.root_dir().join(&old), self.root_dir().join(&rel));
                if fs::symlink_metadata(&to).is_ok() {
                    return error(ErrorCode::Exists, rel.display().to_string());
                }
                if let Err(e) = fs::rename(&from, &to) {
                    return io_error(e);
                }
                // Captured before new content replaces the file, so it
                // stays one node that moved rather than a delete and a
                // create.
                if source.is_some()
                    && let Err(e) = self.capture(Some(vec![old.clone(), rel.clone()])).await
                {
                    warn!("capturing a File Provider move: {e}");
                }
            }
        }
        let full = self.root_dir().join(&rel);
        let mut mode = mode;
        if let Some(src) = source {
            // New content keeps the item's permissions unless told otherwise.
            if mode.is_none() {
                mode = fs::metadata(&full).ok().map(|m| m.mode());
            }
            if let Err(e) = place(&src, &full) {
                return io_error(e);
            }
        }
        let kind = match fs::symlink_metadata(&full) {
            Ok(m) if m.is_dir() => Kind::Dir,
            Ok(m) if m.file_type().is_symlink() => Kind::Symlink,
            Ok(_) => Kind::File,
            Err(e) => return io_error(e),
        };
        if let Err(e) = set_attrs(&full, kind, mode, mtime_ns) {
            return io_error(e);
        }
        self.settle(vec![old, rel.clone()], Some(&rel)).await
    }

    async fn delete(&mut self, id: &str, recursive: bool) -> Response {
        let Some(rel) = self.rel_of(id) else {
            // Already gone: what the caller wanted.
            return Response::Done;
        };
        if rel.as_os_str().is_empty() {
            return error(ErrorCode::BadRequest, "the root cannot be deleted");
        }
        let full = self.root_dir().join(&rel);
        let res = match fs::symlink_metadata(&full) {
            Ok(m) if m.is_dir() && recursive => fs::remove_dir_all(&full),
            Ok(m) if m.is_dir() => fs::remove_dir(&full),
            Ok(_) => fs::remove_file(&full),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        };
        if let Err(e) = res {
            return io_error(e);
        }
        self.settle(vec![rel], None).await
    }
}

fn valid_name(name: &str) -> bool {
    !(name.is_empty() || name.contains(['/', '\0']) || name == "." || name == "..")
}

/// Moves `src` to `dst`, copying when they are on different file systems.
fn place(src: &Path, dst: &Path) -> std::io::Result<()> {
    match fs::rename(src, dst) {
        Ok(()) => Ok(()),
        Err(e) if e.raw_os_error() == Some(libc::EXDEV) => {
            fs::copy(src, dst)?;
            fs::remove_file(src)
        }
        Err(e) => Err(e),
    }
}

fn set_attrs(path: &Path, kind: Kind, mode: Option<u32>, mtime_ns: Option<i64>) -> std::io::Result<()> {
    if kind == Kind::Symlink {
        return Ok(());
    }
    if let Some(mode) = mode {
        fs::set_permissions(path, fs::Permissions::from_mode(mode & 0o7777))?;
    }
    if let Some(ns) = mtime_ns {
        let f = fs::File::options()
            .read(kind == Kind::Dir)
            .write(kind == Kind::File)
            .open(path)?;
        f.set_modified(time_of(ns))?;
    }
    Ok(())
}

/// A request and where its answer goes.
pub type Call = (Request, oneshot::Sender<Response>);

/// Serves the protocol on a Unix socket at `path` (replacing a stale one),
/// passing requests to the sync loop through `calls`. For a lazy directory,
/// `hydrator` fetches content before a `Fetch` is answered.
pub async fn serve(
    path: PathBuf,
    calls: mpsc::Sender<Call>,
    hydrator: Option<Arc<Hydrator>>,
) -> std::io::Result<()> {
    let _ = fs::remove_file(&path);
    let listener = UnixListener::bind(&path)?;
    // Only this user's processes (the extension runs as the user).
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
    loop {
        let (stream, _) = listener.accept().await?;
        tokio::spawn(connection(stream, calls.clone(), hydrator.clone()));
    }
}

async fn connection(stream: UnixStream, calls: mpsc::Sender<Call>, hydrator: Option<Arc<Hydrator>>) {
    let (read, mut write) = stream.into_split();
    let mut lines = BufReader::new(read).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        let resp = match serde_json::from_str::<Request>(&line) {
            Ok(req) => answer(req, &calls, hydrator.as_deref()).await,
            Err(e) => error(ErrorCode::BadRequest, e.to_string()),
        };
        let mut out = serde_json::to_vec(&resp).expect("serializable");
        out.push(b'\n');
        if write.write_all(&out).await.is_err() {
            return;
        }
    }
}

async fn answer(req: Request, calls: &mpsc::Sender<Call>, hydrator: Option<&Hydrator>) -> Response {
    let fetch = matches!(req, Request::Fetch { .. });
    let (tx, rx) = oneshot::channel();
    if calls.send((req, tx)).await.is_err() {
        return error(ErrorCode::Io, "the sync loop has stopped");
    }
    let resp = rx
        .await
        .unwrap_or_else(|_| error(ErrorCode::Io, "the sync loop has stopped"));
    if fetch
        && let (Response::Path { path }, Some(h)) = (&resp, hydrator)
        && let Err(e) = h.hydrate(path).await
    {
        return error(ErrorCode::Io, format!("fetching {}: {e}", path.display()));
    }
    resp
}
