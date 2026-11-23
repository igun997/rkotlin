#[macro_use]
mod util;
mod backend;
mod cmds;
mod config;
mod guidance;
mod http;
mod init;
mod install;
mod mcp;
mod project;
mod run;
mod skills;
mod summary;
mod sync;
mod transport;

use anyhow::Result;
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "rkotlin",
    version,
    about = "Run Gradle (Kotlin, Android, JVM) on remote sandboxes (instabox, Daytona, any SSH host) instead of your own CPU",
    long_about = "Run Gradle (Kotlin, Android, JVM) on remote sandboxes (instabox, Daytona, any SSH host) instead of your own CPU.\n\n\
Your working tree (uncommitted and private code included, plus `includeBuild(\"..\")` composite builds) is delta-synced \
over SSH, gradle runs remotely with persistent Gradle caches and build/ directories, and output streams back with paths \
rewritten to local ones. No GitHub access is involved.\n\n\
Any subcommand not listed below is passed to Gradle and run remotely:\n  \
rkotlin build\n  rkotlin :app:assembleDebug\n  rkotlin test --tests '*FooTest'\n  rkotlin --install assembleDebug   (build remotely, install on your phone with local adb)\n\n\
Gradle's own `init` is shadowed by rkotlin init; run it as `rkotlin gradle init`.",
    allow_external_subcommands = true,
    after_help = "Instances: you can configure several remote machines (`rkotlin instances`) and pick one with -i NAME."
)]
struct Cli {
    /// Remote instance to use (see `rkotlin instances`). Default: project `instance`, then `default_instance`.
    #[arg(short, long, global = true, env = "RKOTLIN_INSTANCE")]
    instance: Option<String>,
    /// Suppress rkotlin's own progress lines on stderr.
    #[arg(short, long, global = true)]
    quiet: bool,
    /// Copy build outputs (apk, aab, jar, distributions) back into the local build/ directories after a successful run.
    #[arg(long)]
    pull: bool,
    /// After a successful build, install the pulled apk on the phone with your local adb (implies --pull).
    /// `--install` needs exactly one device attached; `--install=SERIAL` picks one.
    #[arg(long, num_args = 0..=1, require_equals = true, default_missing_value = "", value_name = "SERIAL")]
    install: Option<String>,
    /// Skip syncing; run against whatever is already on the remote.
    #[arg(long)]
    no_sync: bool,
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// List, add, remove and choose remote instances (machines rkotlin can build on).
    Instances {
        #[command(subcommand)]
        action: Option<InstancesCmd>,
    },
    /// Manage the remote side: power, sync, shell, health, disk usage.
    Remote {
        #[command(subcommand)]
        action: RemoteCmd,
    },
    /// One-command setup for this project: config file, AI agent skill, MCP server and a guard against local Gradle builds.
    Init,
    /// Install agent/editor integration: skill files, MCP config and the local-build guard.
    Integrate(init::IntegrateArgs),
    /// Run as an MCP server over stdio (tools: gradle, shell, sync, instances, power, workspaces).
    Mcp,
    /// Show what is still missing to start building (instance, login, toolchain) and exactly how to fix it.
    Onboard {
        /// Also contact the provider and the instance (never creates or starts anything).
        #[arg(long)]
        live: bool,
        #[arg(long)]
        json: bool,
    },
    /// Claude Code PreToolUse hook: blocks local gradle builds and points to rkotlin (reads JSON on stdin).
    #[command(hide = true)]
    Hook,
    /// List the Android devices your local adb sees (what `--install` can target).
    Devices,
    /// List or install the bundled Kotlin and Android agent skills (from JetBrains and Google).
    Skills {
        #[command(subcommand)]
        action: Option<SkillsCmd>,
    },
    /// Run any Gradle command remotely (the explicit form; needed for names shadowed by rkotlin, e.g. `init`).
    Gradle {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    #[command(external_subcommand)]
    External(Vec<String>),
}

#[derive(Subcommand)]
pub enum SkillsCmd {
    /// Show the bundled skills with their source (default).
    List,
    /// Install bundled skills into .claude/skills and .agents/skills of this project.
    Add {
        /// Skill names (see `rkotlin skills`).
        names: Vec<String>,
        /// Install every bundled skill.
        #[arg(long)]
        all: bool,
        /// Install for your user (~/.claude, ~/.agents) instead of this project.
        #[arg(long)]
        global: bool,
    },
}

#[derive(Subcommand)]
pub enum InstancesCmd {
    /// Show configured instances with live state.
    List {
        #[arg(long)]
        json: bool,
        /// Do not contact providers; configuration only.
        #[arg(long)]
        offline: bool,
    },
    /// Register an instance in ~/.config/rkotlin/config.toml.
    Add {
        name: String,
        #[arg(long)]
        backend: String,
        /// Shown to humans and AI agents; say what the machine is good for.
        #[arg(long)]
        description: Option<String>,
        /// Extra settings as key=value, repeatable (box, vcpus, mem_mib, host, port, identity, name, snapshot, cpu, memory, disk ...).
        #[arg(long = "set", value_name = "KEY=VALUE")]
        set: Vec<String>,
        /// Make it the default instance.
        #[arg(long)]
        default: bool,
    },
    /// Remove an instance from the config (the machine itself is not touched).
    Rm { name: String },
    /// Set the default instance.
    Use { name: String },
    /// Find existing instabox boxes / Daytona sandboxes not yet registered.
    Discover,
}

#[derive(Subcommand)]
pub enum RemoteCmd {
    /// Create/start/resume the instance and verify SSH works.
    Up,
    /// Pause/stop the instance to save money.
    Down,
    /// State of the instance and of this project's remote workspace.
    Status,
    /// Check local tools, the instance and the remote toolchain.
    Doctor,
    /// Install a JDK (and Gradle, optionally the Android SDK) on the remote if missing.
    Setup {
        /// Also install the Android SDK (command-line tools, platform-tools, platform and build-tools).
        #[arg(long)]
        android: bool,
    },
    /// Sync only; run nothing.
    Sync,
    /// Open a shell (or run a command) in this project's remote workspace.
    Shell {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        command: Vec<String>,
    },
    /// List workspaces on the instance with disk usage.
    Ls,
    /// Delete stale workspaces (dry run unless --yes).
    Gc {
        #[arg(long, default_value_t = 14)]
        days: u64,
        #[arg(long)]
        all: bool,
        #[arg(long)]
        yes: bool,
    },
    /// Delete this project's remote workspace, including its build/ directories.
    Rm,
    /// Permanently delete the machine behind the instance (needs --yes). Local caches are cleaned too.
    Destroy {
        #[arg(long)]
        yes: bool,
    },
}

fn main() {
    let cli = Cli::parse();
    util::set_quiet(cli.quiet);
    match dispatch(cli) {
        Ok(code) => std::process::exit(code),
        Err(e) => {
            if let Some(sr) = guidance::find(&e) {
                eprint!("\n{}", sr.render());
                std::process::exit(3);
            }
            eprintln!("rkotlin: error: {e:#}");
            std::process::exit(2);
        }
    }
}

fn dispatch(cli: Cli) -> Result<i32> {
    let inst = cli.instance.clone();
    match cli.cmd {
        None => {
            use clap::CommandFactory;
            Cli::command().print_help()?;
            println!();
            Ok(0)
        }
        Some(Cmd::External(args)) | Some(Cmd::Gradle { args }) => {
            cmds::gradle(inst.as_deref(), args, cli.pull, cli.install, cli.no_sync)
        }
        Some(Cmd::Devices) => install::devices(),
        Some(Cmd::Skills { action }) => match action.unwrap_or(SkillsCmd::List) {
            SkillsCmd::List => {
                print!("{}", skills::list());
                Ok(0)
            }
            SkillsCmd::Add { names, all, global } => {
                if names.is_empty() && !all {
                    anyhow::bail!("name the skills to install, or use --all. `rkotlin skills` lists them");
                }
                let base = if global {
                    util::home()
                } else {
                    let cwd = std::env::current_dir()?;
                    project::discover(&cwd).map(|p| p.root).unwrap_or(cwd)
                };
                let dirs = skills::install(&base, if all { &[] } else { &names })?;
                for d in dirs {
                    println!("  + {}", d.display());
                }
                Ok(0)
            }
        },
        Some(Cmd::Instances { action }) => cmds::instances(action.unwrap_or(InstancesCmd::List {
            json: false,
            offline: false,
        })),
        Some(Cmd::Remote { action }) => cmds::remote(inst.as_deref(), action),
        Some(Cmd::Init) => init::init(),
        Some(Cmd::Integrate(a)) => init::integrate(a),
        Some(Cmd::Onboard { live, json }) => {
            let cwd = std::env::current_dir()?;
            let loaded = config::load(&cwd)?;
            let rep = guidance::onboard(&loaded, inst.as_deref(), live);
            if json {
                println!("{}", serde_json::to_string_pretty(&rep.to_json())?);
            } else {
                print!("{}", rep.render());
            }
            Ok(if rep.ready() { 0 } else { 3 })
        }
        Some(Cmd::Hook) => init::hook(),
        Some(Cmd::Mcp) => {
            util::set_quiet(true);
            mcp::serve(inst)?;
            Ok(0)
        }
    }
}
