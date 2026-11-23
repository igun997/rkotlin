//! Onboarding guidance. Whenever a prerequisite (an instance, credentials, a CLI, a toolchain)
//! is missing, rkotlin fails with a `SetupRequired` that lists exact next steps. The same text is
//! printed by the CLI and returned (plus as structured JSON) by the MCP server, so a human
//! or an AI agent can fix the problem without guessing.

use serde_json::{json, Value};
use std::fmt;

use crate::config::{Instance, Loaded};
use crate::{backend, transport};

#[derive(Debug, Clone)]
pub struct Step {
    pub title: String,
    pub run: Vec<String>,
    /// Needs the human (browser login, secrets, dashboard). An agent must relay it and wait.
    pub human: bool,
    pub note: Option<String>,
}

impl Step {
    pub fn new(title: &str) -> Self {
        Step {
            title: title.into(),
            run: vec![],
            human: false,
            note: None,
        }
    }
    pub fn run(mut self, c: &str) -> Self {
        self.run.push(c.into());
        self
    }
    pub fn human(mut self) -> Self {
        self.human = true;
        self
    }
    pub fn note(mut self, n: &str) -> Self {
        self.note = Some(n.into());
        self
    }
}

#[derive(Debug, Clone)]
pub struct SetupRequired {
    pub problem: String,
    /// true: the steps are alternatives, pick one. false: do them in order.
    pub choose_one: bool,
    pub steps: Vec<Step>,
    pub then: Option<String>,
}

impl fmt::Display for SetupRequired {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "setup required: {}", self.problem)
    }
}

impl std::error::Error for SetupRequired {}

impl SetupRequired {
    pub fn into_err(self) -> anyhow::Error {
        anyhow::Error::new(self)
    }

    pub fn render(&self) -> String {
        let mut s = format!("SETUP REQUIRED: {}\n\n", self.problem);
        s += if self.choose_one {
            "Pick ONE of these options ([USER] = needs the human, e.g. a browser login or a secret):\n"
        } else {
            "Do these in order ([USER] = needs the human, e.g. a browser login or a secret):\n"
        };
        for (i, st) in self.steps.iter().enumerate() {
            let tag = if st.human { " [USER]" } else { "" };
            let n = if self.choose_one {
                ((b'A' + i as u8) as char).to_string()
            } else {
                (i + 1).to_string()
            };
            s += &format!("\n  {n}.{tag} {}\n", st.title);
            for c in &st.run {
                s += &format!("       $ {c}\n");
            }
            if let Some(note) = &st.note {
                for l in note.lines() {
                    s += &format!("       {l}\n");
                }
            }
        }
        if let Some(t) = &self.then {
            s += &format!("\nThen: {t}\n");
        }
        s += "\nFor AI agents: run the steps that are not marked [USER] yourself. For [USER] steps, show the user the exact command or URL, \
              then wait for them to confirm. Never invent or guess credentials and never ask for secrets to be pasted into the chat.\n";
        s
    }

    pub fn to_json(&self) -> Value {
        json!({
            "setup_required": true,
            "problem": self.problem,
            "choose_one": self.choose_one,
            "steps": self.steps.iter().map(|s| json!({
                "title": s.title, "run": s.run, "needs_user": s.human, "note": s.note
            })).collect::<Vec<_>>(),
            "then": self.then,
        })
    }
}

pub fn find(e: &anyhow::Error) -> Option<&SetupRequired> {
    e.downcast_ref::<SetupRequired>()
}

// ------------------------------------------------------------------ catalogue

pub fn no_instances() -> SetupRequired {
    SetupRequired {
        problem: "no remote instance is configured, so there is nowhere to build".into(),
        choose_one: true,
        steps: vec![
            Step::new("instabox cloud box (simplest; has a free tier)")
                .run("npm install -g instabox")
                .run("instabox login")
                .run("rkotlin instances add main --backend instabox --default --description \"general purpose Kotlin/Gradle builder\"")
                .human()
                .note("`instabox login` shows a page and a code the user opens in a browser.\nHeadless agent: run `instabox login --agent --name rkotlin`, relay the printed claim URL and code to the user, and wait."),
            Step::new("Daytona sandbox")
                .run("daytona login      # or: export DAYTONA_API_KEY=<key from https://app.daytona.io/dashboard/keys>")
                .run("rkotlin instances add dt --backend daytona --set snapshot=daytona-large --default --description \"Daytona sandbox, 4 vCPU / 8 GB\"")
                .run("rkotlin remote setup")
                .human()
                .note("The key must be in the environment of whatever launches rkotlin (for MCP: the client's `env` for the rkotlin server)."),
            Step::new("Your own server or VPS reachable by ssh")
                .run("rkotlin instances add vps --backend ssh --set host=user@your-server --default --description \"my 32-core build server\"")
                .run("rkotlin remote setup")
                .human()
                .note("`ssh user@your-server true` must succeed without a password prompt (use a key or ssh-agent)."),
        ],
        then: Some("run `rkotlin remote doctor` to verify, then retry the original command".into()),
    }
}

pub fn ambiguous(instances: &std::collections::BTreeMap<String, Instance>) -> SetupRequired {
    let list: Vec<String> = instances
        .iter()
        .map(|(n, i)| {
            format!(
                "{n} ({}){}",
                i.backend,
                i.description
                    .as_deref()
                    .map(|d| format!(": {d}"))
                    .unwrap_or_default()
            )
        })
        .collect();
    SetupRequired {
        problem: format!(
            "several instances are configured and none is the default: {}",
            instances.keys().cloned().collect::<Vec<_>>().join(", ")
        ),
        choose_one: true,
        steps: vec![
            Step::new("Choose per command")
                .run("rkotlin -i NAME test")
                .note(&format!(
                    "Instances:\n{}\nMCP/agents: pass the `instance` argument of the tool.",
                    list.join("\n")
                )),
            Step::new("Make one the default for this user").run("rkotlin instances use NAME"),
            Step::new("Pin one for this project")
                .note("add `instance = \"NAME\"` to rkotlin.toml in the project root"),
        ],
        then: Some(
            "if you are an agent and unsure which fits, choose by description or ask the user"
                .into(),
        ),
    }
}

pub fn unknown_instance(
    name: &str,
    instances: &std::collections::BTreeMap<String, Instance>,
) -> SetupRequired {
    SetupRequired {
        problem: format!("unknown instance `{name}`"),
        choose_one: true,
        steps: vec![
            Step::new(&format!("Use one that exists: {}", instances.keys().cloned().collect::<Vec<_>>().join(", ")))
                .run("rkotlin instances"),
            Step::new("Register it")
                .run(&format!("rkotlin instances add {name} --backend instabox|daytona|ssh --description \"...\""))
                .note("`rkotlin instances discover` lists machines your accounts already have."),
        ],
        then: None,
    }
}

pub fn ssh_missing() -> SetupRequired {
    SetupRequired {
        problem: "the `ssh` client is not installed locally".into(),
        choose_one: false,
        steps: vec![Step::new("Install an OpenSSH client")
            .run("sudo apt-get install -y openssh-client   # Debian/Ubuntu")
            .run("brew install openssh                      # macOS (usually already present)")
            .human()],
        then: Some("retry the original command".into()),
    }
}

pub fn instabox_missing() -> SetupRequired {
    SetupRequired {
        problem: "the `instabox` CLI is not installed (needed by instances with backend = \"instabox\")".into(),
        choose_one: false,
        steps: vec![
            Step::new("Install the CLI").run("npm install -g instabox").note("or run once without installing: npx instabox@latest"),
            Step::new("Sign in").run("instabox login").human().note("headless agent: `instabox login --agent --name rkotlin`; relay the claim URL and code to the user"),
        ],
        then: Some("retry the original command".into()),
    }
}

pub fn instabox_key_rejected(code: u16) -> SetupRequired {
    SetupRequired {
        problem: format!("instabox rejected INSTABOX_API_KEY (HTTP {code}); it may be wrong, expired, revoked or missing a scope"),
        choose_one: true,
        steps: vec![
            Step::new("Create a new API key and export it")
                .run("export INSTABOX_API_KEY=ibk_...")
                .human()
                .note("create it in the instabox dashboard under API keys with machines:read, machines:write (and machines:delete if you want `rkotlin remote destroy`)"),
            Step::new("Or drop the key and use the CLI login instead")
                .run("unset INSTABOX_API_KEY")
                .run("instabox login")
                .human(),
        ],
        then: Some("retry the original command".into()),
    }
}

pub fn instabox_login() -> SetupRequired {
    SetupRequired {
        problem: "instabox is not signed in (or the API key is invalid/expired)".into(),
        choose_one: true,
        steps: vec![
            Step::new("Interactive sign-in").run("instabox login").human().note("opens a device-code page; the user enters the code and picks GitHub or Google"),
            Step::new("Headless agent sign-up")
                .run("instabox login --agent --name rkotlin --scope machines:read,machines:write --expires-in-days 30")
                .human()
                .note("prints a claim URL and code: relay both to the user, who approves in a browser.\nThe command finishes by itself once approved, then rkotlin works."),
            Step::new("API key")
                .run("export INSTABOX_API_KEY=ibk_...")
                .human()
                .note("create it in the instabox dashboard under API keys; never print or commit it.\nFor MCP, pass it through the client's `env` for the rkotlin server."),
        ],
        then: Some("verify with `instabox whoami`, then retry the original command".into()),
    }
}

pub fn daytona_key_missing() -> SetupRequired {
    SetupRequired {
        problem: "no Daytona login or API key found (needed by instances with backend = \"daytona\")".into(),
        choose_one: true,
        steps: vec![
            Step::new("Log in with the Daytona CLI (rkotlin reuses that login)")
                .run("brew install daytonaio/cli/daytona   # or see https://www.daytona.io/docs/en/tools/cli/")
                .run("daytona login")
                .human()
                .note("opens a browser. Nothing else to configure: rkotlin reads the CLI's stored login and refreshes it when it expires."),
            Step::new("Use an API key instead")
                .run("open https://app.daytona.io/dashboard/keys")
                .run("export DAYTONA_API_KEY=<the key>")
                .human()
                .note("add it to your shell profile to persist. For MCP clients, set it in the rkotlin server's `env` block, because clients do not inherit your interactive shell.\nIf the account has several organizations, also export DAYTONA_ORGANIZATION_ID=<id>."),
        ],
        then: Some("run `rkotlin remote doctor`".into()),
    }
}

pub fn daytona_key_rejected(code: u16) -> SetupRequired {
    SetupRequired {
        problem: format!("Daytona rejected the credentials (HTTP {code})"),
        choose_one: true,
        steps: vec![
            Step::new("Log in again").run("daytona login").human(),
            Step::new("Or create a fresh API key")
                .run("open https://app.daytona.io/dashboard/keys")
                .run("export DAYTONA_API_KEY=<new key>")
                .human()
                .note("with several organizations also export DAYTONA_ORGANIZATION_ID=<id>"),
        ],
        then: Some("retry the original command".into()),
    }
}

pub fn curl_missing() -> SetupRequired {
    SetupRequired {
        problem: "`curl` is not installed locally (used for the Daytona API)".into(),
        choose_one: false,
        steps: vec![Step::new("Install curl")
            .run("sudo apt-get install -y curl   # or: brew install curl")
            .human()],
        then: Some("retry the original command".into()),
    }
}

pub fn ssh_host_missing(instance: &str) -> SetupRequired {
    SetupRequired {
        problem: format!("instance `{instance}` uses backend ssh but has no `host`"),
        choose_one: false,
        steps: vec![Step::new("Set the host")
            .run(&format!("rkotlin instances add {instance} --backend ssh --set host=user@your-server --description \"...\""))
            .note("host may also be an alias from ~/.ssh/config; add --set port=2222 or --set identity=~/.ssh/id_ed25519 if needed")],
        then: Some("run `rkotlin remote doctor`".into()),
    }
}

pub fn ssh_unreachable(host: &str, detail: &str) -> SetupRequired {
    SetupRequired {
        problem: format!("cannot ssh to {host} non-interactively: {}", detail.trim()),
        choose_one: false,
        steps: vec![
            Step::new("Check the connection by hand")
                .run(&format!("ssh {host} true"))
                .note(
                    "rkotlin cannot answer password or host-key prompts; it needs key-based login",
                ),
            Step::new("Install your public key")
                .run(&format!("ssh-copy-id {host}"))
                .human(),
            Step::new("Accept the host key once")
                .run(&format!(
                    "ssh -o StrictHostKeyChecking=accept-new {host} true"
                ))
                .human(),
        ],
        then: Some("run `rkotlin remote doctor`".into()),
    }
}

pub fn toolchain_missing(instance: &str) -> SetupRequired {
    SetupRequired {
        problem: format!("no JDK is available on instance `{instance}`"),
        choose_one: false,
        steps: vec![Step::new("Install a JDK (and Gradle) on the instance")
            .run(&format!("rkotlin -i {instance} remote setup"))
            .note("installs Temurin JDK 21 (or `remote.jdk`) under ~/.rkotlin, plus Gradle for projects without gradlew; Android work also needs `rkotlin remote setup --android`. Needs curl and unzip (installed with sudo/root if missing)")],
        then: Some("retry the original command".into()),
    }
}

// ------------------------------------------------------------------ onboarding report

pub struct Check {
    pub name: String,
    pub ok: bool,
    pub detail: String,
}

pub struct Onboard {
    pub checks: Vec<Check>,
    pub blocker: Option<SetupRequired>,
}

impl Onboard {
    pub fn ready(&self) -> bool {
        self.blocker.is_none()
    }

    pub fn render(&self) -> String {
        let mut s = String::new();
        for c in &self.checks {
            s += &format!(
                "[{}] {}{}\n",
                if c.ok { " ok " } else { "TODO" },
                c.name,
                if c.detail.is_empty() {
                    String::new()
                } else {
                    format!(" — {}", c.detail)
                }
            );
        }
        match &self.blocker {
            None => s += "\nReady: rkotlin can build on your instance.\n",
            Some(b) => {
                s += "\n";
                s += &b.render();
            }
        }
        s
    }

    pub fn to_json(&self) -> Value {
        json!({
            "ready": self.ready(),
            "checks": self.checks.iter().map(|c| json!({"name": c.name, "ok": c.ok, "detail": c.detail})).collect::<Vec<_>>(),
            "setup": self.blocker.as_ref().map(|b| b.to_json()),
        })
    }
}

/// Walk the prerequisites in order and stop at the first blocker. `live` also talks to the
/// provider (status, ssh reachability, remote toolchain) but never creates or starts anything.
pub fn onboard(loaded: &Loaded, requested: Option<&str>, live: bool) -> Onboard {
    let mut checks = vec![];
    let mut push = |name: &str, ok: bool, detail: &str| {
        checks.push(Check {
            name: name.into(),
            ok,
            detail: detail.into(),
        })
    };

    let ssh_ok = crate::util::on_path(&transport::ssh_bin());
    push("local ssh client", ssh_ok, "");
    if !ssh_ok {
        return Onboard {
            checks,
            blocker: Some(ssh_missing()),
        };
    }

    let (name, inst) = match loaded.resolve(requested) {
        Ok(x) => {
            push(
                "instance selected",
                true,
                &format!("`{}` ({})", x.0, x.1.backend),
            );
            x
        }
        Err(e) => {
            push("instance selected", false, "");
            let b = find(&e).cloned().unwrap_or_else(|| SetupRequired {
                problem: format!("{e:#}"),
                choose_one: false,
                steps: vec![],
                then: None,
            });
            return Onboard {
                checks,
                blocker: Some(b),
            };
        }
    };

    match inst.backend.as_str() {
        "instabox" => {
            if crate::backend::instabox::using_api_key() {
                push(
                    "INSTABOX_API_KEY set",
                    true,
                    "REST API, instabox CLI not needed",
                );
            } else {
                let present = crate::util::on_path("instabox");
                push("instabox CLI installed", present, "");
                if !present {
                    return Onboard {
                        checks,
                        blocker: Some(instabox_missing()),
                    };
                }
            }
        }
        "daytona" => {
            let key = crate::backend::daytona::have_credentials(&inst);
            push("Daytona login or API key found", key, "");
            if !key {
                return Onboard {
                    checks,
                    blocker: Some(daytona_key_missing()),
                };
            }
            let curl = crate::util::on_path("curl");
            push("curl installed", curl, "");
            if !curl {
                return Onboard {
                    checks,
                    blocker: Some(curl_missing()),
                };
            }
        }
        _ => {
            let ok = inst.host.is_some();
            push(
                "ssh host configured",
                ok,
                inst.host.as_deref().unwrap_or(""),
            );
            if !ok {
                return Onboard {
                    checks,
                    blocker: Some(ssh_host_missing(&name)),
                };
            }
        }
    }

    if !live {
        push("provider reachability", true, "not checked (offline); run `rkotlin remote doctor` or the MCP `setup` tool with live=true");
        return Onboard {
            checks,
            blocker: None,
        };
    }

    let b = match backend::make(&name, &inst) {
        Ok(b) => b,
        Err(e) => {
            let sr = find(&e).cloned().unwrap_or_else(|| SetupRequired {
                problem: format!("{e:#}"),
                choose_one: false,
                steps: vec![],
                then: None,
            });
            return Onboard {
                checks,
                blocker: Some(sr),
            };
        }
    };
    let st = match b.status() {
        Ok(s) => s,
        Err(e) => {
            push(
                "provider account",
                false,
                format!("{e:#}").lines().next().unwrap_or(""),
            );
            let sr = find(&e).cloned().unwrap_or_else(|| SetupRequired {
                problem: format!("{e:#}"),
                choose_one: false,
                steps: vec![],
                then: None,
            });
            return Onboard {
                checks,
                blocker: Some(sr),
            };
        }
    };
    push("provider account", true, "");
    push(
        &format!("instance `{name}` state"),
        true,
        &format!("{} ({})", st.state, st.detail),
    );
    if st.state == "running" {
        match b.ensure() {
            Ok(ssh) => {
                push("ssh to instance", true, "");
                match ssh.capture(
                    "[ -f \"$HOME/.rkotlin/env\" ] && . \"$HOME/.rkotlin/env\"; command -v java >/dev/null 2>&1 && java -version 2>&1 | head -1",
                    None,
                ) {
                    Ok((0, out, _)) => {
                        push("remote JDK", true, String::from_utf8_lossy(&out).trim())
                    }
                    _ => {
                        push("remote JDK", false, "");
                        return Onboard {
                            checks,
                            blocker: Some(toolchain_missing(&name)),
                        };
                    }
                }
            }
            Err(e) => {
                push("ssh to instance", false, "");
                let sr = find(&e).cloned().unwrap_or_else(|| SetupRequired {
                    problem: format!("{e:#}"),
                    choose_one: false,
                    steps: vec![],
                    then: None,
                });
                return Onboard {
                    checks,
                    blocker: Some(sr),
                };
            }
        }
    } else {
        push("remote toolchain", true, "not checked: instance is not running (it starts on first use; run `rkotlin remote up` then re-check)");
    }
    Onboard {
        checks,
        blocker: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_marks_user_steps_and_agent_rules() {
        let r = no_instances().render();
        assert!(r.contains("Pick ONE"));
        assert!(r.contains("A. [USER]"));
        assert!(r.contains("instabox login --agent"));
        assert!(r.contains("Never invent or guess credentials"));
    }

    #[test]
    fn json_shape_for_agents() {
        let j = instabox_login().to_json();
        assert_eq!(j["setup_required"], true);
        assert!(j["steps"][0]["needs_user"].as_bool().unwrap());
        assert!(j["steps"][1]["run"][0]
            .as_str()
            .unwrap()
            .contains("--agent"));
    }

    #[test]
    fn survives_anyhow_wrapping() {
        let e = anyhow::Error::new(daytona_key_missing()).context("while opening instance");
        assert!(find(&e).is_some());
    }

    #[test]
    fn ambiguous_lists_descriptions() {
        let mut m = std::collections::BTreeMap::new();
        m.insert(
            "a".to_string(),
            Instance {
                backend: "ssh".into(),
                description: Some("big box".into()),
                ..Default::default()
            },
        );
        m.insert(
            "b".to_string(),
            Instance {
                backend: "instabox".into(),
                ..Default::default()
            },
        );
        let r = ambiguous(&m).render();
        assert!(r.contains("a (ssh): big box"));
        assert!(r.contains("rkotlin instances use NAME"));
    }
}
