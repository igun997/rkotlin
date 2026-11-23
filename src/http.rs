//! Minimal JSON-over-HTTPS client built on `curl`. Credentials travel on curl's stdin (`-K -`)
//! so they never appear in the process list.

use anyhow::{bail, Result};
use serde_json::Value;
use std::io::Write;
use std::process::{Command, Stdio};

/// Returns (HTTP status, parsed JSON body or `null`).
pub fn request(
    base: &str,
    headers: &[(String, String)],
    method: &str,
    path: &str,
    body: Option<&Value>,
) -> Result<(u16, Value)> {
    let mut cfg = String::from("header = \"Content-Type: application/json\"\n");
    for (k, v) in headers {
        cfg += &format!("header = \"{k}: {v}\"\n");
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
    if method == "POST" || method == "PUT" || body.is_some() {
        cmd.args([
            "--data-binary",
            &body.map(|b| b.to_string()).unwrap_or_default(),
        ]);
    }
    cmd.arg(format!("{}{}", base.trim_end_matches('/'), path));
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
            "API request failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let (body_text, code) = text.rsplit_once('\n').unwrap_or(("", "0"));
    let code: u16 = code.trim().parse().unwrap_or(0);
    Ok((code, serde_json::from_str(body_text).unwrap_or(Value::Null)))
}

/// Error text from an API error body: `{"error":{"code","message"}}` or `{"message"}`.
pub fn error_text(v: &Value) -> String {
    let m = v["error"]["message"]
        .as_str()
        .or_else(|| v["message"].as_str())
        .unwrap_or("");
    match v["error"]["code"].as_str() {
        Some(c) => format!("{c}: {m}"),
        None => m.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_bodies() {
        assert_eq!(
            error_text(
                &serde_json::json!({"error":{"code":"quota_exceeded","message":"too many"}})
            ),
            "quota_exceeded: too many"
        );
        assert_eq!(error_text(&serde_json::json!({"message":"nope"})), "nope");
        assert_eq!(error_text(&Value::Null), "");
    }
}
