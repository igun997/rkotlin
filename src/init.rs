//! `rkotlin init` / `rkotlin integrate` / `rkotlin hook`: one-command setup for people (and agents)
//! who just want their laptop to stay cool.

use anyhow::{bail, Context, Result};
use clap::Args;
use serde_json::{json, Value};
use std::io::Read;
use std::path::{Path, PathBuf};

use crate::{config, guidance, project};

const SKILL_MD: &str = include_str!("../skills/rkotlin/SKILL.md");

const PROJECT_TOML: &str = r#"# rkotlin project settings. Remote machines ("instances") are configured per user:
#   rkotlin instances            # list them
#   rkotlin instances add ...    # add one
#
# Pin this project to one instance (otherwise your default is used):
# instance = "main"

[sync]
# Extra things that must never be uploaded (gitignore syntax). .gitignore is already honoured.
# Signing keystores, key.properties and local.properties are never uploaded unless you set
# default_excludes = false.
exclude = []

[remote]
# JDK major version on the remote (Temurin, installed under ~/.rkotlin). Default 21.
# jdk = 17
# Environment variables copied from YOUR shell to the remote at run time, never written to disk.
# Needed for private maven repositories or signing passwords, e.g. ["GITHUB_TOKEN", "KEYSTORE_PASSWORD"].
forward_env = []
"#;

#[derive(Args, Default)]
pub struct IntegrateArgs {
    /// Install the agent skill (Claude Code / Codex / Cursor style SKILL.md).
    #[arg(long)]
    pub skill: bool,
    /// Register the MCP server (.mcp.json, .cursor/mcp.json).
    #[arg(long)]
    pub mcp: bool,
    /// Block local `gradle`/`./gradlew` builds in Claude Code and point to rkotlin instead.
    #[arg(long)]
    pub guard: bool,
    /// Everything above (default when no flag is given).
    #[arg(long)]
    pub all: bool,
    /// Install the skill for your user (~/.claude, ~/.agents) instead of this project.
    #[arg(long)]
    pub global: bool,
    /// Do not install the bundled Kotlin and Android skills (JetBrains, Google); only the rkotlin skill.
    #[arg(long)]
    pub no_kotlin_skills: bool,
}

fn base_dir(global: bool) -> Result<PathBuf> {
    if global {
        return Ok(crate::util::home());
    }
    let cwd = std::env::current_dir()?;
    Ok(project::discover(&cwd).map(|p| p.root).unwrap_or(cwd))
}

pub fn init() -> Result<i32> {
    let cwd = std::env::current_dir()?;
    let proj = project::discover(&cwd).map_err(|_| {
        anyhow::anyhow!(
            "run `rkotlin init` inside your Gradle project folder (the one with settings.gradle.kts or build.gradle.kts)"
        )
    })?;
    let toml_path = proj.root.join("rkotlin.toml");
    if toml_path.exists() {
        println!("  = rkotlin.toml already exists, left as is");
    } else {
        std::fs::write(&toml_path, PROJECT_TOML)?;
        println!("  + created rkotlin.toml");
    }
    integrate_in(
        &proj.root,
        &IntegrateArgs {
            all: true,
            ..Default::default()
        },
    )?;

    println!("\nChecking what is still needed...\n");
    let loaded = config::load(&cwd)?;
    let rep = guidance::onboard(&loaded, None, false);
    print!("{}", rep.render());
    if rep.ready() {
        println!(
            "\nTry it:  rkotlin build      (it builds on the remote machine; your laptop stays idle)"
        );
    }
    Ok(0)
}

pub fn integrate(a: IntegrateArgs) -> Result<i32> {
    let base = base_dir(a.global)?;
    integrate_in(&base, &a)?;
    Ok(0)
}

/// The `rkotlin` agent skill, for Claude Code (.claude) and other agents (.agents).
pub fn install_skills(base: &Path) -> Result<Vec<PathBuf>> {
    let mut written = vec![];
    for root in [".claude/skills", ".agents/skills"] {
        let dir = base.join(root).join("rkotlin");
        std::fs::create_dir_all(&dir)?;
        std::fs::write(dir.join("SKILL.md"), SKILL_MD)?;
        written.push(dir.join("SKILL.md"));
    }
    Ok(written)
}

fn integrate_in(base: &Path, a: &IntegrateArgs) -> Result<()> {
    let everything = a.all || !(a.skill || a.mcp || a.guard);
    let global = a.global;
    if everything || a.skill {
        for p in install_skills(base)? {
            println!("  + agent skill: {}", p.display());
        }
        if !a.no_kotlin_skills {
            let n = crate::skills::install(base, &[])?.len() / 2;
            println!("  + {n} Kotlin/Android agent skills from JetBrains and Google (.claude/skills, .agents/skills); `rkotlin skills` lists them");
        }
    }
    if global {
        println!("(--global installs the skill only; run `rkotlin integrate` inside a project for MCP and the guard)");
        return Ok(());
    }
    if everything || a.mcp {
        let entry = json!({"command": "rkotlin", "args": ["mcp"]});
        merge_json(&base.join(".mcp.json"), |v| {
            v["mcpServers"]["rkotlin"] = entry.clone();
        })?;
        println!("  + MCP server registered in .mcp.json (Claude Code)");
        if base.join(".cursor").is_dir() {
            merge_json(&base.join(".cursor/mcp.json"), |v| {
                v["mcpServers"]["rkotlin"] = entry.clone();
            })?;
            println!("  + MCP server registered in .cursor/mcp.json (Cursor)");
        }
        println!("    other agents: `claude mcp add rkotlin -- rkotlin mcp`   `codex mcp add rkotlin -- rkotlin mcp`");
    }
    if everything || a.guard {
        merge_json(&base.join(".claude/settings.json"), |v| {
            let hooks = &mut v["hooks"]["PreToolUse"];
            if !hooks.is_array() {
                *hooks = json!([]);
            }
            let arr = hooks.as_array_mut().unwrap();
            let present = arr.iter().any(|e| e.to_string().contains("rkotlin hook"));
            if !present {
                arr.push(json!({"matcher": "Bash", "hooks": [{"type": "command", "command": "rkotlin hook"}]}));
            }
        })?;
        println!("  + guard installed: Claude Code can no longer run Gradle on this computer by accident (.claude/settings.json)");
    }
    Ok(())
}

/// Read-modify-write a JSON file, creating it if needed. Refuses to touch files that are not plain JSON.
fn merge_json(path: &Path, f: impl FnOnce(&mut Value)) -> Result<()> {
    let mut v: Value = if path.is_file() {
        let s = std::fs::read_to_string(path)?;
        if s.trim().is_empty() {
            json!({})
        } else {
            serde_json::from_str(&s).with_context(|| {
                format!(
                    "{} is not plain JSON (comments?); add the rkotlin entry by hand, see README",
                    path.display()
                )
            })?
        }
    } else {
        json!({})
    };
    if !v.is_object() {
        bail!("{} is not a JSON object", path.display());
    }
    f(&mut v);
    if let Some(d) = path.parent() {
        std::fs::create_dir_all(d)?;
    }
    std::fs::write(path, serde_json::to_string_pretty(&v)? + "\n")?;
    Ok(())
}

// ------------------------------------------------------------------ guard hook

/// Gradle tasks that do no compiling; everything else is heavy.
const LIGHT: &[&str] = &[
    "tasks",
    "help",
    "wrapper",
    "init",
    "properties",
    "projects",
    "dependencies",
    "dependencyInsight",
    "buildEnvironment",
    "outgoingVariants",
    "components",
    "clean",
];
const LIGHT_FLAGS: &[&str] = &["-v", "--version", "-h", "--help", "--stop", "--status"];
/// Gradle options that take a separate value, which must not be mistaken for a task.
const VALUE_FLAGS: &[&str] = &[
    "--gradle-version",
    "--distribution-type",
    "--gradle-distribution-url",
    "--type",
    "--dsl",
    "--package",
    "--project-name",
    "--test-framework",
    "-p",
    "--project-dir",
    "-b",
    "--build-file",
    "-c",
    "--settings-file",
    "-g",
    "--gradle-user-home",
    "-I",
    "--init-script",
    "-P",
    "-D",
    "--include-build",
    "--max-workers",
    "-x",
    "--exclude-task",
    "--tests",
    "--console",
];

/// Does this shell command line start a local Gradle build? Returns the rkotlin equivalent.
pub fn local_gradle_invocation(cmdline: &str) -> Option<String> {
    if cmdline.contains("RKOTLIN_ALLOW_LOCAL=") {
        return None;
    }
    let seps: &[char] = &[';', '&', '|', '\n', '(', ')', '`'];
    for seg in cmdline.split(seps) {
        let mut toks: Vec<&str> = seg.split_whitespace().collect();
        while let Some(t) = toks.first() {
            let is_assign = t.contains('=')
                && !t.starts_with('-')
                && t.split('=').next().is_some_and(crate::util::valid_env_key);
            if is_assign || matches!(*t, "sudo" | "time" | "nice" | "env" | "command" | "exec") {
                toks.remove(0);
            } else {
                break;
            }
        }
        let Some(first) = toks.first() else { continue };
        let name = first.rsplit('/').next().unwrap_or(first);
        if !matches!(name, "gradle" | "gradlew" | "gradlew.bat") {
            continue;
        }
        let args = &toks[1..];
        if args.iter().any(|a| LIGHT_FLAGS.contains(a)) {
            continue;
        }
        let mut tasks: Vec<&str> = vec![];
        let mut skip = false;
        for a in args {
            if skip {
                skip = false;
            } else if VALUE_FLAGS.contains(a) {
                skip = true;
            } else if !a.starts_with('-') {
                tasks.push(a);
            }
        }
        let heavy = tasks
            .iter()
            .any(|t| !LIGHT.contains(&t.rsplit(':').next().unwrap_or(t)));
        if heavy {
            return Some(format!("rkotlin {}", args.join(" ")));
        }
    }
    None
}

/// Claude Code PreToolUse hook: JSON on stdin; exit 2 blocks the tool call and shows stderr to the agent.
pub fn hook() -> Result<i32> {
    let mut s = String::new();
    std::io::stdin().read_to_string(&mut s)?;
    let v: Value = serde_json::from_str(&s).unwrap_or(Value::Null);
    let cmd = v["tool_input"]["command"].as_str().unwrap_or("");
    if let Some(alt) = local_gradle_invocation(cmd) {
        eprintln!(
            "Blocked: this would run Gradle on the user's own computer, which is slow and can freeze it.\n\
             Run it on the remote build machine instead:\n    {alt}\n\
             (MCP clients can call the `gradle` tool of the rkotlin server.) \
             If the user explicitly wants a local run, prefix the command with RKOTLIN_ALLOW_LOCAL=1."
        );
        return Ok(2);
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blocks_heavy_local_gradle() {
        for c in [
            "./gradlew build",
            "gradle test --info",
            "cd app && ./gradlew :app:assembleDebug",
            "JAVA_HOME=/x ./gradlew assembleRelease",
            "sudo gradle build",
            "/opt/gradle/bin/gradle check",
            "echo hi; time ./gradlew -p app testDebugUnitTest",
            "./gradlew --no-daemon connectedAndroidTest",
            "./gradlew clean build",
            "./gradlew",
        ] {
            if c == "./gradlew" {
                // no task at all: Gradle runs the default (help) task
                assert!(local_gradle_invocation(c).is_none(), "should allow: {c}");
            } else {
                assert!(local_gradle_invocation(c).is_some(), "should block: {c}");
            }
        }
    }

    #[test]
    fn allows_light_and_remote_commands() {
        for c in [
            "rkotlin build",
            "./gradlew --version",
            "./gradlew tasks",
            "./gradlew :app:dependencies",
            "gradle wrapper --gradle-version 8.10",
            "./gradlew clean",
            "./gradlew --stop",
            "git commit -m 'gradle build fixed'",
            "echo ./gradlew build",
            "RKOTLIN_ALLOW_LOCAL=1 ./gradlew build",
            "ls",
        ] {
            assert!(local_gradle_invocation(c).is_none(), "should allow: {c}");
        }
    }

    #[test]
    fn value_flags_are_not_tasks() {
        // `-p app` is a project dir, the only task is `tasks`
        assert!(local_gradle_invocation("./gradlew -p app tasks").is_none());
        assert!(local_gradle_invocation("./gradlew -Pfoo=bar -x test build").is_some());
    }

    #[test]
    fn suggests_rkotlin_equivalent() {
        assert_eq!(
            local_gradle_invocation("./gradlew :app:test --tests '*FooTest'").unwrap(),
            "rkotlin :app:test --tests '*FooTest'"
        );
    }

    #[test]
    fn merge_json_creates_and_preserves() {
        let d = std::env::temp_dir().join(format!("rkotlin-json-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let p = d.join("s.json");
        std::fs::write(
            &p,
            r#"{"keep": 1, "hooks": {"PreToolUse": [{"matcher":"Edit"}]}}"#,
        )
        .unwrap();
        merge_json(&p, |v| {
            v["hooks"]["PreToolUse"]
                .as_array_mut()
                .unwrap()
                .push(json!({"matcher":"Bash"}));
        })
        .unwrap();
        let v: Value = serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
        assert_eq!(v["keep"], 1);
        assert_eq!(v["hooks"]["PreToolUse"].as_array().unwrap().len(), 2);
        std::fs::write(&p, "// comment\n{}").unwrap();
        assert!(
            merge_json(&p, |_| {}).is_err(),
            "refuses JSONC instead of clobbering it"
        );
        let _ = std::fs::remove_dir_all(&d);
    }
}
