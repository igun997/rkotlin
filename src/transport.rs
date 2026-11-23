//! Thin wrapper over the system `ssh` client. Every backend ends up as "ssh args + destination".

use anyhow::{bail, Result};
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use crate::util::shq;

#[derive(Debug, Clone)]
pub struct Ssh {
    /// Backend specific arguments; the last one is the destination (host alias or user@host).
    pub args: Vec<String>,
    pub control_dir: PathBuf,
}

pub fn ssh_bin() -> String {
    std::env::var("RKOTLIN_SSH").unwrap_or_else(|_| "ssh".into())
}

impl Ssh {
    fn common(&self) -> Command {
        let mut c = Command::new(ssh_bin());
        c.args([
            "-o",
            "BatchMode=yes",
            "-o",
            "LogLevel=ERROR",
            "-o",
            "ServerAliveInterval=30",
            "-o",
            "ConnectTimeout=20",
        ]);
        c.arg("-o")
            .arg(format!("ControlPath={}/%C", self.control_dir.display()));
        c
    }

    /// Start a background multiplexing master so each later call skips the SSH handshake.
    /// Best effort: if it fails, commands simply open their own connection.
    pub fn ensure_master(&self) {
        let _ = std::fs::create_dir_all(&self.control_dir);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ =
                std::fs::set_permissions(&self.control_dir, std::fs::Permissions::from_mode(0o700));
        }
        if self.master_alive() {
            return;
        }
        // All stdio must be detached or the persistent master keeps our pipes open forever.
        let _ = self
            .common()
            .args(["-o", "ControlMaster=yes", "-o", "ControlPersist=10m", "-fN"])
            .args(&self.args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }

    pub fn close_master(&self) {
        let _ = self
            .common()
            .args(["-O", "exit"])
            .args(&self.args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }

    /// True when a multiplexing master for this destination is already running (local, no network).
    pub fn master_alive(&self) -> bool {
        self.common()
            .args(["-O", "check"])
            .args(&self.args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    }

    /// Command that runs `script` under `bash -lc` on the remote.
    pub fn remote(&self, script: &str) -> Command {
        let mut c = self.common();
        c.args(["-T", "-o", "ControlMaster=no"]);
        c.args(&self.args);
        c.arg(format!("bash -lc {}", shq(script)));
        c
    }

    /// Run a script, feed optional stdin, return (exit code, stdout, stderr).
    pub fn capture(&self, script: &str, stdin: Option<&[u8]>) -> Result<(i32, Vec<u8>, Vec<u8>)> {
        let mut child = self
            .remote(script)
            .stdin(if stdin.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(spawn_err)?;
        let writer = stdin.map(|data| {
            let mut si = child.stdin.take().expect("piped stdin");
            let data = data.to_vec();
            std::thread::spawn(move || {
                let _ = si.write_all(&data);
            })
        });
        let out = child.wait_with_output()?;
        if let Some(w) = writer {
            let _ = w.join();
        }
        Ok((exit_code(out.status), out.stdout, out.stderr))
    }

    /// Like `capture` but fails with the remote's stderr on non-zero exit.
    pub fn capture_ok(&self, script: &str) -> Result<String> {
        let (code, out, err) = self.capture(script, None)?;
        if code != 0 {
            bail!(
                "remote command failed (exit {code}): {}",
                String::from_utf8_lossy(&err).trim()
            );
        }
        Ok(String::from_utf8_lossy(&out).into_owned())
    }

    /// Interactive shell with a tty; returns its exit code.
    pub fn interactive(&self, script: &str) -> Result<i32> {
        let mut c = self.common();
        c.args(["-tt", "-o", "ControlMaster=no"]);
        c.args(&self.args);
        c.arg(format!("bash -lc {}", shq(script)));
        let st = c.status().map_err(spawn_err)?;
        Ok(exit_code(st))
    }
}

pub fn exit_code(st: std::process::ExitStatus) -> i32 {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(sig) = st.signal() {
            return 128 + sig;
        }
    }
    st.code().unwrap_or(1)
}

/// Spawn failure for the ssh client: a missing binary is a setup problem, not a crash.
pub fn spawn_err(e: std::io::Error) -> anyhow::Error {
    if e.kind() == std::io::ErrorKind::NotFound {
        crate::guidance::ssh_missing().into_err()
    } else {
        anyhow::anyhow!("failed to start `{}`: {e}", ssh_bin())
    }
}
