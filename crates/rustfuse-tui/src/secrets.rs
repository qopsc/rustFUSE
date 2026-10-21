//! Hub tokens and S3 secret keys, kept apart from the library and handed
//! to each daemon in its environment. On a Mac they are in the login
//! keychain under the same service names and accounts as the Mac app's
//! (the connection's id), so either can use what the other saved; the
//! keychain may ask once before letting the TUI read an item the app
//! wrote. Elsewhere they are in `secrets.json`, readable only by the user.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Kind {
    HubToken,
    S3Secret,
}

impl Kind {
    /// The keychain service, as the Mac app names it.
    pub fn service(self) -> &'static str {
        match self {
            Kind::HubToken => "dev.rustfuse.hub-token",
            Kind::S3Secret => "dev.rustfuse.s3-secret",
        }
    }
}

/// Where secrets are kept.
#[derive(Clone, Debug)]
pub enum Secrets {
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    Keychain,
    /// `{"<service>": {"<connection id>": "<secret>"}}`, mode 0600.
    File(PathBuf),
}

impl Secrets {
    /// The keychain on a Mac, otherwise `file`.
    pub fn for_platform(file: &Path) -> Secrets {
        if cfg!(target_os = "macos") && std::env::var_os("RUSTFUSE_HOME").is_none() {
            Secrets::Keychain
        } else {
            Secrets::File(file.to_path_buf())
        }
    }

    pub fn get(&self, kind: Kind, connection: &str) -> Option<String> {
        match self {
            Secrets::Keychain => keychain::get(kind.service(), connection),
            Secrets::File(f) => read(f).ok()?.get(kind.service())?.get(connection).cloned(),
        }
    }

    /// Saves `value`, or deletes the secret if it is empty.
    pub fn set(&self, kind: Kind, connection: &str, value: &str) -> Result<()> {
        match self {
            Secrets::Keychain => keychain::set(kind.service(), connection, value),
            Secrets::File(f) => {
                let mut all = read(f)?;
                let entries = all.entry(kind.service().to_string()).or_default();
                if value.is_empty() {
                    entries.remove(connection);
                } else {
                    entries.insert(connection.to_string(), value.to_string());
                }
                write(f, &all)
            }
        }
    }

    /// Deletes both of a connection's secrets.
    pub fn forget(&self, connection: &str) -> Result<()> {
        for kind in [Kind::HubToken, Kind::S3Secret] {
            self.set(kind, connection, "")?;
        }
        Ok(())
    }
}

type Store = BTreeMap<String, BTreeMap<String, String>>;

fn read(file: &Path) -> Result<Store> {
    match std::fs::read(file) {
        Ok(data) => serde_json::from_slice(&data).with_context(|| format!("reading {}", file.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Store::new()),
        Err(e) => Err(e).with_context(|| format!("reading {}", file.display())),
    }
}

fn write(file: &Path, all: &Store) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    if let Some(dir) = file.parent() {
        std::fs::create_dir_all(dir)?;
    }
    // Written in full to a new file created 0600, then renamed over, so
    // the secrets are never readable by others, not even briefly.
    let tmp = file.with_extension("json.tmp");
    let _ = std::fs::remove_file(&tmp);
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&tmp)?;
    f.write_all(&serde_json::to_vec_pretty(all)?)?;
    f.sync_all()?;
    std::fs::rename(&tmp, file).with_context(|| format!("saving {}", file.display()))
}

#[cfg(target_os = "macos")]
mod keychain {
    use anyhow::Result;
    use security_framework::passwords;

    pub fn get(service: &str, account: &str) -> Option<String> {
        let data = passwords::get_generic_password(service, account).ok()?;
        String::from_utf8(data).ok()
    }

    pub fn set(service: &str, account: &str, value: &str) -> Result<()> {
        let _ = passwords::delete_generic_password(service, account);
        if !value.is_empty() {
            passwords::set_generic_password(service, account, value.as_bytes())?;
        }
        Ok(())
    }
}

#[cfg(not(target_os = "macos"))]
mod keychain {
    use anyhow::{Result, bail};

    pub fn get(_: &str, _: &str) -> Option<String> {
        None
    }

    pub fn set(_: &str, _: &str, _: &str) -> Result<()> {
        bail!("no keychain here")
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    #[test]
    fn file_secrets() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("cfg/secrets.json");
        let s = Secrets::File(file.clone());
        assert_eq!(s.get(Kind::HubToken, "A"), None);
        s.set(Kind::HubToken, "A", "tok").unwrap();
        s.set(Kind::S3Secret, "A", "sk").unwrap();
        s.set(Kind::HubToken, "B", "tok-b").unwrap();
        assert_eq!(s.get(Kind::HubToken, "A").as_deref(), Some("tok"));
        assert_eq!(s.get(Kind::S3Secret, "A").as_deref(), Some("sk"));
        assert_eq!(
            std::fs::metadata(&file).unwrap().permissions().mode() & 0o777,
            0o600
        );
        s.forget("A").unwrap();
        assert_eq!(s.get(Kind::HubToken, "A"), None);
        assert_eq!(s.get(Kind::HubToken, "B").as_deref(), Some("tok-b"));
    }
}
