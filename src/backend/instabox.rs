//! instabox backend. Two interchangeable ways to talk to it:
//!
//! * `INSTABOX_API_KEY` set: REST API directly (no `instabox` CLI needed, no CLI start-up cost).
//! * otherwise: the signed-in `instabox` CLI.
//!
//! SSH access works either way, and both go through a cached connection so a warm run makes
//! no provider calls at all.

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use super::{control_dir, ssh_cache_dir, Backend, Status};
use crate::config::Instance;
use crate::http;
use crate::transport::Ssh;
use crate::util;

struct Api {
    base: String,
    key: String,
}

fn api_from_env() -> Option<Api> {
    let key = std::env::var("INSTABOX_API_KEY")
        .ok()
        .filter(|k| !k.is_empty())?;
    let base = std::env::var("INSTABOX_API_URL")
        .or_else(|_| std::env::var("INSTABOX_SERVER"))
        .unwrap_or_else(|_| "https://api.instabox.sh".into());
    Some(Api { base, key })
}

/// True when rkotlin can use the REST API (so the instabox CLI is not required).
pub fn using_api_key() -> bool {
    api_from_env().is_some()
}

static IDEM: AtomicU64 = AtomicU64::new(0);

fn idem_key() -> String {
    let n = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!(
        "rkotlin-{n:x}-{}-{}",
        std::process::id(),
        IDEM.fetch_add(1, Ordering::Relaxed)
    )
}

impl Api {
    fn call(
        &self,
        method: &str,
        path: &str,
        body: Option<&Value>,
        extra: &[(&str, String)],
    ) -> Result<(u16, Value)> {
        let mut h = vec![("X-API-Key".to_string(), self.key.clone())];
        if method != "GET" {
            h.push(("Idempotency-Key".into(), idem_key()));
        }
        h.extend(extra.iter().map(|(k, v)| (k.to_string(), v.clone())));
        let (code, v) = http::request(&self.base, &h, method, path, body)?;
        if code == 401 || code == 403 {
            return Err(crate::guidance::instabox_key_rejected(code).into_err());
        }
        Ok((code, v))
    }

    fn ok(
        &self,
        method: &str,
        path: &str,
        body: Option<&Value>,
        extra: &[(&str, String)],
    ) -> Result<Value> {
        let (code, v) = self.call(method, path, body, extra)?;
        if !(200..300).contains(&code) {
            bail!(
                "instabox {method} {path} failed (HTTP {code}): {}",
                http::error_text(&v)
            );
        }
        Ok(v)
    }
}

pub struct Instabox {
    name: String,
    box_name: String,
    inst: Instance,
    api: Option<Api>,
}

fn cli(args: &[&str]) -> Result<(i32, String, String)> {
    let out = Command::new("instabox")
        .args(args)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                crate::guidance::instabox_missing().into_err()
            } else {
                anyhow::anyhow!("failed to run instabox: {e}")
            }
        })?;
    if out.status.code() == Some(3) {
        return Err(crate::guidance::instabox_login().into_err());
    }
    Ok((
        out.status.code().unwrap_or(1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    ))
}

fn spec_of(b: &Value) -> String {
    format!(
        "{} vCPU / {} MiB",
        b["spec"]["vcpus"].as_u64().unwrap_or(0),
        b["spec"]["memMiB"].as_u64().unwrap_or(0)
    )
}

/// Every box on the account: (name, state, "N vCPU / M MiB").
pub fn list_boxes() -> Result<Vec<(String, String, String)>> {
    let v: Value = match api_from_env() {
        Some(api) => api.ok("GET", "/v1/machines", None, &[])?,
        None => {
            let (code, out, err) = cli(&["ls", "--json"])?;
            if code != 0 {
                bail!("`instabox ls` failed ({code}): {}", err.trim());
            }
            serde_json::from_str(&out).context("parsing `instabox ls` output")?
        }
    };
    let items = v
        .as_array()
        .or_else(|| v["items"].as_array())
        .or_else(|| v["machines"].as_array())
        .cloned()
        .unwrap_or_default();
    Ok(items
        .iter()
        .map(|b| {
            (
                b["name"].as_str().unwrap_or("?").to_string(),
                b["state"].as_str().unwrap_or("?").to_string(),
                spec_of(b),
            )
        })
        .collect())
}

impl Instabox {
    pub fn new(name: &str, inst: &Instance) -> Self {
        Self {
            name: name.to_string(),
            box_name: inst.box_name.clone().unwrap_or_else(|| name.to_string()),
            inst: inst.clone(),
            api: api_from_env(),
        }
    }

    /// Names of the account's other boxes, so a first use can suggest reusing one.
    fn other_boxes(&self) -> Vec<String> {
        list_boxes()
            .map(|b| {
                b.into_iter()
                    .map(|(n, _, _)| n)
                    .filter(|n| *n != self.box_name)
                    .collect()
            })
            .unwrap_or_default()
    }

    fn get(&self) -> Result<Option<Value>> {
        if let Some(api) = &self.api {
            let (code, v) =
                api.call("GET", &format!("/v1/machines/{}", self.box_name), None, &[])?;
            return match code {
                200 => Ok(Some(v)),
                404 => Ok(None),
                _ => bail!(
                    "instabox GET machine failed (HTTP {code}): {}",
                    http::error_text(&v)
                ),
            };
        }
        let (code, out, err) = cli(&["get", &self.box_name, "--json"])?;
        match code {
            0 => Ok(Some(
                serde_json::from_str(&out).context("parsing `instabox get` output")?,
            )),
            4 => Ok(None),
            _ => bail!(
                "`instabox get {}` failed ({code}): {}",
                self.box_name,
                err.trim()
            ),
        }
    }

    fn create(&self) -> Result<()> {
        if self.inst.auto_create == Some(false) {
            bail!(
                "instabox box `{}` does not exist and auto_create=false for instance `{}`",
                self.box_name,
                self.name
            );
        }
        info!(
            "creating instabox box `{}` (first use; this is a one-time cost)",
            self.box_name
        );
        let sized = match (self.inst.vcpus, self.inst.mem_mib) {
            (Some(v), Some(m)) => Some((v, m)),
            (None, None) => None,
            _ => bail!(
                "instance `{}`: set both `vcpus` and `mem_mib`, or neither",
                self.name
            ),
        };
        if let Some(api) = &self.api {
            let mut b = json!({"name": self.box_name});
            if let Some((v, m)) = sized {
                b["vcpus"] = json!(v);
                b["memMiB"] = json!(m);
            }
            if let Some(t) = &self.inst.template {
                b["templateId"] = json!(t);
            }
            api.ok("POST", "/v1/machines", Some(&b), &[])?;
            return Ok(());
        }
        let mut args: Vec<String> = vec![
            "create".into(),
            "--name".into(),
            self.box_name.clone(),
            "--json".into(),
        ];
        if let Some((v, m)) = sized {
            args.extend([
                "--vcpus".into(),
                v.to_string(),
                "--mem-mib".into(),
                m.to_string(),
            ]);
        }
        if let Some(t) = &self.inst.template {
            args.extend(["--template".into(), t.clone()]);
        }
        let refs: Vec<&str> = args.iter().map(String::as_str).collect();
        let (code, _, err) = cli(&refs)?;
        if code != 0 {
            bail!("`instabox create` failed ({code}): {}", err.trim());
        }
        Ok(())
    }

    /// verb: start | resume | suspend | stop
    fn lifecycle(&self, machine: &Value, verb: &str) -> Result<()> {
        if let Some(api) = &self.api {
            let id = machine["id"].as_str().context("machine has no id")?;
            api.ok(
                "POST",
                &format!("/v1/machines/{id}/{verb}"),
                Some(&json!({})),
                &[],
            )?;
            return Ok(());
        }
        let (c, _, e) = cli(&[verb, &self.box_name])?;
        if c != 0 {
            bail!("`instabox {verb}` failed: {}", e.trim());
        }
        Ok(())
    }

    /// The API is eventually consistent right after create/delete: wait until the box is visible.
    fn get_wait(&self, secs: u64) -> Result<Value> {
        let end = Instant::now() + Duration::from_secs(secs);
        loop {
            if let Some(v) = self.get()? {
                return Ok(v);
            }
            if Instant::now() > end {
                bail!(
                    "box `{}` did not appear within {secs}s after creation",
                    self.box_name
                );
            }
            std::thread::sleep(Duration::from_secs(1));
        }
    }

    fn state_of(v: &Value) -> String {
        v["state"].as_str().unwrap_or("unknown").to_string()
    }

    fn local_files(&self) -> (std::path::PathBuf, std::path::PathBuf) {
        let dir = ssh_cache_dir();
        (
            dir.join(format!("instabox-{}.conf", self.box_name)),
            dir.join(format!("instabox-{}.alias", self.box_name)),
        )
    }

    fn token_file(&self) -> std::path::PathBuf {
        ssh_cache_dir().join(format!("instabox-{}.token.json", self.box_name))
    }

    /// An ssh handle built purely from local cache, if it is still valid. No network.
    fn cached_ssh(&self) -> Option<Ssh> {
        if self.api.is_some() {
            let v: Value =
                serde_json::from_str(&std::fs::read_to_string(self.token_file()).ok()?).ok()?;
            if v["expires"].as_i64()? <= util::now_secs() + 600 {
                return None;
            }
            return Some(self.token_ssh(
                v["token"].as_str()?,
                v["host"].as_str()?,
                v["port"].as_u64().unwrap_or(22) as u16,
            ));
        }
        let (conf, alias_file) = self.local_files();
        let fresh = std::fs::metadata(&conf)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|age| age < Duration::from_secs(25 * 60));
        if !fresh {
            return None;
        }
        let alias = std::fs::read_to_string(alias_file).ok()?;
        Some(Ssh {
            args: vec![
                "-F".into(),
                conf.to_string_lossy().into_owned(),
                alias.trim().to_string(),
            ],
            control_dir: control_dir(),
        })
    }

    fn token_ssh(&self, token: &str, host: &str, port: u16) -> Ssh {
        let known = ssh_cache_dir().join("instabox_known_hosts");
        Ssh {
            args: vec![
                "-o".into(),
                "StrictHostKeyChecking=accept-new".into(),
                "-o".into(),
                format!("UserKnownHostsFile={}", known.display()),
                "-p".into(),
                port.to_string(),
                format!("{token}@{host}"),
            ],
            control_dir: control_dir(),
        }
    }

    /// Fresh ssh access: an API-issued token, or the CLI's short-lived certificate config.
    fn ssh_access(&self, machine: &Value) -> Result<Ssh> {
        std::fs::create_dir_all(ssh_cache_dir())?;
        if let Some(api) = &self.api {
            let id = machine["id"].as_str().context("machine has no id")?;
            const TTL: i64 = 24 * 3600;
            let v = api.ok(
                "POST",
                &format!("/v1/machines/{id}/ssh-tokens"),
                Some(&json!({"name": "rkotlin", "ttlSeconds": TTL})),
                &[],
            )?;
            let token = v["token"]
                .as_str()
                .context("ssh-token response has no token")?;
            let host = v["host"]
                .as_str()
                .context("ssh-token response has no host")?;
            let port = v["port"].as_u64().unwrap_or(22) as u16;
            let rec = json!({"token": token, "host": host, "port": port, "expires": util::now_secs() + TTL});
            let f = self.token_file();
            std::fs::write(&f, rec.to_string())?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o600));
            }
            return Ok(self.token_ssh(token, host, port));
        }
        let _ = cli(&["ssh-cert", &self.box_name]);
        let (code, out, err) = cli(&["ssh-config", &self.box_name, "--json"])?;
        if code != 0 {
            bail!("`instabox ssh-config` failed ({code}): {}", err.trim());
        }
        let v: Value =
            serde_json::from_str(&out).context("parsing `instabox ssh-config` output")?;
        let text = v["config"]
            .as_str()
            .context("ssh-config JSON has no `config`")?;
        let alias = v["host"]
            .as_str()
            .context("ssh-config JSON has no `host`")?;
        let (conf, alias_file) = self.local_files();
        std::fs::write(&conf, text)?;
        std::fs::write(&alias_file, alias)?;
        self.cached_ssh()
            .context("could not read back the ssh config")
    }

    fn forget_local(&self) {
        let (conf, alias) = self.local_files();
        for f in [conf, alias, self.token_file()] {
            let _ = std::fs::remove_file(f);
        }
    }
}

impl Backend for Instabox {
    fn ensure(&self) -> Result<Ssh> {
        // Fast path: cached credentials and a live connection => zero provider calls.
        if let Some(ssh) = self.cached_ssh() {
            ssh.ensure_master();
            if ssh.master_alive() {
                return Ok(ssh);
            }
        }
        let mut v = match self.get()? {
            Some(v) => v,
            None => {
                let others = self.other_boxes();
                if !others.is_empty() {
                    info!(
                        "box `{}` does not exist, but you already have: {}. To reuse one instead: rkotlin instances add NAME --backend instabox --set box=<existing>",
                        self.box_name,
                        others.join(", ")
                    );
                }
                if others.is_empty() {
                    self.create()?;
                } else {
                    self.create().with_context(|| {
                        format!(
                            "existing boxes you could reuse: {} (rkotlin instances add NAME --backend instabox --set box=<existing>)",
                            others.join(", ")
                        )
                    })?;
                }
                self.get_wait(90)?
            }
        };
        let deadline = Instant::now() + Duration::from_secs(240);
        loop {
            let state = Self::state_of(&v);
            match state.as_str() {
                "running" => break,
                s if s.contains("suspend") || s.contains("pause") => {
                    info!("resuming `{}`", self.box_name);
                    self.lifecycle(&v, "resume")?;
                }
                "stopped" => {
                    info!("starting `{}`", self.box_name);
                    self.lifecycle(&v, "start")?;
                }
                "error" | "failed" => bail!(
                    "box `{}` is in state `{state}`; see `instabox logs {}`",
                    self.box_name,
                    self.box_name
                ),
                _ => std::thread::sleep(Duration::from_secs(3)),
            }
            if Instant::now() > deadline {
                bail!(
                    "box `{}` did not reach `running` in time (last state `{state}`)",
                    self.box_name
                );
            }
            std::thread::sleep(Duration::from_secs(1));
            v = self.get_wait(90)?;
        }
        self.ssh_access(&v)
    }

    fn status(&self) -> Result<Status> {
        let via = if self.api.is_some() {
            "api key"
        } else {
            "instabox CLI"
        };
        match self.get()? {
            None => Ok(Status {
                state: "missing".into(),
                detail: format!(
                    "box `{}` not created yet (will be on first use) [{via}]",
                    self.box_name
                ),
            }),
            Some(v) => Ok(Status {
                state: Self::state_of(&v),
                detail: format!("box `{}` {} [{via}]", self.box_name, spec_of(&v)),
            }),
        }
    }

    fn stop(&self) -> Result<()> {
        let Some(v) = self.get()? else { return Ok(()) };
        if Self::state_of(&v) != "running" {
            return Ok(());
        }
        if self.lifecycle(&v, "suspend").is_err() {
            self.lifecycle(&v, "stop")?;
        }
        Ok(())
    }

    fn destroy(&self) -> Result<String> {
        let Some(v) = self.get()? else {
            return Ok(format!(
                "box `{}` does not exist (nothing to delete)",
                self.box_name
            ));
        };
        if let Some(api) = &self.api {
            let id = v["id"].as_str().context("machine has no id")?;
            api.ok(
                "DELETE",
                &format!("/v1/machines/{id}"),
                None,
                &[("X-Confirm-Delete", id.to_string())],
            )?;
        } else {
            let (c, _, e) = cli(&["rm", "--yes", &self.box_name])?;
            if c != 0 {
                bail!("`instabox rm` failed ({c}): {}", e.trim());
            }
        }
        // wait for the delete to land so an immediate re-create does not race a zombie
        let end = Instant::now() + Duration::from_secs(90);
        while self.get()?.is_some() && Instant::now() < end {
            std::thread::sleep(Duration::from_secs(1));
        }
        self.forget_local();
        Ok(format!("deleted instabox box `{}`", self.box_name))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn idempotency_keys_are_unique() {
        let a = idem_key();
        let b = idem_key();
        assert_ne!(a, b);
        assert!(a.starts_with("rkotlin-"));
    }

    #[test]
    fn spec_formatting() {
        assert_eq!(
            spec_of(&json!({"spec": {"vcpus": 4, "memMiB": 8192}})),
            "4 vCPU / 8192 MiB"
        );
        assert_eq!(spec_of(&json!({})), "0 vCPU / 0 MiB");
    }
}
