//! Where the TUI and its volume daemons keep things. On a Mac that is the
//! Mac app's group container, so both see the same library; on Linux the
//! library is in `~/.config/rustfuse` and the rest in
//! `~/.local/share/rustfuse` (or the XDG directories). `RUSTFUSE_HOME`
//! puts everything in one directory instead.
//!
//! ```text
//! <data>/volumes/<key>/  a lazy volume's synced directory
//! <data>/s/<key>.sock    its daemon's provider socket
//! <data>/logs/<key>.log  its daemon's log
//! <data>/ca/<id>.pem     a connection's hub CA certificates
//! ```

use std::path::{Path, PathBuf};

use anyhow::{Result, bail};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Places {
    /// `library.json`.
    pub library: PathBuf,
    /// Secrets where there is no keychain (`secrets.json`, mode 0600).
    pub secrets: PathBuf,
    pub data: PathBuf,
}

impl Places {
    /// Everything in `dir`.
    pub fn in_dir(dir: &Path) -> Places {
        Places {
            library: dir.join("library.json"),
            secrets: dir.join("secrets.json"),
            data: dir.to_path_buf(),
        }
    }

    pub fn find() -> Result<Places> {
        if let Some(dir) = std::env::var_os("RUSTFUSE_HOME") {
            return Ok(Places::in_dir(Path::new(&dir)));
        }
        let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
            bail!("HOME is not set");
        };
        if cfg!(target_os = "macos") {
            return Ok(Places::in_dir(&mac_container(&home)));
        }
        let xdg = |var: &str, default: &str| {
            std::env::var_os(var)
                .map(PathBuf::from)
                .filter(|p| p.is_absolute())
                .unwrap_or_else(|| home.join(default))
                .join("rustfuse")
        };
        let config = xdg("XDG_CONFIG_HOME", ".config");
        Ok(Places {
            library: config.join("library.json"),
            secrets: config.join("secrets.json"),
            data: xdg("XDG_DATA_HOME", ".local/share"),
        })
    }

    /// Kept short: socket paths have a 104-byte limit on a Mac.
    pub fn socket(&self, key: &str) -> PathBuf {
        self.data.join("s").join(format!("{key}.sock"))
    }

    pub fn log(&self, key: &str) -> PathBuf {
        self.data.join("logs").join(format!("{key}.log"))
    }

    pub fn ca_file(&self, connection: &str) -> PathBuf {
        self.data.join("ca").join(format!("{connection}.pem"))
    }

    /// Creates the directories daemons write into.
    pub fn create(&self) -> Result<()> {
        for sub in ["volumes", "s", "logs", "ca"] {
            std::fs::create_dir_all(self.data.join(sub))?;
        }
        Ok(())
    }
}

/// The Mac app's group container: `dev.rustfuse` when the app is unsigned,
/// `<team id>.dev.rustfuse` when signed. One that exists wins, the signed
/// one first.
fn mac_container(home: &Path) -> PathBuf {
    let groups = home.join("Library/Group Containers");
    let mut found: Vec<PathBuf> = std::fs::read_dir(&groups)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.ends_with(".dev.rustfuse"))
        })
        .collect();
    found.sort();
    found
        .into_iter()
        .next()
        .unwrap_or_else(|| groups.join("dev.rustfuse"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout() {
        let p = Places::in_dir(Path::new("/R"));
        assert_eq!(p.library, Path::new("/R/library.json"));
        assert_eq!(p.socket("proj"), Path::new("/R/s/proj.sock"));
        assert_eq!(p.log("proj"), Path::new("/R/logs/proj.log"));
    }

    #[test]
    fn mac_container_prefers_the_signed_app() {
        let home = tempfile::tempdir().unwrap();
        assert_eq!(
            mac_container(home.path()),
            home.path().join("Library/Group Containers/dev.rustfuse")
        );
        std::fs::create_dir_all(home.path().join("Library/Group Containers/dev.rustfuse")).unwrap();
        std::fs::create_dir_all(
            home.path()
                .join("Library/Group Containers/ABCDE12345.dev.rustfuse"),
        )
        .unwrap();
        assert_eq!(
            mac_container(home.path()),
            home.path()
                .join("Library/Group Containers/ABCDE12345.dev.rustfuse")
        );
    }
}
