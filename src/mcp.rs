//! MCP server over stdio (newline-delimited JSON-RPC 2.0). Tool descriptions and server
//! instructions are generated from the user's config so an agent sees which instances exist.

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::io::{BufRead, Write};
use std::path::PathBuf;

use crate::config::{self, Loaded};
use crate::run::{self, Cmd, ExecSpec};
use crate::summary::Capture;
use crate::{backend, cmds, guidance};

const PROTOCOL: &str = "2025-06-18";

pub fn serve(default_instance: Option<String>) -> Result<()> {
    let stdin = std::io::stdin();
    let mut out = std::io::stdout();
    for line in stdin.lock().lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let resp = match serde_json::from_str::<Value>(&line) {
            Ok(msg) => handle(&msg, &default_instance),
            Err(e) => Some(
                json!({"jsonrpc":"2.0","id":null,"error":{"code":-32700,"message":format!("parse error: {e}")}}),
            ),
        };
        if let Some(r) = resp {
            writeln!(out, "{r}")?;
            out.flush()?;
        }
    }
    Ok(())
}

pub fn handle(msg: &Value, def: &Option<String>) -> Option<Value> {
    let method = msg["method"].as_str()?;
    let id = msg.get("id").cloned();
    let params = &msg["params"];
    let result: Result<Value, (i64, String)> = match method {
        "initialize" => Ok(json!({
            "protocolVersion": params["protocolVersion"].as_str().unwrap_or(PROTOCOL),
            "capabilities": {"tools": {"listChanged": false}},
            "serverInfo": {"name": "rkotlin", "version": env!("CARGO_PKG_VERSION")},
            "instructions": instructions(),
        })),
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({"tools": tools()})),
        "tools/call" => Ok(call_tool(params, def)),
        m if m.starts_with("notifications/") => return None,
        other => Err((-32601, format!("method not found: {other}"))),
    };
    let id = id?;
    Some(match result {
        Ok(r) => json!({"jsonrpc":"2.0","id":id,"result":r}),
        Err((code, message)) => {
            json!({"jsonrpc":"2.0","id":id,"error":{"code":code,"message":message}})
        }
    })
}

// ------------------------------------------------------------------ discovery text

fn snapshot() -> (Vec<String>, String) {
    let cwd = std::env::current_dir().unwrap_or_default();
    match config::load(&cwd) {
        Err(e) => (vec![], format!("Configuration error: {e:#}")),
        Ok(l) if l.cfg.instances.is_empty() => (
            vec![],
            "No instances are configured yet. Call the `setup` tool: it returns the exact steps (some need the user).".into(),
        ),
        Ok(l) => {
            let rows = l.rows();
            let mut s = format!("Configured instances ({}):\n", rows.len());
            for r in &rows {
                s += &format!(
                    "- {}{} ({}){}\n",
                    r.name,
                    if r.is_default { " [default]" } else { "" },
                    r.inst.backend,
                    r.inst.description.as_deref().map(|d| format!(": {d}")).unwrap_or_default()
                );
            }
            if l.default_name().is_none() {
                s += "No default: you MUST pass `instance` on every call (pick by description, or ask the user).\n";
            }
            (rows.into_iter().map(|r| r.name).collect(), s)
        }
    }
}

fn instructions() -> String {
    let (_, blurb) = snapshot();
    format!(
        "rkotlin runs Gradle (Kotlin, Android and JVM builds, tests, lint) on remote machines so the user's own computer stays free.\n\
         ALWAYS use the `gradle` tool instead of running gradle or ./gradlew locally: a local build can freeze a small laptop.\n\
         Output is compact: compiler errors first, failing tests next, progress noise removed.\n\n\
         {blurb}\n\
         Choosing an instance: omit `instance` to use the default. With several instances, choose by description \
         (e.g. a bigger machine for release builds), and call the `instances` tool to see live state. \
         Never create, delete or reconfigure instances yourself; ask the user.\n\n\
         If any tool returns SETUP REQUIRED, follow its steps: run the ones not marked [USER] yourself; for [USER] steps show the \
         user the exact command or URL and wait. Then call the `setup` tool to re-check. Never invent credentials or ask for secrets in chat."
    )
}

fn instance_prop(names: &[String]) -> Value {
    let mut p = json!({"type":"string","description":"Remote instance to use. Omit for the default. Names and descriptions are in the server instructions and the `instances` tool."});
    if !names.is_empty() {
        p["enum"] = json!(names);
    }
    p
}

fn tools() -> Vec<Value> {
    let (names, blurb) = snapshot();
    let inst = instance_prop(&names);
    let project_dir = json!({"type":"string","description":"Directory of the Gradle project. Default: the server's working directory."});
    let max = json!({"type":"integer","minimum":2000,"maximum":200000,"description":"Cap on returned characters (default 30000)."});
    vec![
        json!({
            "name": "gradle",
            "description": format!("Run a Gradle command on a remote build machine (never locally). The project directory, including uncommitted and private code and includeBuild() composite builds, is synced first; persistent remote Gradle caches and build/ directories keep rebuilds fast. Uses ./gradlew when the project has one. Returns compiler errors first, then failing tests, with progress noise removed. Examples: args=[\"build\"], [\":app:assembleDebug\"], [\"test\",\"--tests\",\"*FooTest\"], [\"lintDebug\"], [\"tasks\"].\n\n{blurb}"),
            "inputSchema": {"type":"object","required":["args"],"properties":{
                "args": {"type":"array","items":{"type":"string"},"minItems":1,"description":"Arguments after `gradle` / `./gradlew`."},
                "instance": inst, "project_dir": project_dir,
                "pull": {"type":"boolean","description":"After a successful run, copy build outputs (apk, aab, jar, distributions) into the local build/ directories. Installing on the user's phone is not available here: tell the user to run `rkotlin --install ...` themselves."},
                "max_output_chars": max
            }}
        }),
        json!({
            "name": "shell",
            "description": "Run a shell command inside the synced project directory on the remote machine (e.g. `ls app/build/outputs/apk`, `cat app/build/reports/lint-results-debug.txt`, `du -sh build`). Not for building: use `gradle` for that.",
            "inputSchema": {"type":"object","required":["command"],"properties":{
                "command": {"type":"string"}, "instance": inst, "project_dir": project_dir, "max_output_chars": max
            }}
        }),
        json!({
            "name": "sync",
            "description": "Upload local changes to the remote workspace without running anything.",
            "inputSchema": {"type":"object","properties":{"instance": inst, "project_dir": project_dir}}
        }),
        json!({
            "name": "instances",
            "description": "List the configured remote instances with description, backend and live state (running/stopped/missing). Use it to choose an instance when several exist.",
            "inputSchema": {"type":"object","properties":{"live":{"type":"boolean","description":"Query providers for live state (default true)."}}}
        }),
        json!({
            "name": "setup",
            "description": "Check readiness (ssh, instance, credentials, remote toolchain) and return SETUP REQUIRED steps for whatever is missing. Call it when something fails or before the first build. Steps marked [USER] need the human.",
            "inputSchema": {"type":"object","properties":{"instance": inst, "live":{"type":"boolean","description":"Also contact the provider and the instance (default true; never creates anything)."}}}
        }),
        json!({
            "name": "power",
            "description": "Start (`up`) or pause (`down`) an instance. Builds start it automatically; use `down` when the work session is over to save the user's money.",
            "inputSchema": {"type":"object","required":["action"],"properties":{"action":{"type":"string","enum":["up","down"]},"instance": inst}}
        }),
        json!({
            "name": "workspaces",
            "description": "List the project workspaces stored on an instance with their disk usage.",
            "inputSchema": {"type":"object","properties":{"instance": inst}}
        }),
    ]
}

// ------------------------------------------------------------------ tool calls

struct Out {
    text: String,
    error: bool,
    structured: Option<Value>,
}

impl Out {
    fn ok(text: String) -> Self {
        Out {
            text,
            error: false,
            structured: None,
        }
    }
}

fn call_tool(params: &Value, def: &Option<String>) -> Value {
    let name = params["name"].as_str().unwrap_or("");
    let args = &params["arguments"];
    let r = match name {
        "gradle" => tool_gradle(args, def),
        "shell" => tool_shell(args, def),
        "sync" => tool_sync(args, def),
        "instances" => tool_instances(args),
        "setup" => tool_setup(args, def),
        "power" => tool_power(args, def),
        "workspaces" => tool_workspaces(args, def),
        other => Err(anyhow::anyhow!("unknown tool `{other}`")),
    };
    let out = match r {
        Ok(o) => o,
        Err(e) => match guidance::find(&e) {
            Some(sr) => Out {
                text: sr.render(),
                error: true,
                structured: Some(sr.to_json()),
            },
            None => Out {
                text: format!("error: {e:#}"),
                error: true,
                structured: None,
            },
        },
    };
    let mut v = json!({"content":[{"type":"text","text":out.text}],"isError":out.error});
    if let Some(s) = out.structured {
        v["structuredContent"] = s;
    }
    v
}

fn inst_arg(args: &Value, def: &Option<String>) -> Option<String> {
    args["instance"]
        .as_str()
        .map(str::to_string)
        .or_else(|| def.clone())
}

fn project_dir(args: &Value) -> Result<PathBuf> {
    match args["project_dir"].as_str() {
        Some(p) => {
            let p = PathBuf::from(p);
            if !p.is_dir() {
                bail!("project_dir `{}` is not a directory", p.display());
            }
            Ok(p)
        }
        None => std::env::current_dir().context("no working directory"),
    }
}

fn max_chars(args: &Value) -> usize {
    args["max_output_chars"]
        .as_u64()
        .unwrap_or(30_000)
        .clamp(2_000, 200_000) as usize
}

fn open(args: &Value, def: &Option<String>) -> Result<run::Session> {
    let cwd = project_dir(args)?;
    let loaded = config::load(&cwd)?;
    run::open(&loaded, inst_arg(args, def).as_deref(), &cwd)
}

fn header(sess: &run::Session, res: &run::ExecResult) -> String {
    format!(
        "instance: {} ({}) | {} | {:.1}s total, sync {} of {} files ({}) in {:.1}s\n",
        sess.inst_name,
        sess.inst.backend,
        if res.code == 0 {
            "SUCCESS".to_string()
        } else {
            format!("FAILED (exit {})", res.code)
        },
        res.total_secs,
        res.sync.files_sent,
        res.sync.files_total,
        crate::util::human_bytes(res.sync.bytes_sent),
        res.sync_secs
    )
}

fn tool_gradle(args: &Value, def: &Option<String>) -> Result<Out> {
    let list: Vec<String> = args["args"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    if list.is_empty() {
        bail!("`args` must be a non-empty array of strings, e.g. [\"build\"]");
    }
    let sess = open(args, def)?;
    let mut cap = Capture::default();
    let res = sess.exec(
        &ExecSpec {
            cmd: Cmd::Gradle(list),
            sync: true,
        },
        &mut |s, l| cap.feed(s, l),
    )?;
    let mut text = header(&sess, &res);
    text += &cap.render(res.code, max_chars(args));
    if args["pull"].as_bool().unwrap_or(false) && res.code == 0 {
        match sess.pull(false) {
            Ok(f) if f.is_empty() => text += "\n(pull: no build outputs were produced)",
            Ok(f) => {
                text += &format!(
                    "\n(pulled {} output file(s) into build/ directories)",
                    f.len()
                )
            }
            Err(e) => text += &format!("\n(pull failed: {e:#})"),
        }
    }
    Ok(Out {
        text,
        error: res.code != 0,
        structured: None,
    })
}

fn tool_shell(args: &Value, def: &Option<String>) -> Result<Out> {
    let command = args["command"]
        .as_str()
        .context("`command` is required")?
        .to_string();
    let sess = open(args, def)?;
    let mut cap = Capture::default();
    let res = sess.exec(
        &ExecSpec {
            cmd: Cmd::Shell(command),
            sync: true,
        },
        &mut |s, l| cap.feed(s, l),
    )?;
    let mut text = header(&sess, &res);
    text += &cap.raw(max_chars(args));
    Ok(Out {
        text,
        error: res.code != 0,
        structured: None,
    })
}

fn tool_sync(args: &Value, def: &Option<String>) -> Result<Out> {
    let sess = open(args, def)?;
    let res = sess.sync_only()?;
    Ok(Out::ok(header(&sess, &res)))
}

fn tool_instances(args: &Value) -> Result<Out> {
    let cwd = std::env::current_dir()?;
    let loaded: Loaded = config::load(&cwd)?;
    if loaded.cfg.instances.is_empty() {
        return Err(guidance::no_instances().into_err());
    }
    let live = args["live"].as_bool().unwrap_or(true);
    let rows = if live {
        cmds::live_status(&loaded)
    } else {
        loaded
            .rows()
            .into_iter()
            .map(|r| {
                (
                    r,
                    cmds::Live {
                        state: "unknown".into(),
                        detail: String::new(),
                    },
                )
            })
            .collect()
    };
    let structured: Vec<Value> = rows
        .iter()
        .map(|(r, l)| json!({"name": r.name, "backend": r.inst.backend, "default": r.is_default, "description": r.inst.description, "state": l.state, "detail": l.detail}))
        .collect();
    let mut text = cmds::render_instances(&rows);
    if let Some(d) = loaded.default_name() {
        text += &format!("\ndefault instance: {d}\n");
    }
    Ok(Out {
        text,
        error: false,
        structured: Some(json!({"instances": structured})),
    })
}

fn tool_setup(args: &Value, def: &Option<String>) -> Result<Out> {
    let cwd = std::env::current_dir()?;
    let loaded = config::load(&cwd)?;
    let rep = guidance::onboard(
        &loaded,
        inst_arg(args, def).as_deref(),
        args["live"].as_bool().unwrap_or(true),
    );
    Ok(Out {
        text: rep.render(),
        error: !rep.ready(),
        structured: Some(rep.to_json()),
    })
}

fn tool_power(args: &Value, def: &Option<String>) -> Result<Out> {
    let cwd = std::env::current_dir()?;
    let loaded = config::load(&cwd)?;
    let (name, inst) = loaded.resolve(inst_arg(args, def).as_deref())?;
    let b = backend::make(&name, &inst)?;
    match args["action"].as_str() {
        Some("up") => {
            let ssh = b.ensure()?;
            ssh.ensure_master();
            let st = b.status()?;
            Ok(Out::ok(format!("`{name}` is {} ({})", st.state, st.detail)))
        }
        Some("down") => {
            b.stop()?;
            Ok(Out::ok(format!(
                "`{name}` paused. It resumes automatically on the next build."
            )))
        }
        _ => bail!("`action` must be \"up\" or \"down\""),
    }
}

fn tool_workspaces(args: &Value, def: &Option<String>) -> Result<Out> {
    let cwd = std::env::current_dir()?;
    let loaded = config::load(&cwd)?;
    let (name, inst) = loaded.resolve(inst_arg(args, def).as_deref())?;
    let ssh = backend::make(&name, &inst)?.ensure()?;
    let out = ssh.capture_ok(&run::workspaces_script(inst.base.as_deref()))?;
    let ws = run::parse_workspaces(&out);
    if ws.is_empty() {
        return Ok(Out::ok(format!("No workspaces on `{name}` yet.")));
    }
    let mut text = format!("Workspaces on `{name}`:\n");
    for w in ws {
        text += &format!(
            "- {} {} (id {})\n",
            crate::util::human_bytes(w.kib * 1024),
            w.origin,
            w.id
        );
    }
    Ok(Out::ok(text))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(method: &str, params: Value) -> Value {
        json!({"jsonrpc":"2.0","id":1,"method":method,"params":params})
    }

    #[test]
    fn initialize_and_notifications() {
        let r = handle(
            &req("initialize", json!({"protocolVersion":"2025-03-26"})),
            &None,
        )
        .unwrap();
        assert_eq!(r["result"]["protocolVersion"], "2025-03-26");
        assert_eq!(r["result"]["serverInfo"]["name"], "rkotlin");
        assert!(r["result"]["instructions"]
            .as_str()
            .unwrap()
            .contains("ALWAYS use the `gradle` tool"));
        assert!(handle(
            &json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
            &None
        )
        .is_none());
        let e = handle(&req("nope", json!({})), &None).unwrap();
        assert_eq!(e["error"]["code"], -32601);
    }

    #[test]
    fn tool_list_is_complete_and_well_formed() {
        let r = handle(&req("tools/list", json!({})), &None).unwrap();
        let tools = r["result"]["tools"].as_array().unwrap();
        let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
        for n in [
            "gradle",
            "shell",
            "sync",
            "instances",
            "setup",
            "power",
            "workspaces",
        ] {
            assert!(names.contains(&n), "missing tool {n}");
        }
        for t in tools {
            assert_eq!(t["inputSchema"]["type"], "object");
            assert!(t["description"].as_str().unwrap().len() > 20);
        }
    }

    #[test]
    fn bad_arguments_are_tool_errors_not_protocol_errors() {
        let r = handle(
            &req(
                "tools/call",
                json!({"name":"gradle","arguments":{"args":[]}}),
            ),
            &None,
        )
        .unwrap();
        assert_eq!(r["result"]["isError"], true);
        assert!(r["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("non-empty"));
        let r = handle(
            &req("tools/call", json!({"name":"bogus","arguments":{}})),
            &None,
        )
        .unwrap();
        assert_eq!(r["result"]["isError"], true);
    }
}
