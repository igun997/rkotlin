use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::util;

pub const BACKENDS: [&str; 3] = ["instabox", "daytona", "ssh"];

/// JDK installed on the remote when `remote.jdk` is not set (current LTS, what AGP 8.x and Kotlin 2 expect).
pub const DEFAULT_JDK: u32 = 21;

/// One remote machine rkotlin can build on. A user may configure many.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Instance {
    /// instabox | daytona | ssh
    pub backend: String,
    /// Free text shown to humans *and agents* so they can pick the right instance.
    pub description: Option<String>,

    // instabox
    #[serde(rename = "box")]
    pub box_name: Option<String>,
    pub auto_create: Option<bool>,
    pub vcpus: Option<u32>,
    pub mem_mib: Option<u32>,
    pub template: Option<String>,

    // daytona
    pub name: Option<String>,
    pub api_url: Option<String>,
    pub snapshot: Option<String>,
    pub cpu: Option<u32>,
    /// GB
    pub memory: Option<u32>,
    /// GB
    pub disk: Option<u32>,
    pub auto_stop_minutes: Option<u32>,
    pub organization_id: Option<String>,
    pub ssh_host: Option<String>,

    // ssh
    pub host: Option<String>,
    pub port: Option<u16>,
    pub identity: Option<String>,
    pub user: Option<String>,

    // common
    pub env: BTreeMap<String, String>,
    pub forward_env: Vec<String>,
    /// Absolute directory on the remote that holds workspaces (default: ~/.rkotlin/ws).
    pub base: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SyncCfg {
    /// gitignore-style patterns that are never sent.
    pub exclude: Vec<String>,
    pub include_git: bool,
    /// Skip .env, *.pem, *.key, signing keystores, local.properties and .npmrc (default true). Set false to upload them.
    pub default_excludes: Option<bool>,
    /// Follow `includeBuild("../x")` / `projectDir = file("../x")` outside the project root (default true).
    pub path_deps: Option<bool>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RemoteCfg {
    pub env: BTreeMap<String, String>,
    pub forward_env: Vec<String>,
    /// Gradle command on the remote (default: `./gradlew` if the project has one, else `gradle`).
    pub gradle: Option<String>,
    /// JDK major version (Temurin, installed under ~/.rkotlin; default 21).
    pub jdk: Option<u32>,
    /// Gradle version installed by `remote setup` for projects without a wrapper (default: latest).
    pub gradle_version: Option<String>,
    /// sdkmanager packages installed by `remote setup --android`
    /// (default: platform-tools, platforms;android-35, build-tools;35.0.0).
    pub android_packages: Vec<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// User-level default instance.
    pub default_instance: Option<String>,
    /// Project-level choice of instance (rkotlin.toml).
    pub instance: Option<String>,
    pub instances: BTreeMap<String, Instance>,
    pub sync: SyncCfg,
    pub remote: RemoteCfg,
}

pub struct Loaded {
    pub cfg: Config,
    pub project_file: Option<PathBuf>,
    pub global_file: PathBuf,
    implicit: bool,
}

pub struct InstanceRow {
    pub name: String,
    pub inst: Instance,
    pub is_default: bool,
}

pub fn global_path() -> PathBuf {
    if let Some(p) = std::env::var_os("RKOTLIN_CONFIG").filter(|v| !v.is_empty()) {
        return PathBuf::from(p);
    }
    util::config_dir().join("config.toml")
}

pub fn find_project_file(start: &Path) -> Option<PathBuf> {
    for dir in start.ancestors() {
        for n in ["rkotlin.toml", ".rkotlin.toml"] {
            let p = dir.join(n);
            if p.is_file() {
                return Some(p);
            }
        }
    }
    None
}

fn read_value(p: &Path) -> Result<toml::Value> {
    let s = std::fs::read_to_string(p).with_context(|| format!("reading {}", p.display()))?;
    s.parse::<toml::Value>()
        .with_context(|| format!("parsing {}", p.display()))
}

fn merge(base: &mut toml::Value, over: toml::Value) {
    match (base, over) {
        (toml::Value::Table(b), toml::Value::Table(o)) => {
            for (k, v) in o {
                match b.get_mut(&k) {
                    Some(slot) => merge(slot, v),
                    None => {
                        b.insert(k, v);
                    }
                }
            }
        }
        (slot, v) => *slot = v,
    }
}

pub fn load(cwd: &Path) -> Result<Loaded> {
    let global_file = global_path();
    let mut v = toml::Value::Table(Default::default());
    if global_file.is_file() {
        merge(&mut v, read_value(&global_file)?);
    }
    let project_file = find_project_file(cwd);
    if let Some(p) = &project_file {
        merge(&mut v, read_value(p)?);
    }
    let mut cfg: Config = v.try_into().context("invalid rkotlin configuration")?;
    let mut implicit = false;
    if cfg.instances.is_empty() && util::on_path("instabox") {
        cfg.instances.insert(
            "default".into(),
            Instance {
                backend: "instabox".into(),
                box_name: Some("rkotlin".into()),
                description: Some(
                    "implicit default: instabox box named `rkotlin` (created on first use)".into(),
                ),
                ..Default::default()
            },
        );
        implicit = true;
    }
    Ok(Loaded {
        cfg,
        project_file,
        global_file,
        implicit,
    })
}

impl Loaded {
    pub fn default_name(&self) -> Option<String> {
        if let Some(n) = self
            .cfg
            .instance
            .as_ref()
            .or(self.cfg.default_instance.as_ref())
        {
            return Some(n.clone());
        }
        if self.cfg.instances.len() == 1 {
            return self.cfg.instances.keys().next().cloned();
        }
        None
    }

    pub fn rows(&self) -> Vec<InstanceRow> {
        let d = self.default_name();
        self.cfg
            .instances
            .iter()
            .map(|(n, i)| InstanceRow {
                name: n.clone(),
                inst: i.clone(),
                is_default: d.as_deref() == Some(n.as_str()),
            })
            .collect()
    }

    pub fn is_implicit(&self) -> bool {
        self.implicit
    }

    /// `requested` > RKOTLIN_INSTANCE (handled by clap) > project `instance` > `default_instance` > sole instance.
    pub fn resolve(&self, requested: Option<&str>) -> Result<(String, Instance)> {
        let name = match requested {
            Some(n) => n.to_string(),
            None => match self.default_name() {
                Some(n) => n,
                None if self.cfg.instances.is_empty() => {
                    return Err(crate::guidance::no_instances().into_err())
                }
                None => return Err(crate::guidance::ambiguous(&self.cfg.instances).into_err()),
            },
        };
        let Some(inst) = self.cfg.instances.get(&name).cloned() else {
            return Err(crate::guidance::unknown_instance(&name, &self.cfg.instances).into_err());
        };
        if !BACKENDS.contains(&inst.backend.as_str()) {
            bail!(
                "instance `{name}`: backend must be one of {} (got `{}`)",
                BACKENDS.join(" | "),
                inst.backend
            );
        }
        Ok((name, inst))
    }
}

/// Mutate the user-level config file (comments are not preserved).
pub fn edit_global(f: impl FnOnce(&mut toml::value::Table) -> Result<()>) -> Result<PathBuf> {
    let path = global_path();
    let mut v = if path.is_file() {
        read_value(&path)?
    } else {
        toml::Value::Table(Default::default())
    };
    let table = v.as_table_mut().context("config root is not a table")?;
    f(table)?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(&path, toml::to_string_pretty(&v)?)?;
    Ok(path)
}

/// Parse `key=value` where value is a TOML scalar if it looks like one, else a string.
pub fn parse_kv(s: &str) -> Result<(String, toml::Value)> {
    let (k, v) = s
        .split_once('=')
        .with_context(|| format!("expected key=value, got `{s}`"))?;
    let val = match format!("v = {v}").parse::<toml::Table>() {
        Ok(t) => match t.get("v") {
            Some(x) if !x.is_table() => x.clone(),
            _ => toml::Value::String(v.to_string()),
        },
        Err(_) => toml::Value::String(v.to_string()),
    };
    Ok((k.trim().to_string(), val))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merge_layers() {
        let mut a: toml::Value = "default_instance='a'\n[instances.a]\nbackend='ssh'\nhost='h'\n"
            .parse()
            .unwrap();
        let b: toml::Value =
            "instance='b'\n[instances.b]\nbackend='instabox'\n[instances.a]\nhost='h2'\n"
                .parse()
                .unwrap();
        merge(&mut a, b);
        let c: Config = a.try_into().unwrap();
        assert_eq!(c.instance.as_deref(), Some("b"));
        assert_eq!(c.instances["a"].host.as_deref(), Some("h2"));
        assert_eq!(c.instances["a"].backend, "ssh");
        assert_eq!(c.instances.len(), 2);
    }

    #[test]
    fn kv_types() {
        assert_eq!(parse_kv("vcpus=8").unwrap().1, toml::Value::Integer(8));
        assert_eq!(
            parse_kv("auto_create=false").unwrap().1,
            toml::Value::Boolean(false)
        );
        assert_eq!(
            parse_kv("box=rkotlin-big").unwrap().1,
            toml::Value::String("rkotlin-big".into())
        );
        assert_eq!(
            parse_kv("host=me@x.io").unwrap().1,
            toml::Value::String("me@x.io".into())
        );
    }

    fn loaded(toml: &str) -> Loaded {
        Loaded {
            cfg: toml::from_str(toml).unwrap(),
            project_file: None,
            global_file: PathBuf::new(),
            implicit: false,
        }
    }

    #[test]
    fn resolution_rules() {
        let l = loaded("[instances.a]\nbackend='ssh'\n[instances.b]\nbackend='ssh'\n");
        assert!(l.resolve(None).is_err(), "ambiguous without default");
        assert_eq!(l.resolve(Some("b")).unwrap().0, "b");
        assert!(l.resolve(Some("zzz")).is_err());
        let l = loaded(
            "default_instance='b'\n[instances.a]\nbackend='ssh'\n[instances.b]\nbackend='ssh'\n",
        );
        assert_eq!(l.resolve(None).unwrap().0, "b");
        let l = loaded("[instances.only]\nbackend='ssh'\n");
        assert_eq!(l.resolve(None).unwrap().0, "only");
        let l = loaded("[instances.x]\nbackend='nope'\n");
        assert!(l.resolve(None).is_err());
    }

    #[test]
    fn unknown_keys_rejected() {
        assert!(toml::from_str::<Config>("[instances.a]\nbackend='ssh'\nbogus=1\n").is_err());
    }
}
