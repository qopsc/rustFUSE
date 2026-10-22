//! Where a machine keeps an encrypted volume's key (`docs/design.md` §5.6):
//! `.rustfuse/key`, readable only by the user. The directory beside it
//! holds the same files in the clear, so the key on this disk exposes
//! nothing more; it is what the bucket and the hub never see.

use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use rustfuse_crypt::{Cipher, VolumeKey};

use crate::{Error, IoContext, Result, STATE_DIR};

fn key_path(root: &Path) -> PathBuf {
    root.join(STATE_DIR).join("key")
}

pub(crate) fn save_key(root: &Path, key: &VolumeKey) -> Result<()> {
    let path = key_path(root);
    let tmp = path.with_extension("tmp");
    let _ = std::fs::remove_file(&tmp);
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&tmp)
        .at(&tmp)?;
    f.write_all(key.to_hex().as_bytes()).at(&tmp)?;
    f.sync_all().at(&tmp)?;
    std::fs::rename(&tmp, &path).at(&path)
}

/// The key, if the directory's volume is encrypted.
pub fn load_key_if_any(root: &Path) -> Result<Option<Cipher>> {
    if key_path(root).exists() {
        load_key(root).map(Some)
    } else {
        Ok(None)
    }
}

pub(crate) fn load_key(root: &Path) -> Result<Cipher> {
    let path = key_path(root);
    let hex = std::fs::read_to_string(&path).at(&path)?;
    let key = VolumeKey::from_hex(&hex).map_err(|e| Error::Corrupt(format!("{}: {e}", path.display())))?;
    Ok(Cipher::new(&key))
}
