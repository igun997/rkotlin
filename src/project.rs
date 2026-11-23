//! Project discovery: which directory is the Gradle root, which extra directories
//! (`includeBuild("../x")` composite builds and `projectDir = file("../x")` modules, possibly
//! private and never on GitHub) must travel with it.

use anyhow::{bail, Context, Result};
use std::collections::VecDeque;
use std::path::{Path, PathBuf};

use crate::config::SyncCfg;
use crate::sync::{self, Root};
use crate::util::fnv64;

#[derive(Debug, Clone)]
pub struct Project {
    pub root: PathBuf,
    /// Current directory relative to `root` ("" at the root).
    pub cwd_rel: String,
    /// Stable id of this project's remote workspace.
    pub id: String,
    /// The root has an executable-or-not `gradlew` wrapper script.
    pub has_wrapper: bool,
}

pub const SETTINGS_FILES: [&str; 2] = ["settings.gradle.kts", "settings.gradle"];
pub const BUILD_FILES: [&str; 2] = ["build.gradle.kts", "build.gradle"];

fn has_any(dir: &Path, names: &[&str]) -> bool {
    names.iter().any(|n| dir.join(n).is_file())
}

/// Gradle looks for `settings.gradle[.kts]` upwards from the current directory and uses the nearest;
/// a directory with only a build script is a single-project build.
pub fn discover(cwd: &Path) -> Result<Project> {
    let cwd = cwd
        .canonicalize()
        .with_context(|| format!("cannot resolve {}", cwd.display()))?;
    let root = cwd
        .ancestors()
        .find(|d| has_any(d, &SETTINGS_FILES))
        .or_else(|| cwd.ancestors().find(|d| has_any(d, &BUILD_FILES)))
        .map(Path::to_path_buf);
    let Some(root) = root else {
        bail!(
            "{} is not inside a Gradle project (no settings.gradle[.kts] or build.gradle[.kts] found upwards)",
            cwd.display()
        );
    };
    let cwd_rel = cwd
        .strip_prefix(&root)
        .ok()
        .and_then(|p| p.to_str())
        .unwrap_or("")
        .to_string();
    let name: String = root
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("project")
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect();
    let id = format!(
        "{}-{:012x}",
        name,
        fnv64(root.as_os_str().as_encoded_bytes()) & 0xffff_ffff_ffff
    );
    let has_wrapper = root.join("gradlew").is_file();
    Ok(Project {
        root,
        cwd_rel,
        id,
        has_wrapper,
    })
}

/// Quoted string literals ('...' or "...") on one line of a Gradle script.
fn quoted(line: &str) -> Vec<String> {
    let mut out = vec![];
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '"' || c == '\'' {
            let mut s = String::new();
            for d in chars.by_ref() {
                if d == c {
                    out.push(s);
                    break;
                }
                s.push(d);
            }
        }
    }
    out
}

/// Directories a settings script pulls in from outside the project: `includeBuild("../x")`
/// (also inside `pluginManagement`) and `project(":m").projectDir = file("../m")`.
/// Only literal relative paths are understood; anything with `$` interpolation is skipped.
pub fn external_dirs(settings: &str) -> Vec<String> {
    let mut out = vec![];
    for line in settings.lines() {
        let code = line.split("//").next().unwrap_or("");
        if !(code.contains("includeBuild") || code.contains("projectDir")) {
            continue;
        }
        for q in quoted(code) {
            if q.contains('$') || q.is_empty() || q.starts_with(':') {
                continue;
            }
            // a path, not a module name or plain word like "settingsDir"
            if q.starts_with('.') || q.contains('/') {
                out.push(q);
            }
        }
    }
    out.sort();
    out.dedup();
    out
}

/// An included build may itself be a subdirectory of a bigger checkout (its own settings
/// file decides); ship the directory itself, plus nothing else.
fn contains(outer: &Path, inner: &Path) -> bool {
    inner.starts_with(outer)
}

/// Walk the project root plus every external included-build root, breadth first.
pub fn collect_roots(
    project: &Project,
    cfg: &SyncCfg,
    warnings: &mut Vec<String>,
) -> Result<Vec<Root>> {
    let follow = cfg.path_deps.unwrap_or(true);
    let mut roots: Vec<Root> = Vec::new();
    let mut queue: VecDeque<PathBuf> = VecDeque::from([project.root.clone()]);
    while let Some(path) = queue.pop_front() {
        if roots.iter().any(|r| contains(&r.path, &path)) {
            continue;
        }
        let entries = sync::walk(&path, cfg, warnings)?;
        if follow {
            for e in &entries {
                let name = e.rel.rsplit('/').next().unwrap_or(&e.rel);
                if !SETTINGS_FILES.contains(&name) {
                    continue;
                }
                let Ok(text) = std::fs::read_to_string(&e.abs) else {
                    continue;
                };
                let sdir = e.abs.parent().unwrap_or(&path);
                for dep in external_dirs(&text) {
                    let target = match sdir.join(&dep).canonicalize() {
                        Ok(t) if t.is_dir() => t,
                        _ => {
                            warnings.push(format!(
                                "included path `{dep}` of {} does not exist locally",
                                e.abs.display()
                            ));
                            continue;
                        }
                    };
                    if contains(&path, &target) || roots.iter().any(|r| contains(&r.path, &target))
                    {
                        continue;
                    }
                    queue.push_back(target);
                }
            }
        }
        roots.push(Root {
            mirror: sync::mirror_of(&path)?,
            key: sync::root_key(&path),
            path,
            entries,
        });
    }
    // a later root may contain an earlier one: keep the outermost only
    let paths: Vec<PathBuf> = roots.iter().map(|r| r.path.clone()).collect();
    roots.retain(|r| !paths.iter().any(|p| p != &r.path && contains(p, &r.path)));
    Ok(roots)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn finds_external_dirs_in_kts_and_groovy() {
        let s = r#"
rootProject.name = "app"
pluginManagement { includeBuild("../build-logic") }
includeBuild("../shared-lib")
includeBuild '../groovy-lib'
include(":core")
project(":core").projectDir = file("../core")
project(':old').projectDir = new File(settingsDir, '../old')
includeBuild("libs/inner") // inside the project is fine, filtered later
includeBuild("$rootDir/x")
// includeBuild("../commented")
val x = "../not-a-build"
"#;
        assert_eq!(
            external_dirs(s),
            vec![
                "../build-logic",
                "../core",
                "../groovy-lib",
                "../old",
                "../shared-lib",
                "libs/inner",
            ]
        );
    }

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("rkotlin-proj-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d.canonicalize().unwrap()
    }

    #[test]
    fn discovers_nearest_settings_from_a_module_dir() {
        let d = tmp("disc");
        fs::create_dir_all(d.join("app/src")).unwrap();
        fs::write(d.join("settings.gradle.kts"), "include(\":app\")").unwrap();
        fs::write(d.join("build.gradle.kts"), "").unwrap();
        fs::write(d.join("app/build.gradle.kts"), "").unwrap();
        fs::write(d.join("gradlew"), "#!/bin/sh").unwrap();
        let p = discover(&d.join("app/src")).unwrap();
        assert_eq!(p.root, d);
        assert_eq!(p.cwd_rel, "app/src");
        assert!(p.has_wrapper);
    }

    #[test]
    fn single_project_without_settings_uses_build_script() {
        let d = tmp("single");
        fs::write(d.join("build.gradle"), "").unwrap();
        let p = discover(&d).unwrap();
        assert_eq!(p.root, d);
        assert!(!p.has_wrapper);
    }

    #[test]
    fn rejects_non_gradle_dirs() {
        let d = tmp("none");
        fs::write(d.join("Cargo.toml"), "").unwrap();
        assert!(discover(&d).is_err());
    }

    #[test]
    fn included_build_outside_root_travels_along() {
        let d = tmp("roots");
        fs::create_dir_all(d.join("app")).unwrap();
        fs::create_dir_all(d.join("shared/src")).unwrap();
        fs::write(
            d.join("app/settings.gradle.kts"),
            "includeBuild(\"../shared\")\nincludeBuild(\"../missing\")",
        )
        .unwrap();
        fs::write(d.join("app/build.gradle.kts"), "").unwrap();
        fs::write(d.join("shared/src/A.kt"), "class A").unwrap();
        fs::write(d.join("shared/settings.gradle.kts"), "").unwrap();
        let p = discover(&d.join("app")).unwrap();
        let mut w = vec![];
        let roots = collect_roots(&p, &SyncCfg::default(), &mut w).unwrap();
        let paths: Vec<_> = roots.iter().map(|r| r.path.clone()).collect();
        assert_eq!(paths, vec![d.join("app"), d.join("shared")]);
        assert!(w.iter().any(|m| m.contains("../missing")));
    }
}
