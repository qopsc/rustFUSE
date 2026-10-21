//! `rustfuse daemon <key>`: what a volume's background service runs. It
//! reads the volume from the library and its connection's secrets, then
//! becomes `rustfuse sync` (or `rustfuse mount`) with the same arguments
//! the Mac app would use, so signals from the service manager reach the
//! daemon itself.

use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, bail};

use crate::library::{self, Library, Mode};
use crate::places::Places;
use crate::secrets::{Kind, Secrets};

pub fn run(key: &str) -> Result<()> {
    let places = Places::find()?;
    let lib = Library::load(&places.library)?;
    let Some(v) = lib.volume(key) else {
        bail!("no volume {key} in {}", places.library.display());
    };
    if v.mode == Mode::Finder {
        bail!("{key} shows in Finder, which only the Mac app can run");
    }
    let Some(c) = lib.connection(&v.connection) else {
        bail!("volume {key} has no connection");
    };
    places.create()?;
    let secrets = Secrets::for_platform(&places.secrets);
    let dir = library::synced_dir(v, &places.data);
    if v.mode == Mode::Mount {
        let Some(mountpoint) = v.path.as_deref() else {
            bail!("volume {key} has no mount point");
        };
        std::fs::create_dir_all(mountpoint).with_context(|| format!("creating {mountpoint}"))?;
    }
    let args = library::daemon_args(v, &dir, &places.socket(key), &lib.settings);
    let env = library::tool_env(
        c,
        secrets.get(Kind::HubToken, &c.id),
        secrets.get(Kind::S3Secret, &c.id),
    );
    let exe = std::env::current_exe()?;
    eprintln!("rustfuse daemon {key}: {}", args.join(" "));
    let err = exec(&exe, &args, &env);
    Err(err).with_context(|| format!("running {}", exe.display()))
}

fn exec(exe: &Path, args: &[String], env: &std::collections::BTreeMap<String, String>) -> std::io::Error {
    let mut cmd = Command::new(exe);
    cmd.args(args).envs(env);
    // The log is a file: no colours.
    cmd.env("NO_COLOR", "1");
    if std::env::var_os("RUST_LOG").is_none() {
        cmd.env("RUST_LOG", "info");
    }
    cmd.exec()
}
