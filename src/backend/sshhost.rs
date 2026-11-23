use anyhow::Result;

use super::{control_dir, Backend, Status};
use crate::config::Instance;
use crate::transport::Ssh;

/// Any machine you can `ssh` into: your own VPS, a Hetzner box, a beefy desktop at home.
pub struct SshHost {
    ssh: Ssh,
}

impl SshHost {
    pub fn new(name: &str, inst: &Instance) -> Result<Self> {
        let Some(host) = inst.host.clone() else {
            return Err(crate::guidance::ssh_host_missing(name).into_err());
        };
        let mut args = vec![];
        if let Some(p) = inst.port {
            args.extend(["-p".to_string(), p.to_string()]);
        }
        if let Some(i) = &inst.identity {
            args.extend([
                "-i".to_string(),
                i.clone(),
                "-o".into(),
                "IdentitiesOnly=yes".into(),
            ]);
        }
        args.push(match &inst.user {
            Some(u) if !host.contains('@') => format!("{u}@{host}"),
            _ => host,
        });
        Ok(Self {
            ssh: Ssh {
                args,
                control_dir: control_dir(),
            },
        })
    }
}

impl Backend for SshHost {
    fn ensure(&self) -> Result<Ssh> {
        let (code, _, err) = self.ssh.capture("true", None)?;
        if code != 0 {
            return Err(crate::guidance::ssh_unreachable(
                self.ssh.args.last().unwrap(),
                &String::from_utf8_lossy(&err),
            )
            .into_err());
        }
        Ok(self.ssh.clone())
    }

    fn status(&self) -> Result<Status> {
        match self
            .ssh
            .capture("nproc; free -m | awk '/Mem:/{print $2}'", None)
        {
            Ok((0, out, _)) => {
                let t = String::from_utf8_lossy(&out);
                let mut l = t.lines();
                let cpus = l.next().unwrap_or("?");
                let mem = l.next().unwrap_or("?");
                Ok(Status {
                    state: "running".into(),
                    detail: format!("{cpus} vCPU / {mem} MiB (ssh)"),
                })
            }
            Ok((_, _, err)) => Ok(Status {
                state: "unreachable".into(),
                detail: String::from_utf8_lossy(&err).trim().to_string(),
            }),
            Err(e) => Ok(Status {
                state: "unreachable".into(),
                detail: e.to_string(),
            }),
        }
    }

    fn stop(&self) -> Result<()> {
        self.ssh.close_master();
        Ok(())
    }

    fn destroy(&self) -> Result<String> {
        self.ssh.close_master();
        anyhow::bail!(
            "rkotlin does not own ssh hosts, so it will not delete or power off yours. \
             To free its disk use `rkotlin remote gc --all --yes` (all workspaces) or `rkotlin remote rm` (this project)"
        )
    }
}
