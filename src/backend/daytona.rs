use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::io::Write;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use super::{control_dir, ssh_cache_dir, Backend, Status};
use crate::config::Instance;
use crate::transport::Ssh;
use crate::util;

/// Daytona via its REST API (`curl`, credential passed on stdin so it never shows in `ps`) and its SSH gateway.
/// Credentials: `DAYTONA_API_KEY`, or the login the `daytona` CLI already stored (`daytona login`).
pub struct Daytona {
    sb: String,
    inst: Instance,
}

struct Creds {
    bearer: String,
    org: Option<String>,
    url: Option<String>,
    /// Came from the daytona CLI's OAuth login (can be refreshed by running the CLI).
    from_cli: bool,
}

fn cli_config_candidates() -> Vec<std::path::PathBuf> {
    let home = util::home();
    let mut v = vec![];
    if let Some(x) = std::env::var_os("XDG_CONFIG_HOME").filter(|x| !x.is_empty()) {
        v.push(std::path::PathBuf::from(x).join("daytona/config.json"));
    }
    v.push(home.join(".config/daytona/config.json"));
    v.push(home.join("Library/Application Support/daytona/config.json"));
    v
}

fn creds_from_profile(cfg: &Value, org: Option<String>, url: Option<String>) -> Option<Creds> {
    let active = cfg["activeProfile"].as_str();
    let profiles = cfg["profiles"].as_array()?;
    let p = profiles
        .iter()
        .find(|p| active.is_some() && p["id"].as_str() == active)
        .or_else(|| profiles.first())?;
    let api = &p["api"];
    let key = api["key"].as_str().filter(|k| !k.is_empty());
    let token = api["token"]["accessToken"]
        .as_str()
        .filter(|k| !k.is_empty());
    let (bearer, from_cli) = match (key, token) {
        (Some(k), _) => (k, false),
        (None, Some(t)) => (t, true),
        _ => return None,
    };
    Some(Creds {
        bearer: bearer.to_string(),
        org: org.or_else(|| p["activeOrganizationId"].as_str().map(str::to_string)),
        url: url.or_else(|| api["url"].as_str().map(str::to_string)),
        from_cli,
    })
}

fn credentials(inst: &Instance) -> Result<Creds> {
    let org = std::env::var("DAYTONA_ORGANIZATION_ID")
        .ok()
        .or_else(|| inst.organization_id.clone());
    let url = std::env::var("DAYTONA_API_URL")
        .ok()
        .or_else(|| inst.api_url.clone());
    if let Ok(k) = std::env::var("DAYTONA_API_KEY") {
        return Ok(Creds {
            bearer: k,
            org,
            url,
            from_cli: false,
        });
    }
    for p in cli_config_candidates() {
        if let Some(cfg) = std::fs::read_to_string(&p)
            .ok()
            .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        {
            if let Some(c) = creds_from_profile(&cfg, org.clone(), url.clone()) {
                return Ok(c);
            }
        }
    }
    Err(crate::guidance::daytona_key_missing().into_err())
}

/// True when rkotlin can authenticate to Daytona (env key or an existing `daytona login`).
pub fn have_credentials(inst: &Instance) -> bool {
    credentials(inst).is_ok()
}

/// Any daytona CLI call that hits the API makes it refresh its stored access token.
fn refresh_cli_login() {
    let _ = Command::new("daytona")
        .args(["organization", "list"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

impl Daytona {
    pub fn new(name: &str, inst: &Instance) -> Self {
        Self {
            sb: inst.name.clone().unwrap_or_else(|| name.to_string()),
            inst: inst.clone(),
        }
    }

    fn api_url(&self, c: &Creds) -> String {
        c.url
            .clone()
            .unwrap_or_else(|| "https://app.daytona.io/api".into())
            .trim_end_matches('/')
            .to_string()
    }

    fn call(&self, method: &str, path: &str, body: Option<&Value>) -> Result<(u16, Value)> {
        let mut creds = credentials(&self.inst)?;
        let mut r = self.call_with(&creds, method, path, body)?;
        if r.0 == 401 && creds.from_cli {
            refresh_cli_login();
            creds = credentials(&self.inst)?;
            r = self.call_with(&creds, method, path, body)?;
        }
        Ok(r)
    }

    fn call_with(
        &self,
        creds: &Creds,
        method: &str,
        path: &str,
        body: Option<&Value>,
    ) -> Result<(u16, Value)> {
        let mut cfg = format!(
            "header = \"Authorization: Bearer {}\"\nheader = \"Content-Type: application/json\"\n",
            creds.bearer
        );
        if let Some(o) = &creds.org {
            cfg += &format!("header = \"X-Daytona-Organization-ID: {o}\"\n");
        }
        let mut cmd = Command::new("curl");
        cmd.args([
            "-sS",
            "-m",
            "60",
            "-K",
            "-",
            "-X",
            method,
            "-w",
            "\n%{http_code}",
        ]);
        if method == "POST" || body.is_some() {
            cmd.args([
                "--data-binary",
                &body.map(|b| b.to_string()).unwrap_or_default(),
            ]);
        }
        cmd.arg(format!("{}{}", self.api_url(creds), path));
        let mut child = cmd
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| {
                if e.kind() == std::io::ErrorKind::NotFound {
                    crate::guidance::curl_missing().into_err()
                } else {
                    anyhow::anyhow!("failed to run curl: {e}")
                }
            })?;
        child.stdin.take().unwrap().write_all(cfg.as_bytes())?;
        let out = child.wait_with_output()?;
        if !out.status.success() {
            bail!(
                "daytona API request failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        let text = String::from_utf8_lossy(&out.stdout);
        let (body_text, code) = text.rsplit_once('\n').unwrap_or(("", "0"));
        let code: u16 = code.trim().parse().unwrap_or(0);
        let v = serde_json::from_str(body_text).unwrap_or(Value::Null);
        Ok((code, v))
    }

    fn get(&self) -> Result<Option<Value>> {
        let (code, v) = self.call("GET", &format!("/sandbox/{}", self.sb), None)?;
        match code {
            200 => Ok(Some(v)),
            404 => Ok(None),
            401 | 403 => Err(crate::guidance::daytona_key_rejected(code).into_err()),
            _ => bail!(
                "daytona GET sandbox failed (HTTP {code}): {}",
                v["message"].as_str().unwrap_or("")
            ),
        }
    }

    fn create(&self) -> Result<()> {
        info!("creating Daytona sandbox `{}` (first use)", self.sb);
        let mut b = json!({
            "name": self.sb,
            "labels": {"rkotlin": "1"},
            "autoStopInterval": self.inst.auto_stop_minutes.unwrap_or(30),
        });
        for (k, v) in [
            ("cpu", self.inst.cpu),
            ("memory", self.inst.memory),
            ("disk", self.inst.disk),
        ] {
            if let Some(v) = v {
                b[k] = json!(v);
            }
        }
        if let Some(s) = &self.inst.snapshot {
            b["snapshot"] = json!(s);
        }
        let (code, v) = self.call("POST", "/sandbox", Some(&b))?;
        if !(200..300).contains(&code) {
            let msg = v["message"].as_str().unwrap_or("");
            if msg.contains("resources") && msg.contains("snapshot") {
                bail!(
                    "daytona create failed (HTTP {code}): {msg}\n\
                     Daytona applies a snapshot to every new sandbox and does not accept cpu/memory/disk next to it. \
                     Size the sandbox with a snapshot instead: remove cpu/memory/disk from this instance and set \
                     `snapshot` to daytona-small, daytona-medium or daytona-large (`daytona snapshot list` shows yours), \
                     e.g. rkotlin instances add NAME --backend daytona --set name={} --set snapshot=daytona-large",
                    self.sb
                );
            }
            bail!("daytona create failed (HTTP {code}): {msg}");
        }
        Ok(())
    }

    fn cache_file(&self) -> std::path::PathBuf {
        ssh_cache_dir().join(format!("daytona-{}.json", self.sb))
    }

    fn ssh_for(&self, token: &str, host: &str, port: u16) -> Ssh {
        let known = ssh_cache_dir().join("daytona_known_hosts");
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

    /// ssh handle from the locally cached token, if still valid. No network.
    fn cached_ssh(&self) -> Option<Ssh> {
        let v: Value =
            serde_json::from_str(&std::fs::read_to_string(self.cache_file()).ok()?).ok()?;
        if v["expires"].as_i64()? <= util::now_secs() + 600 {
            return None;
        }
        Some(self.ssh_for(
            v["token"].as_str()?,
            v["host"].as_str()?,
            v["port"].as_u64().unwrap_or(22) as u16,
        ))
    }

    fn token(&self) -> Result<(String, String, u16)> {
        let dir = ssh_cache_dir();
        std::fs::create_dir_all(&dir)?;
        let cache = dir.join(format!("daytona-{}.json", self.sb));
        if let Ok(s) = std::fs::read_to_string(&cache) {
            if let Ok(v) = serde_json::from_str::<Value>(&s) {
                if v["expires"].as_i64().unwrap_or(0) > util::now_secs() + 600 {
                    return Ok((
                        v["token"].as_str().unwrap_or_default().into(),
                        v["host"].as_str().unwrap_or_default().into(),
                        v["port"].as_u64().unwrap_or(22) as u16,
                    ));
                }
            }
        }
        let (code, v) = self.call(
            "POST",
            &format!("/sandbox/{}/ssh-access?expiresInMinutes=240", self.sb),
            None,
        )?;
        if !(200..300).contains(&code) {
            bail!(
                "daytona ssh-access failed (HTTP {code}): {}",
                v["message"].as_str().unwrap_or("")
            );
        }
        let token = v["token"]
            .as_str()
            .context("ssh-access response has no token")?
            .to_string();
        let mut host = self
            .inst
            .ssh_host
            .clone()
            .unwrap_or_else(|| "ssh.app.daytona.io".into());
        let mut port = 22u16;
        if let Some(cmd) = v["sshCommand"].as_str() {
            let parts: Vec<&str> = cmd.split_whitespace().collect();
            if let Some(i) = parts.iter().position(|p| *p == "-p") {
                port = parts.get(i + 1).and_then(|p| p.parse().ok()).unwrap_or(22);
            }
            if self.inst.ssh_host.is_none() {
                if let Some(h) = parts.iter().find_map(|p| p.split_once('@').map(|x| x.1)) {
                    host = h.to_string();
                }
            }
        }
        let rec = json!({"token": token, "host": host, "port": port, "expires": util::now_secs() + 240 * 60});
        std::fs::write(&cache, rec.to_string())?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&cache, std::fs::Permissions::from_mode(0o600));
        }
        Ok((token, host, port))
    }
}

impl Backend for Daytona {
    fn ensure(&self) -> Result<Ssh> {
        // Fast path: cached token and a live connection => zero API calls.
        if let Some(ssh) = self.cached_ssh() {
            ssh.ensure_master();
            if ssh.master_alive() {
                return Ok(ssh);
            }
        }
        let mut v = match self.get()? {
            Some(v) => v,
            None => {
                let others: Vec<String> = list_sandboxes()
                    .map(|s| {
                        s.into_iter()
                            .map(|(n, st)| format!("{n} ({st})"))
                            .filter(|n| !n.starts_with(&format!("{} (", self.sb)))
                            .collect()
                    })
                    .unwrap_or_default();
                if !others.is_empty() {
                    info!(
                        "sandbox `{}` does not exist, but you already have: {}. To reuse one instead: rkotlin instances add NAME --backend daytona --set name=<existing>",
                        self.sb,
                        others.join(", ")
                    );
                }
                self.create()?;
                self.get()?
                    .context("sandbox vanished right after creation")?
            }
        };
        let deadline = Instant::now() + Duration::from_secs(300);
        loop {
            let state = v["state"].as_str().unwrap_or("unknown").to_string();
            match state.as_str() {
                "started" => break,
                "stopped" | "archived" | "paused" => {
                    info!("starting Daytona sandbox `{}` (was {state})", self.sb);
                    let (c, r) = self.call("POST", &format!("/sandbox/{}/start", self.sb), None)?;
                    if !(200..300).contains(&c) {
                        bail!(
                            "daytona start failed (HTTP {c}): {}",
                            r["message"].as_str().unwrap_or("")
                        );
                    }
                }
                "error" | "build_failed" | "destroyed" | "destroying" => {
                    bail!("daytona sandbox `{}` is in state `{state}`", self.sb)
                }
                _ => {}
            }
            if Instant::now() > deadline {
                bail!(
                    "daytona sandbox `{}` did not start in time (last state `{state}`)",
                    self.sb
                );
            }
            std::thread::sleep(Duration::from_secs(3));
            v = self.get()?.context("sandbox disappeared")?;
        }
        let (token, host, port) = self.token()?;
        Ok(self.ssh_for(&token, &host, port))
    }

    fn status(&self) -> Result<Status> {
        match self.get()? {
            None => Ok(Status {
                state: "missing".into(),
                detail: format!(
                    "sandbox `{}` not created yet (will be on first use)",
                    self.sb
                ),
            }),
            Some(v) => Ok(Status {
                state: match v["state"].as_str().unwrap_or("unknown") {
                    "started" => "running".to_string(),
                    s => s.to_string(),
                },
                detail: format!(
                    "sandbox `{}` {} vCPU / {} GB RAM / {} GB disk",
                    self.sb,
                    v["cpu"].as_u64().unwrap_or(0),
                    v["memory"].as_u64().unwrap_or(0),
                    v["disk"].as_u64().unwrap_or(0)
                ),
            }),
        }
    }

    fn stop(&self) -> Result<()> {
        let Some(v) = self.get()? else { return Ok(()) };
        if v["state"].as_str() != Some("started") {
            return Ok(());
        }
        let (c, r) = self.call("POST", &format!("/sandbox/{}/stop", self.sb), None)?;
        if !(200..300).contains(&c) {
            bail!(
                "daytona stop failed (HTTP {c}): {}",
                r["message"].as_str().unwrap_or("")
            );
        }
        Ok(())
    }

    fn destroy(&self) -> Result<String> {
        if self.get()?.is_none() {
            return Ok(format!(
                "sandbox `{}` does not exist (nothing to delete)",
                self.sb
            ));
        }
        let (c, r) = self.call("DELETE", &format!("/sandbox/{}", self.sb), None)?;
        if !(200..300).contains(&c) {
            bail!(
                "daytona delete failed (HTTP {c}): {}",
                r["message"].as_str().unwrap_or("")
            );
        }
        let _ = std::fs::remove_file(self.cache_file());
        Ok(format!("deleted Daytona sandbox `{}`", self.sb))
    }
}

/// Every sandbox visible to the API key: (name, state).
pub fn list_sandboxes() -> Result<Vec<(String, String)>> {
    let d = Daytona::new("discover", &Instance::default());
    let (code, v) = d.call("GET", "/sandbox", None)?;
    if code != 200 {
        bail!(
            "daytona list failed (HTTP {code}): {}",
            v["message"].as_str().unwrap_or("")
        );
    }
    let items = v
        .as_array()
        .or_else(|| v["items"].as_array())
        .cloned()
        .unwrap_or_default();
    Ok(items
        .iter()
        .map(|s| {
            (
                s["name"]
                    .as_str()
                    .or_else(|| s["id"].as_str())
                    .unwrap_or("?")
                    .to_string(),
                s["state"].as_str().unwrap_or("?").to_string(),
            )
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reuses_cli_oauth_login() {
        let cfg = serde_json::json!({
            "activeProfile": "p2",
            "profiles": [
                {"id": "p1", "api": {"url": "https://one/api", "key": null, "token": {"accessToken": "tok1"}}, "activeOrganizationId": "o1"},
                {"id": "p2", "api": {"url": "https://two/api", "key": null, "token": {"accessToken": "tok2"}}, "activeOrganizationId": "o2"}
            ]
        });
        let c = creds_from_profile(&cfg, None, None).unwrap();
        assert_eq!(
            (
                c.bearer.as_str(),
                c.org.as_deref(),
                c.url.as_deref(),
                c.from_cli
            ),
            ("tok2", Some("o2"), Some("https://two/api"), true)
        );
        // explicit overrides win
        let c = creds_from_profile(&cfg, Some("mine".into()), Some("https://x".into())).unwrap();
        assert_eq!(
            (c.org.as_deref(), c.url.as_deref()),
            (Some("mine"), Some("https://x"))
        );
    }

    #[test]
    fn api_key_in_profile_beats_token_and_is_not_refreshable() {
        let cfg = serde_json::json!({"profiles": [{"id": "a", "api": {"key": "k", "token": {"accessToken": "t"}}}]});
        let c = creds_from_profile(&cfg, None, None).unwrap();
        assert_eq!((c.bearer.as_str(), c.from_cli), ("k", false));
        assert!(creds_from_profile(&serde_json::json!({"profiles": []}), None, None).is_none());
        assert!(creds_from_profile(
            &serde_json::json!({"profiles": [{"api": {"key": null}}]}),
            None,
            None
        )
        .is_none());
    }
}
