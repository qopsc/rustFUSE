//! Saved connections, volumes and settings: the same `library.json` the
//! Mac app keeps (`macos/RustFuseKit/Sources/RustFuseKit/Library.swift`),
//! so a volume added in either shows in both. Secrets are never kept here
//! (see [`crate::secrets`]). The daemon command lines are built here, as
//! the app builds them, so they can be tested.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// The default cache size for new lazy volumes, in bytes (decimal GB, as
/// the app shows sizes).
pub const DEFAULT_CACHE_SIZE: u64 = 20_000_000_000;

/// How a volume shows up on this machine.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    /// In Finder under Locations (File Provider). Only the Mac app can run
    /// these.
    Finder,
    /// A plain folder anywhere, holding a full copy.
    Folder,
    /// A FUSE mount anywhere. Files download when first opened.
    Mount,
}

impl Mode {
    /// Whether content stays in the bucket until used (and a cache size
    /// applies).
    pub fn is_lazy(self) -> bool {
        self != Mode::Folder
    }

    pub fn label(self) -> &'static str {
        match self {
            Mode::Finder => "Finder",
            Mode::Folder => "folder",
            Mode::Mount => "mount",
        }
    }
}

/// Reading the bucket directly, beside the hub: faster on a LAN.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct S3Access {
    pub bucket: String,
    /// Defaults to AWS.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub region: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prefix: Option<String>,
    pub access_key: String,
}

/// A saved hub (and optionally its bucket). Its token and S3 secret key
/// are kept as secrets under its id.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Connection {
    /// A UUID, upper case as the app writes it.
    pub id: String,
    pub name: String,
    /// `host[:port]` or `tls://host[:port]`.
    pub hub: String,
    /// CA certificates (PEM) to trust for the hub.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ca_file: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub s3: Option<S3Access>,
}

impl Connection {
    pub fn new(name: String, hub: String) -> Connection {
        Connection {
            id: new_id(),
            name,
            hub,
            ca_file: None,
            s3: None,
        }
    }
}

/// A volume this machine uses.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VolumeConfig {
    /// Unique on this machine: the file names for it. Usually the volume's
    /// name.
    pub key: String,
    /// The volume's name in the bucket.
    pub name: String,
    /// The connection's id.
    pub connection: String,
    pub mode: Mode,
    /// `folder`: the synced folder. `mount`: the mount point.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// Lazy modes: the most fetched content to keep, in bytes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_size: Option<u64>,
}

/// What quitting the Mac app does to syncing. Kept for the app; the TUI
/// leaves its volumes to background services either way.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum OnQuit {
    #[default]
    Stop,
    KeepSyncing,
}

/// macFUSE's two ways to mount on a Mac.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MountBackend {
    /// No kernel extension (macOS 15.4 or later).
    #[default]
    Fskit,
    /// macFUSE's kernel extension.
    Kernel,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Settings {
    pub on_quit: OnQuit,
    /// For new lazy volumes.
    pub default_cache_size: u64,
    pub mount_backend: MountBackend,
}

impl Default for Settings {
    fn default() -> Settings {
        Settings {
            on_quit: OnQuit::Stop,
            default_cache_size: DEFAULT_CACHE_SIZE,
            mount_backend: MountBackend::Fskit,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Library {
    pub connections: Vec<Connection>,
    pub volumes: Vec<VolumeConfig>,
    pub settings: Settings,
}

impl Library {
    /// The library in `file`, or an empty one if there is none yet.
    pub fn load(file: &Path) -> Result<Library> {
        match std::fs::read(file) {
            Ok(data) => serde_json::from_slice(&data).with_context(|| format!("reading {}", file.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Library::default()),
            Err(e) => Err(e).with_context(|| format!("reading {}", file.display())),
        }
    }

    /// Saves to `file` atomically, sorted and indented as the app saves.
    pub fn save(&self, file: &Path) -> Result<()> {
        if let Some(dir) = file.parent() {
            std::fs::create_dir_all(dir)?;
        }
        // Through `Value` for sorted keys, like the app's `.sortedKeys`.
        let value = serde_json::to_value(self)?;
        let tmp = file.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(&value)?)?;
        std::fs::rename(&tmp, file).with_context(|| format!("saving {}", file.display()))
    }

    pub fn connection(&self, id: &str) -> Option<&Connection> {
        self.connections.iter().find(|c| c.id.eq_ignore_ascii_case(id))
    }

    pub fn volume(&self, key: &str) -> Option<&VolumeConfig> {
        self.volumes.iter().find(|v| v.key == key)
    }

    /// A key for a new volume named `name`: the name, or the name with a
    /// number when the same volume is already used another way.
    pub fn new_key(&self, name: &str) -> String {
        let taken: HashSet<&str> = self.volumes.iter().map(|v| v.key.as_str()).collect();
        if !taken.contains(name) {
            return name.to_string();
        }
        (2..)
            .map(|n| format!("{name}-{n}"))
            .find(|k| !taken.contains(k.as_str()))
            .unwrap()
    }
}

/// Volume names and keys double as file names (and, on a Mac, File
/// Provider domain ids).
pub fn valid_name(name: &str) -> bool {
    (1..=40).contains(&name.chars().count())
        && !name.starts_with('.')
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

/// A new random (version 4) UUID, upper case as the app writes them.
pub fn new_id() -> String {
    let mut b = [0u8; 16];
    getrandom::fill(&mut b).expect("no random numbers");
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let hex: String = b.iter().map(|x| format!("{x:02X}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..]
    )
}

/// Where a volume's synced directory is: the user's folder, or
/// `data/volumes/<key>` for lazy volumes.
pub fn synced_dir(v: &VolumeConfig, data: &Path) -> PathBuf {
    match (&v.mode, &v.path) {
        (Mode::Folder, Some(p)) => PathBuf::from(p),
        _ => data.join("volumes").join(&v.key),
    }
}

/// `--hub`, `--hub-ca` and the bucket options for `c`.
pub fn connection_args(c: &Connection) -> Vec<String> {
    let mut args = vec!["--hub".to_string(), c.hub.clone()];
    if let Some(ca) = &c.ca_file {
        args.extend(["--hub-ca".to_string(), ca.clone()]);
    }
    if let Some(s3) = &c.s3 {
        args.extend(["--bucket".to_string(), s3.bucket.clone()]);
        for (flag, value) in [
            ("--endpoint", &s3.endpoint),
            ("--region", &s3.region),
            ("--prefix", &s3.prefix),
        ] {
            if let Some(v) = value.as_ref().filter(|v| !v.is_empty()) {
                args.extend([flag.to_string(), v.clone()]);
            }
        }
    }
    args
}

fn strings(parts: &[&str]) -> Vec<String> {
    parts.iter().map(|s| s.to_string()).collect()
}

/// `rustfuse attach` for a new volume into `dir`.
pub fn attach_args(v: &VolumeConfig, c: &Connection, dir: &Path) -> Vec<String> {
    let mut args = strings(&["attach", &dir.to_string_lossy(), "--volume", &v.name]);
    if v.mode.is_lazy() {
        args.push("--lazy".into());
    }
    args.extend(connection_args(c));
    args
}

/// `rustfuse init`: a new volume `name` from the folder `dir`, which then
/// syncs in place.
pub fn init_args(name: &str, c: &Connection, dir: &Path) -> Vec<String> {
    let mut args = strings(&["init", &dir.to_string_lossy(), "--volume", name]);
    args.extend(connection_args(c));
    args
}

/// `rustfuse volumes`: the volumes `c`'s token opens.
pub fn volumes_args(c: &Connection) -> Vec<String> {
    let mut args = strings(&["volumes", "--json"]);
    args.extend(connection_args(c));
    args
}

/// `rustfuse ls`: a folder of volume `name` as the bucket holds it.
pub fn ls_args(name: &str, path: &str, c: &Connection) -> Vec<String> {
    let mut args = strings(&["ls", "--volume", name, "--json"]);
    if !path.is_empty() {
        args.push(path.to_string());
    }
    args.extend(connection_args(c));
    args
}

/// `rustfuse get`: downloads `path` of volume `name` to `dest`.
pub fn get_args(name: &str, path: &str, dest: &Path, c: &Connection) -> Vec<String> {
    let mut args = strings(&["get", "--volume", name, path, &dest.to_string_lossy()]);
    args.extend(connection_args(c));
    args
}

/// The long-running command that keeps `v` in sync (and mounted), with
/// the socket the TUI (and the app) talk to.
pub fn daemon_args(v: &VolumeConfig, dir: &Path, socket: &Path, settings: &Settings) -> Vec<String> {
    let dir = dir.to_string_lossy();
    let socket = socket.to_string_lossy();
    let mut args = match v.mode {
        Mode::Finder | Mode::Folder => strings(&["sync", &dir, "--provider-socket", &socket]),
        Mode::Mount => strings(&[
            "mount",
            &dir,
            v.path.as_deref().unwrap_or(""),
            "--provider-socket",
            &socket,
        ]),
    };
    if v.mode.is_lazy() {
        let size = v.cache_size.unwrap_or(settings.default_cache_size);
        args.extend(["--cache-size".to_string(), size.to_string()]);
    }
    if v.mode == Mode::Mount && cfg!(target_os = "macos") && settings.mount_backend == MountBackend::Fskit {
        args.push("--fskit".into());
    }
    args
}

/// The environment variables carrying `c`'s secrets to `rustfuse`.
pub fn tool_env(
    c: &Connection,
    token: Option<String>,
    s3_secret: Option<String>,
) -> BTreeMap<String, String> {
    let mut env = BTreeMap::new();
    if let Some(t) = token {
        env.insert("RUSTFUSE_HUB_TOKEN".into(), t);
    }
    if let Some(s3) = &c.s3 {
        env.insert("RUSTFUSE_ACCESS_KEY".into(), s3.access_key.clone());
        if let Some(s) = s3_secret {
            env.insert("RUSTFUSE_SECRET_KEY".into(), s);
        }
    }
    env
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The sample the Swift tests decode too.
    const SAMPLE: &str = include_str!("../../../macos/RustFuseKit/Tests/RustFuseKitTests/library.json");

    #[test]
    fn reads_and_writes_the_apps_library() {
        let lib: Library = serde_json::from_str(SAMPLE).unwrap();
        assert_eq!(lib.connections.len(), 2);
        assert_eq!(lib.connections[1].s3.as_ref().unwrap().access_key, "rustfuse");
        assert_eq!(
            lib.volumes.iter().map(|v| v.mode).collect::<Vec<_>>(),
            [Mode::Finder, Mode::Folder, Mode::Mount]
        );
        assert_eq!(lib.settings.on_quit, OnQuit::KeepSyncing);
        assert_eq!(lib.settings.mount_backend, MountBackend::Kernel);
        // Saved back, the sample is unchanged.
        let expected: serde_json::Value = serde_json::from_str(SAMPLE).unwrap();
        assert_eq!(serde_json::to_value(&lib).unwrap(), expected);
    }

    #[test]
    fn missing_fields_default() {
        let lib: Library = serde_json::from_str(r#"{"settings":{"onQuit":"keepSyncing"}}"#).unwrap();
        assert_eq!(lib.settings.on_quit, OnQuit::KeepSyncing);
        assert_eq!(lib.settings.default_cache_size, DEFAULT_CACHE_SIZE);
        assert!(lib.volumes.is_empty());
        assert_eq!(
            Library::load(Path::new("/nonexistent/library.json")).unwrap(),
            Library::default()
        );
    }

    #[test]
    fn save_and_load() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("sub/library.json");
        let mut lib = Library::default();
        lib.connections
            .push(Connection::new("home".into(), "nas:7420".into()));
        lib.save(&file).unwrap();
        assert_eq!(Library::load(&file).unwrap(), lib);
    }

    #[test]
    fn keys_names_and_ids() {
        let mut lib = Library::default();
        assert_eq!(lib.new_key("proj"), "proj");
        for key in ["proj", "proj-2"] {
            lib.volumes.push(VolumeConfig {
                key: key.into(),
                name: "proj".into(),
                connection: "c".into(),
                mode: Mode::Folder,
                path: None,
                cache_size: None,
            });
        }
        assert_eq!(lib.new_key("proj"), "proj-3");
        assert!(valid_name("my_vol-1.2") && !valid_name(".x") && !valid_name("a/b") && !valid_name(""));
        let id = new_id();
        assert_eq!(id.len(), 36);
        assert_eq!(&id[14..15], "4");
        assert_eq!(id, id.to_uppercase());
    }

    #[test]
    fn command_lines() {
        let c = Connection {
            ca_file: Some("/D/ca/x.pem".into()),
            s3: Some(S3Access {
                bucket: "b".into(),
                endpoint: Some("http://nas:9000".into()),
                region: Some(String::new()),
                prefix: None,
                access_key: "AK".into(),
            }),
            ..Connection::new("home".into(), "tls://hub:7420".into())
        };
        let data = Path::new("/D");
        let sock = Path::new("/D/s/proj.sock");
        let settings = Settings::default();
        let mount = VolumeConfig {
            key: "proj-2".into(),
            name: "proj".into(),
            connection: c.id.clone(),
            mode: Mode::Mount,
            path: Some("/mnt/proj".into()),
            cache_size: Some(1000),
        };
        let dir = synced_dir(&mount, data);
        assert_eq!(dir, Path::new("/D/volumes/proj-2"));
        assert_eq!(
            attach_args(&mount, &c, &dir).join(" "),
            "attach /D/volumes/proj-2 --volume proj --lazy --hub tls://hub:7420 --hub-ca /D/ca/x.pem \
             --bucket b --endpoint http://nas:9000"
        );
        let fskit = if cfg!(target_os = "macos") { " --fskit" } else { "" };
        assert_eq!(
            daemon_args(&mount, &dir, sock, &settings).join(" "),
            format!(
                "mount /D/volumes/proj-2 /mnt/proj --provider-socket /D/s/proj.sock --cache-size 1000{fskit}"
            )
        );
        let folder = VolumeConfig {
            mode: Mode::Folder,
            path: Some("/home/me/proj".into()),
            ..mount
        };
        let dir = synced_dir(&folder, data);
        assert_eq!(
            daemon_args(&folder, &dir, sock, &settings).join(" "),
            "sync /home/me/proj --provider-socket /D/s/proj.sock"
        );
        assert!(!attach_args(&folder, &c, &dir).contains(&"--lazy".to_string()));
        assert_eq!(
            ls_args("proj", "", &c)[..4],
            strings(&["ls", "--volume", "proj", "--json"])
        );
        let env = tool_env(&c, Some("tok".into()), None);
        assert_eq!(env["RUSTFUSE_HUB_TOKEN"], "tok");
        assert_eq!(env["RUSTFUSE_ACCESS_KEY"], "AK");
        assert!(!env.contains_key("RUSTFUSE_SECRET_KEY"));
    }
}
