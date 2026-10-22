//! Keeps volumes syncing after the TUI quits: one background service per
//! volume, which runs `rustfuse daemon <key>` (see [`crate::daemon`]) and
//! restarts it if it stops. A systemd user unit on Linux, a launchd agent
//! on a Mac, and where neither is there (a container, a system without
//! systemd) a plain detached process that lasts until it exits.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};

use crate::places::Places;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Services {
    /// `~/.config/systemd/user/rustfuse-<key>.service`.
    Systemd { dir: PathBuf },
    /// `~/Library/LaunchAgents/dev.rustfuse.tui.volume.<key>.plist`.
    Launchd { dir: PathBuf },
    /// A detached process, its pid in `<data>/run/<key>.pid`.
    Detached { dir: PathBuf },
}

impl Services {
    pub fn for_platform(places: &Places) -> Services {
        let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
        if cfg!(target_os = "macos") {
            return Services::Launchd {
                dir: home.join("Library/LaunchAgents"),
            };
        }
        let systemd = Command::new("systemctl")
            .args(["--user", "show-environment"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success());
        if systemd {
            let config = std::env::var_os("XDG_CONFIG_HOME")
                .map(PathBuf::from)
                .filter(|p| p.is_absolute())
                .unwrap_or_else(|| home.join(".config"));
            Services::Systemd {
                dir: config.join("systemd/user"),
            }
        } else {
            Services::Detached {
                dir: places.data.join("run"),
            }
        }
    }

    pub fn describe(&self) -> &'static str {
        match self {
            Services::Systemd { .. } => "systemd user service",
            Services::Launchd { .. } => "launchd agent",
            Services::Detached { .. } => "background process",
        }
    }

    fn file(&self, key: &str) -> PathBuf {
        match self {
            Services::Systemd { dir } => dir.join(unit(key)),
            Services::Launchd { dir } => dir.join(format!("{}.plist", label(key))),
            Services::Detached { dir } => dir.join(format!("{key}.pid")),
        }
    }

    /// Whether `key` has a service (it may not be running).
    pub fn installed(&self, key: &str) -> bool {
        match self {
            Services::Detached { .. } => self.detached_pid(key).is_some(),
            _ => self.file(key).exists(),
        }
    }

    /// Installs (or replaces) and starts the service for `key`, running
    /// `exe daemon <key>` with its output appended to `log`.
    pub fn start(&self, key: &str, exe: &Path, log: &Path) -> Result<()> {
        if let Some(dir) = log.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let file = self.file(key);
        if let Some(dir) = file.parent() {
            std::fs::create_dir_all(dir)?;
        }
        match self {
            Services::Systemd { .. } => {
                std::fs::write(&file, systemd_unit(key, exe, log))?;
                run("systemctl", &["--user", "daemon-reload"])?;
                run("systemctl", &["--user", "enable", &unit(key)])?;
                run("systemctl", &["--user", "restart", &unit(key)])
            }
            Services::Launchd { .. } => {
                std::fs::write(&file, launchd_plist(key, exe, log))?;
                let domain = format!("gui/{}", rustix::process::getuid().as_raw());
                let _ = run("launchctl", &["bootout", &format!("{domain}/{}", label(key))]);
                run("launchctl", &["bootstrap", &domain, &file.to_string_lossy()])
            }
            Services::Detached { .. } => {
                self.stop(key)?;
                let out = std::fs::OpenOptions::new().create(true).append(true).open(log)?;
                let mut child = detached(exe, key)
                    .stdout(out.try_clone()?)
                    .stderr(out)
                    .spawn()
                    .with_context(|| format!("starting {}", exe.display()))?;
                std::fs::write(&file, child.id().to_string())?;
                // Reaped when it exits, so a stopped daemon does not linger
                // as a zombie that still looks alive.
                std::thread::spawn(move || child.wait());
                Ok(())
            }
        }
    }

    /// Stops the service for `key` (and its daemon, which unmounts) and
    /// removes it, so it does not come back at login.
    pub fn stop(&self, key: &str) -> Result<()> {
        let file = self.file(key);
        match self {
            Services::Systemd { .. } => {
                if file.exists() {
                    run("systemctl", &["--user", "disable", "--now", &unit(key)])?;
                    std::fs::remove_file(&file)?;
                    let _ = run("systemctl", &["--user", "daemon-reload"]);
                }
            }
            Services::Launchd { .. } => {
                let domain = format!("gui/{}", rustix::process::getuid().as_raw());
                let _ = run("launchctl", &["bootout", &format!("{domain}/{}", label(key))]);
                let _ = std::fs::remove_file(&file);
            }
            Services::Detached { .. } => {
                if let Some(pid) = self.detached_pid(key) {
                    rustix::process::kill_process(pid, rustix::process::Signal::TERM)?;
                    // A mount takes a moment to unmount.
                    for _ in 0..100 {
                        if rustix::process::test_kill_process(pid).is_err() {
                            break;
                        }
                        std::thread::sleep(std::time::Duration::from_millis(100));
                    }
                }
                let _ = std::fs::remove_file(&file);
            }
        }
        Ok(())
    }

    /// The detached daemon's pid, if it is still running.
    fn detached_pid(&self, key: &str) -> Option<rustix::process::Pid> {
        let raw: i32 = std::fs::read_to_string(self.file(key))
            .ok()?
            .trim()
            .parse()
            .ok()?;
        let pid = rustix::process::Pid::from_raw(raw)?;
        rustix::process::test_kill_process(pid).ok()?;
        Some(pid)
    }
}

/// `exe daemon <key>` in its own process group, so it outlives the
/// terminal (and its hang-up) the TUI ran in.
fn detached(exe: &Path, key: &str) -> Command {
    use std::os::unix::process::CommandExt;
    let mut cmd = Command::new(exe);
    cmd.args(["daemon", key]).stdin(Stdio::null()).process_group(0);
    cmd
}

fn run(tool: &str, args: &[&str]) -> Result<()> {
    let out = Command::new(tool)
        .args(args)
        .output()
        .with_context(|| format!("running {tool}"))?;
    if !out.status.success() {
        bail!(
            "{tool} {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

pub fn unit(key: &str) -> String {
    format!("rustfuse-{key}.service")
}

pub fn label(key: &str) -> String {
    format!("dev.rustfuse.tui.volume.{key}")
}

/// `RUSTFUSE_HOME`, when set, so the service finds the same library.
fn home_env() -> Option<String> {
    std::env::var("RUSTFUSE_HOME").ok()
}

fn systemd_quote(s: &str) -> String {
    format!(
        "\"{}\"",
        s.replace('\\', "\\\\").replace('"', "\\\"").replace('%', "%%")
    )
}

fn systemd_unit(key: &str, exe: &Path, log: &Path) -> String {
    let env = home_env().map(|h| format!("Environment={}\n", systemd_quote(&format!("RUSTFUSE_HOME={h}"))));
    format!(
        "[Unit]\n\
         Description=rustFUSE volume {key}\n\
         After=network-online.target\n\
         \n\
         [Service]\n\
         ExecStart={} daemon {}\n\
         {}Restart=always\n\
         RestartSec=5\n\
         TimeoutStopSec=20\n\
         StandardOutput=append:{}\n\
         StandardError=append:{}\n\
         \n\
         [Install]\n\
         WantedBy=default.target\n",
        systemd_quote(&exe.to_string_lossy()),
        systemd_quote(key),
        env.unwrap_or_default(),
        log.display(),
        log.display(),
    )
}

fn xml(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

fn launchd_plist(key: &str, exe: &Path, log: &Path) -> String {
    let log = xml(&log.to_string_lossy());
    let env = home_env()
        .map(|h| {
            format!(
                "  <key>EnvironmentVariables</key>\n  <dict>\n    <key>RUSTFUSE_HOME</key>\n    <string>{}</string>\n  </dict>\n",
                xml(&h)
            )
        })
        .unwrap_or_default();
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
         <plist version=\"1.0\">\n\
         <dict>\n  \
         <key>Label</key>\n  <string>{}</string>\n  \
         <key>ProgramArguments</key>\n  <array>\n    <string>{}</string>\n    <string>daemon</string>\n    <string>{}</string>\n  </array>\n\
         {env}  \
         <key>RunAtLoad</key>\n  <true/>\n  \
         <key>KeepAlive</key>\n  <true/>\n  \
         <key>LimitLoadToSessionType</key>\n  <string>Aqua</string>\n  \
         <key>StandardOutPath</key>\n  <string>{log}</string>\n  \
         <key>StandardErrorPath</key>\n  <string>{log}</string>\n\
         </dict>\n\
         </plist>\n",
        xml(&label(key)),
        xml(&exe.to_string_lossy()),
        xml(key),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unit_and_plist() {
        let unit = systemd_unit(
            "proj",
            Path::new("/opt/rust fuse/rustfuse"),
            Path::new("/L/proj.log"),
        );
        assert!(
            unit.contains("ExecStart=\"/opt/rust fuse/rustfuse\" daemon \"proj\"\n"),
            "{unit}"
        );
        assert!(unit.contains("StandardOutput=append:/L/proj.log\n"));
        let plist = launchd_plist("a&b", Path::new("/bin/rustfuse"), Path::new("/L/x.log"));
        assert!(
            plist.contains("<string>dev.rustfuse.tui.volume.a&amp;b</string>"),
            "{plist}"
        );
        assert!(plist.contains("<string>daemon</string>"));
    }

    #[test]
    fn detached_service_starts_and_stops() {
        let dir = tempfile::tempdir().unwrap();
        let s = Services::Detached {
            dir: dir.path().join("run"),
        };
        // `sleep daemon proj` fails at once; a script that ignores its
        // arguments stands in for the daemon.
        let exe = dir.path().join("fake");
        std::fs::write(&exe, "#!/bin/sh\necho started \"$@\"\nexec sleep 30\n").unwrap();
        std::fs::set_permissions(&exe, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
        let log = dir.path().join("logs/proj.log");
        assert!(!s.installed("proj"));
        s.start("proj", &exe, &log).unwrap();
        assert!(s.installed("proj"));
        for _ in 0..100 {
            if std::fs::metadata(&log).is_ok_and(|m| m.len() > 0) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        s.stop("proj").unwrap();
        assert!(!s.installed("proj"));
        assert_eq!(std::fs::read_to_string(&log).unwrap(), "started daemon proj\n");
    }
}
