//! Agent skills bundled into the binary: the official Kotlin skills from JetBrains
//! (github.com/Kotlin/kotlin-agent-skills) and the build-related Android skills from Google
//! (github.com/android/skills), both Apache-2.0. They are vendored unchanged under `skills/kotlin/`
//! (with their LICENSE.txt), embedded by `build.rs`, and installed by `rkotlin init` or
//! `rkotlin skills add` into `.claude/skills` and `.agents/skills`.

use anyhow::{bail, Result};
use std::path::{Path, PathBuf};

mod table {
    include!(concat!(env!("OUT_DIR"), "/bundled_skills.rs"));
}

const SOURCES_TSV: &str = include_str!("../skills/kotlin/SOURCES.tsv");

pub struct Skill {
    pub name: &'static str,
    pub description: String,
    /// "JetBrains" or "Google (Android)".
    pub origin: &'static str,
    pub url: &'static str,
    /// (path inside the skill folder, bytes, executable)
    pub files: Vec<(&'static str, &'static [u8], bool)>,
}

/// The `description:` of a SKILL.md frontmatter: single line, `>` folded block or plain continuation lines.
pub fn description_of(md: &str) -> String {
    let mut lines = md.lines();
    if lines.next().map(str::trim_end) != Some("---") {
        return String::new();
    }
    let mut desc = String::new();
    let mut in_desc = false;
    for l in lines {
        if l.trim_end() == "---" {
            break;
        }
        if in_desc {
            if l.starts_with(' ') || l.starts_with('\t') {
                if !desc.is_empty() {
                    desc.push(' ');
                }
                desc.push_str(l.trim());
                continue;
            }
            break;
        }
        if let Some(rest) = l.strip_prefix("description:") {
            in_desc = true;
            let r = rest.trim();
            if !matches!(r, ">" | "|" | ">-" | "|-") {
                desc.push_str(r);
            }
        }
    }
    desc.trim()
        .trim_matches(|c| c == '"' || c == '\'')
        .to_string()
}

pub fn bundled() -> Vec<Skill> {
    let mut skills: Vec<Skill> = vec![];
    for line in SOURCES_TSV.lines().filter(|l| !l.trim().is_empty()) {
        let mut it = line.split('\t');
        let (Some(name), Some(origin), Some(url)) = (it.next(), it.next(), it.next()) else {
            continue;
        };
        let origin: &'static str = match origin {
            "JetBrains" => "JetBrains",
            _ => "Google (Android)",
        };
        let files: Vec<(&'static str, &'static [u8], bool)> = table::FILES
            .iter()
            .filter_map(|(rel, bytes, exec)| {
                rel.strip_prefix(name)
                    .and_then(|r| r.strip_prefix('/'))
                    .map(|r| (r, *bytes, *exec))
            })
            .collect();
        let description = files
            .iter()
            .find(|f| f.0 == "SKILL.md")
            .map(|f| description_of(&String::from_utf8_lossy(f.1)))
            .unwrap_or_default();
        skills.push(Skill {
            name,
            description,
            origin,
            url,
            files,
        });
    }
    skills
}

/// Write the named skills (all of them when `names` is empty) into `<base>/.claude/skills` and
/// `<base>/.agents/skills`. Returns the skill folders written.
pub fn install(base: &Path, names: &[String]) -> Result<Vec<PathBuf>> {
    let all = bundled();
    let chosen: Vec<&Skill> = if names.is_empty() {
        all.iter().collect()
    } else {
        let mut v = vec![];
        for n in names {
            match all.iter().find(|s| s.name == n) {
                Some(s) => v.push(s),
                None => bail!(
                    "no bundled skill named `{n}`. Available: {}",
                    all.iter().map(|s| s.name).collect::<Vec<_>>().join(", ")
                ),
            }
        }
        v
    };
    let mut dirs = vec![];
    for s in chosen {
        for root in [".claude/skills", ".agents/skills"] {
            let dir = base.join(root).join(s.name);
            for (rel, bytes, exec) in &s.files {
                let dest = dir.join(rel);
                if let Some(d) = dest.parent() {
                    std::fs::create_dir_all(d)?;
                }
                std::fs::write(&dest, bytes)?;
                if *exec {
                    use std::os::unix::fs::PermissionsExt;
                    std::fs::set_permissions(&dest, std::fs::Permissions::from_mode(0o755))?;
                }
            }
            dirs.push(dir);
        }
    }
    Ok(dirs)
}

pub fn list() -> String {
    let mut s = String::new();
    for sk in bundled() {
        let d: String = sk.description.chars().take(150).collect();
        let ellipsis = if sk.description.chars().count() > 150 {
            "..."
        } else {
            ""
        };
        s += &format!(
            "{:<52} {}\n    {d}{ellipsis}\n    {}\n",
            sk.name, sk.origin, sk.url
        );
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frontmatter_descriptions() {
        assert_eq!(
            description_of("---\nname: a\ndescription: >\n  Does a thing.\n  Use when x.\nlicense: MIT\n---\nbody"),
            "Does a thing. Use when x."
        );
        assert_eq!(
            description_of("---\nname: a\ndescription: One line.\nlicense: MIT\n---\n"),
            "One line."
        );
        assert_eq!(
            description_of("---\ndescription: 'Quoted.'\n---\n"),
            "Quoted."
        );
        assert_eq!(
            description_of(
                "---\nname: a\ndescription: Plain start\n  and continuation.\nlicense: x\n---\n"
            ),
            "Plain start and continuation."
        );
        assert_eq!(description_of("no frontmatter"), "");
    }

    #[test]
    fn every_bundled_skill_is_complete_and_attributed() {
        let all = bundled();
        assert_eq!(all.len(), 13, "10 JetBrains + 3 Google skills");
        for s in &all {
            let md = s
                .files
                .iter()
                .find(|f| f.0 == "SKILL.md")
                .unwrap_or_else(|| panic!("{} has no SKILL.md", s.name));
            let text = String::from_utf8_lossy(md.1);
            assert!(
                text.contains(&format!("name: {}", s.name)),
                "frontmatter name must equal the folder name for {}",
                s.name
            );
            assert!(s.description.len() > 40, "{}: `{}`", s.name, s.description);
            assert!(
                s.files.iter().any(|f| f.0 == "LICENSE.txt"),
                "{} must ship its Apache-2.0 license",
                s.name
            );
            assert!(s.url.starts_with("https://github.com/"), "{}", s.url);
        }
        assert_eq!(all.iter().filter(|s| s.origin == "JetBrains").count(), 10);
    }

    #[test]
    fn install_writes_both_agent_dirs_and_keeps_scripts_executable() {
        let d = std::env::temp_dir().join(format!("rkotlin-skills-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        let dirs = install(
            &d,
            &["kotlin-tooling-agp9-migration".into(), "r8-analyzer".into()],
        )
        .unwrap();
        assert_eq!(dirs.len(), 4);
        for root in [".claude/skills", ".agents/skills"] {
            assert!(d.join(root).join("r8-analyzer/SKILL.md").is_file());
            let script = d
                .join(root)
                .join("kotlin-tooling-agp9-migration/scripts/analyze-project.sh");
            assert!(script.is_file());
            use std::os::unix::fs::PermissionsExt;
            assert!(std::fs::metadata(script).unwrap().permissions().mode() & 0o111 != 0);
        }
        let e = install(&d, &["nope".into()]).unwrap_err().to_string();
        assert!(
            e.contains("Available:") && e.contains("kotlin-tooling-java-to-kotlin"),
            "{e}"
        );
        let _ = std::fs::remove_dir_all(&d);
    }
}
