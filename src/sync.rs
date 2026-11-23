//! Delta sync of local source trees to a remote workspace using only `ssh`, `tar`, `find`
//! on the remote. No rsync, no git, no GitHub: uncommitted and private code is sent as-is.
//!
//! Protocol (one ssh round trip each):
//!   1. query: remote lists `size/mtime/mode` of files under each mirrored root;
//!   2. exec:  stdin = env header + tar of changed files; the remote extracts under a lock
//!      and then runs the command.

use anyhow::{Context, Result};
use ignore::overrides::OverrideBuilder;
use ignore::WalkBuilder;
use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use crate::config::SyncCfg;
use crate::util::{fnv64, shq};

/// Secrets that must not leave the machine by accident (vibe-coded projects often keep keys in
/// untracked files). Disable with `sync.default_excludes = false`.
pub const DEFAULT_EXCLUDES: [&str; 14] = [
    ".env",
    ".env.local",
    ".env.*.local",
    "*.pem",
    "*.key",
    "id_rsa*",
    "id_ed25519*",
    ".npmrc",
    // Android/Kotlin signing material and machine-local SDK paths
    "*.jks",
    "*.keystore",
    "key.properties",
    "keystore.properties",
    "local.properties",
    "google-services.json.local",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    File,
    Symlink,
}

#[derive(Debug, Clone)]
pub struct Entry {
    pub rel: String,
    pub abs: PathBuf,
    pub kind: Kind,
    pub size: u64,
    pub mtime: i64,
    pub mtime_nsec: u32,
    pub mode: u32,
    pub link: Option<String>,
}

/// A local directory mirrored on the remote at `<base>/<mirror>`.
#[derive(Debug, Clone)]
pub struct Root {
    pub path: PathBuf,
    /// Absolute local path without the leading slash.
    pub mirror: String,
    pub key: String,
    pub entries: Vec<Entry>,
}

pub fn mirror_of(path: &Path) -> Result<String> {
    let s = path
        .to_str()
        .with_context(|| format!("non-UTF-8 path: {}", path.display()))?;
    Ok(s.trim_start_matches('/').to_string())
}

pub fn root_key(path: &Path) -> String {
    format!("{:016x}", fnv64(path.as_os_str().as_encoded_bytes()))
}

/// Directories that hold Gradle/Kotlin build output or machine-local caches. `build/` is only
/// output when it sits next to a `build.gradle[.kts]` (a `build` source package must travel).
/// The remote `find` in [`query_script`] applies the same rule.
fn is_gradle_output_dir(p: &Path, name: &std::ffi::OsStr) -> bool {
    if name == ".gradle" || name == ".kotlin" {
        return true;
    }
    name == "build"
        && p.parent().is_some_and(|d| {
            d.join("build.gradle").is_file() || d.join("build.gradle.kts").is_file()
        })
}

/// Walk `root` honouring .gitignore/.ignore/.rkotlinignore; skips Gradle output dirs and `.git`.
pub fn walk(root: &Path, cfg: &SyncCfg, warnings: &mut Vec<String>) -> Result<Vec<Entry>> {
    let include_git = cfg.include_git;
    let mut wb = WalkBuilder::new(root);
    wb.hidden(false)
        .git_ignore(true)
        .git_global(false)
        .git_exclude(true)
        .require_git(false)
        .parents(true)
        .follow_links(false)
        .add_custom_ignore_filename(".rkotlinignore")
        .sort_by_file_path(|a, b| a.cmp(b))
        .filter_entry(move |e| {
            if e.depth() == 0 {
                return true;
            }
            let name = e.file_name();
            if name == ".git" && !include_git {
                return false;
            }
            let is_dir = e.file_type().is_some_and(|t| t.is_dir());
            !(is_dir && is_gradle_output_dir(e.path(), name))
        });
    let mut patterns: Vec<&str> = vec![];
    if cfg.default_excludes != Some(false) {
        patterns.extend(DEFAULT_EXCLUDES);
    }
    patterns.extend(cfg.exclude.iter().map(String::as_str));
    if !patterns.is_empty() {
        let mut ob = OverrideBuilder::new(root);
        for pat in patterns {
            ob.add(&format!("!{pat}"))
                .with_context(|| format!("bad sync.exclude pattern `{pat}`"))?;
        }
        wb.overrides(ob.build()?);
    }

    let mut out = Vec::new();
    for r in wb.build() {
        let e = match r {
            Ok(e) => e,
            Err(err) => {
                warnings.push(format!("skipping unreadable entry: {err}"));
                continue;
            }
        };
        if e.depth() == 0 {
            continue;
        }
        let Some(ft) = e.file_type() else { continue };
        if ft.is_dir() {
            continue;
        }
        let kind = if ft.is_symlink() {
            Kind::Symlink
        } else if ft.is_file() {
            Kind::File
        } else {
            continue;
        };
        let rel = match e.path().strip_prefix(root).ok().and_then(|p| p.to_str()) {
            Some(s) => s.to_string(),
            None => {
                warnings.push(format!("skipping non-UTF-8 path {}", e.path().display()));
                continue;
            }
        };
        if rel.contains('\n') {
            warnings.push(format!("skipping path containing a newline: {rel:?}"));
            continue;
        }
        let meta = match e.metadata() {
            Ok(m) => m,
            Err(err) => {
                warnings.push(format!("skipping {rel}: {err}"));
                continue;
            }
        };
        let link = if kind == Kind::Symlink {
            match std::fs::read_link(e.path()) {
                Ok(t) => t.to_str().map(str::to_string),
                Err(_) => None,
            }
        } else {
            None
        };
        if kind == Kind::Symlink && link.is_none() {
            warnings.push(format!("skipping unreadable symlink {rel}"));
            continue;
        }
        out.push(Entry {
            rel,
            abs: e.path().to_path_buf(),
            kind,
            size: if kind == Kind::File { meta.len() } else { 0 },
            mtime: meta.mtime(),
            mtime_nsec: meta.mtime_nsec() as u32,
            mode: meta.mode(),
            link,
        });
    }
    Ok(out)
}

// ---------------------------------------------------------------- remote state

#[derive(Debug, Clone, PartialEq)]
pub struct RemoteFile {
    pub kind: char,
    pub size: u64,
    pub mtime: i64,
    pub mtime_nsec: u32,
    pub exec: bool,
    pub link: String,
}

#[derive(Debug, Default)]
pub struct RemoteRoot {
    pub prev_list: Vec<String>,
    pub files: HashMap<String, RemoteFile>,
}

#[derive(Debug, Default)]
pub struct RemoteState {
    pub base: String,
    pub roots: HashMap<String, RemoteRoot>,
    /// Whether the remote has the requested JDK (the configured version, or any `java` on PATH).
    pub has_java: bool,
    /// Whether the remote has a `gradle` command (only needed for projects without `gradlew`).
    pub has_gradle: bool,
}

/// Shell expression (a single word) for the workspace directory `B`.
pub fn base_expr(base_cfg: Option<&str>, id: &str) -> String {
    match base_cfg {
        Some(b) => shq(&format!("{}/{}", b.trim_end_matches('/'), id)),
        None => format!("\"$HOME/.rkotlin/ws/{id}\""),
    }
}

pub fn query_script(
    base: &str,
    jdk: Option<u32>,
    roots: &[Root],
    include_git: bool,
    with_find: bool,
) -> String {
    let git = if include_git { "" } else { " -o -path ./.git" };
    let skip_git = if include_git { "" } else { " -o -name .git" };
    let java_test = match jdk {
        Some(v) => format!("[ -x \"$HOME/.rkotlin/jdk-{v}/bin/java\" ]"),
        None => "command -v java >/dev/null 2>&1".to_string(),
    };
    let mut s = format!(
        "B={base}\nmkdir -p \"$B/.state\"\n[ -f \"$HOME/.rkotlin/env\" ] && . \"$HOME/.rkotlin/env\"\nprintf '@@BASE\\t%s\\0' \"$B\"\nif {java_test}; then printf '@@JAVA\\t1\\0'; else printf '@@JAVA\\t0\\0'; fi\nif command -v gradle >/dev/null 2>&1; then printf '@@GRADLE\\t1\\0'; else printf '@@GRADLE\\t0\\0'; fi\n"
    );
    if !with_find {
        return s;
    }
    for r in roots {
        let k = &r.key;
        s += &format!(
            "printf '@@LIST\\t%s\\0' {k}\n[ -f \"$B/.state/{k}.list\" ] && tr '\\n' '\\0' < \"$B/.state/{k}.list\" || true\n"
        );
        // find has no "sibling file exists" test: the build/ dirs next to a build script are
        // collected first and pruned by exact path (same rule as `is_gradle_output_dir`).
        s += &format!(
            "printf '@@FIND\\t%s\\0' {k}\nif cd \"$B\"/{m} 2>/dev/null; then\n  find . \\( -name .gradle -o -name .kotlin -o -name build{skip_git} \\) -prune -o \\( -name build.gradle -o -name build.gradle.kts \\) -print |\n  {{ set -- \\( -name .gradle -o -name .kotlin{git}; while IFS= read -r f; do set -- \"$@\" -o -path \"$(dirname \"$f\")/build\"; done; set -- \"$@\" \\);\n     find . \"$@\" -prune -o ! -type d -printf '%y\\t%s\\t%T@\\t%m\\t%l\\t%P\\0'; }}\n  cd \"$B\"\nfi\n",
            m = shq(&r.mirror)
        );
    }
    s
}

pub fn parse_query(out: &[u8]) -> Result<RemoteState> {
    enum Sec {
        None,
        List(String),
        Find(String),
    }
    let mut st = RemoteState {
        has_java: true,
        has_gradle: true,
        ..Default::default()
    };
    let mut sec = Sec::None;
    for rec in out.split(|b| *b == 0) {
        if rec.is_empty() {
            continue;
        }
        let text = String::from_utf8_lossy(rec).into_owned();
        if let Some(c) = text.strip_prefix("@@GRADLE\t") {
            st.has_gradle = c == "1";
        } else if let Some(c) = text.strip_prefix("@@JAVA\t") {
            st.has_java = c == "1";
        } else if let Some(b) = text.strip_prefix("@@BASE\t") {
            st.base = b.to_string();
        } else if let Some(k) = text.strip_prefix("@@LIST\t") {
            st.roots.entry(k.to_string()).or_default();
            sec = Sec::List(k.to_string());
        } else if let Some(k) = text.strip_prefix("@@FIND\t") {
            st.roots.entry(k.to_string()).or_default();
            sec = Sec::Find(k.to_string());
        } else {
            match &sec {
                Sec::List(k) => st.roots.get_mut(k).unwrap().prev_list.push(text),
                Sec::Find(k) => {
                    let mut it = text.splitn(6, '\t');
                    let (Some(y), Some(sz), Some(mt), Some(mode), Some(link), Some(path)) = (
                        it.next(),
                        it.next(),
                        it.next(),
                        it.next(),
                        it.next(),
                        it.next(),
                    ) else {
                        continue;
                    };
                    let (secs, frac) = mt.split_once('.').unwrap_or((mt, "0"));
                    let mtime = secs.parse().unwrap_or(-1);
                    let mut digits: String = frac.chars().take(9).collect();
                    while digits.len() < 9 {
                        digits.push('0');
                    }
                    let mtime_nsec = digits.parse().unwrap_or(0);
                    let mode = u32::from_str_radix(mode, 8).unwrap_or(0);
                    st.roots.get_mut(k).unwrap().files.insert(
                        path.to_string(),
                        RemoteFile {
                            kind: y.chars().next().unwrap_or('?'),
                            size: sz.parse().unwrap_or(0),
                            mtime,
                            mtime_nsec,
                            exec: mode & 0o111 != 0,
                            link: link.to_string(),
                        },
                    );
                }
                Sec::None => {}
            }
        }
    }
    anyhow::ensure!(
        !st.base.is_empty(),
        "remote did not report its workspace directory"
    );
    Ok(st)
}

// ---------------------------------------------------------------- plan

#[derive(Debug, Default, Clone)]
pub struct SyncStats {
    pub files_total: usize,
    pub files_sent: usize,
    pub bytes_sent: u64,
    pub deleted: usize,
}

#[derive(Debug, Default)]
pub struct Plan {
    /// (root index, entry index)
    pub send: Vec<(usize, usize)>,
    /// Paths relative to the workspace base.
    pub deletions: Vec<String>,
    /// (state file name, content)
    pub state: Vec<(String, String)>,
    pub stats: SyncStats,
}

/// Files modified within this many seconds are always resent: mtime has 1 s resolution.
const HOT_SECS: i64 = 3;

fn needs_send(e: &Entry, remote: Option<&RemoteFile>, now: i64) -> bool {
    let Some(r) = remote else { return true };
    match e.kind {
        Kind::Symlink => r.kind != 'l' || Some(&r.link) != e.link.as_ref(),
        Kind::File => {
            r.kind != 'f'
                || r.size != e.size
                || r.mtime != e.mtime
                || r.mtime_nsec != e.mtime_nsec
                || r.exec != (e.mode & 0o111 != 0)
                || now - e.mtime < HOT_SECS
        }
    }
}

pub fn plan(roots: &[Root], remote: &RemoteState, now: i64) -> Plan {
    let empty = RemoteRoot::default();
    let mut p = Plan::default();
    for (ri, r) in roots.iter().enumerate() {
        let rr = remote.roots.get(&r.key).unwrap_or(&empty);
        p.stats.files_total += r.entries.len();
        for (ei, e) in r.entries.iter().enumerate() {
            if needs_send(e, rr.files.get(&e.rel), now) {
                p.send.push((ri, ei));
                p.stats.files_sent += 1;
                p.stats.bytes_sent += e.size;
            }
        }
        let local: HashSet<&str> = r.entries.iter().map(|e| e.rel.as_str()).collect();
        for old in &rr.prev_list {
            if !local.contains(old.as_str()) && rr.files.contains_key(old) {
                p.deletions.push(format!("{}/{}", r.mirror, old));
            }
        }
        let new_list: Vec<&str> = r.entries.iter().map(|e| e.rel.as_str()).collect();
        let unchanged = new_list.len() == rr.prev_list.len()
            && new_list.iter().zip(&rr.prev_list).all(|(a, b)| *a == b);
        if !unchanged {
            let mut c = new_list.join("\n");
            c.push('\n');
            p.state.push((format!(".state/{}.list", r.key), c));
        }
    }
    p.stats.deleted = p.deletions.len();
    p
}

// ---------------------------------------------------------------- stream

fn add_blob<W: Write>(b: &mut tar::Builder<W>, path: &str, data: &[u8]) -> io::Result<()> {
    let mut h = tar::Header::new_gnu();
    h.set_size(data.len() as u64);
    h.set_mode(0o644);
    h.set_uid(1000);
    h.set_gid(1000);
    h.set_mtime(crate::util::now_secs().max(0) as u64);
    h.set_entry_type(tar::EntryType::Regular);
    b.append_data(&mut h, path, data)
}

/// Writes `<len>\n<env>` followed by a tar archive of everything in `plan`.
pub fn write_stream<W: Write>(w: W, env: &str, roots: &[Root], plan: &Plan) -> io::Result<()> {
    let mut w = w;
    write!(w, "{}\n{}", env.len(), env)?;
    let mut b = tar::Builder::new(w);
    for &(ri, ei) in &plan.send {
        let r = &roots[ri];
        let e = &r.entries[ei];
        let path = format!("{}/{}", r.mirror, e.rel);
        let mut h = tar::Header::new_gnu();
        h.set_uid(1000);
        h.set_gid(1000);
        h.set_mtime(e.mtime.max(0) as u64);
        match e.kind {
            Kind::Symlink => {
                h.set_entry_type(tar::EntryType::Symlink);
                h.set_size(0);
                h.set_mode(0o777);
                b.append_link(&mut h, &path, e.link.as_deref().unwrap_or(""))?;
            }
            Kind::File => {
                h.set_entry_type(tar::EntryType::Regular);
                h.set_mode(e.mode & 0o7777);
                // Sub-second mtimes matter: cargo compares source mtime to its dep-info files.
                let precise = format!("{}.{:09}", e.mtime.max(0), e.mtime_nsec);
                b.append_pax_extensions([("mtime", precise.as_bytes())])?;
                if e.size <= 16 * 1024 * 1024 {
                    let data = std::fs::read(&e.abs)?;
                    h.set_size(data.len() as u64);
                    b.append_data(&mut h, &path, data.as_slice())?;
                } else {
                    h.set_size(e.size);
                    let f = File::open(&e.abs)?;
                    b.append_data(&mut h, &path, f.take(e.size))?;
                }
            }
        }
    }
    for (name, content) in &plan.state {
        add_blob(&mut b, name, content.as_bytes())?;
    }
    if !plan.deletions.is_empty() {
        let mut blob = Vec::new();
        for d in &plan.deletions {
            blob.extend_from_slice(d.as_bytes());
            blob.push(0);
        }
        add_blob(&mut b, ".state/deletions", &blob)?;
    }
    let mut w = b.into_inner()?;
    w.flush()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("rkotlin-test-{}-{}", name, std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn mkroot(path: &Path) -> Root {
        let mut w = vec![];
        let entries = walk(path, &SyncCfg::default(), &mut w).unwrap();
        Root {
            path: path.to_path_buf(),
            mirror: mirror_of(path).unwrap(),
            key: root_key(path),
            entries,
        }
    }

    #[test]
    fn walk_respects_ignores() {
        let d = tmp("walk");
        fs::create_dir_all(d.join("app/src/main/kotlin/com/x/build")).unwrap();
        fs::create_dir_all(d.join("app/build/outputs/apk")).unwrap();
        fs::write(d.join("app/build.gradle.kts"), "").unwrap();
        fs::write(d.join("app/build/outputs/apk/app.apk"), "x").unwrap();
        // a `build` source package (no build script next to it) must travel
        fs::write(d.join("app/src/main/kotlin/com/x/build/Info.kt"), "package com.x.build").unwrap();
        fs::create_dir_all(d.join(".gradle/8.5")).unwrap();
        fs::write(d.join(".gradle/8.5/x.bin"), "x").unwrap();
        fs::create_dir_all(d.join(".kotlin/sessions")).unwrap();
        fs::write(d.join(".kotlin/sessions/a"), "x").unwrap();
        fs::create_dir_all(d.join(".git")).unwrap();
        fs::write(d.join(".git/HEAD"), "x").unwrap();
        fs::write(d.join(".gitignore"), "secret.txt\n").unwrap();
        fs::write(d.join("secret.txt"), "x").unwrap();
        fs::write(d.join("local.properties"), "sdk.dir=/home/me/Android").unwrap();
        fs::write(d.join("settings.gradle.kts"), "include(\":app\")").unwrap();
        fs::write(d.join("gradlew"), "#!/bin/sh").unwrap();
        fs::create_dir_all(d.join("gradle/wrapper")).unwrap();
        fs::write(d.join("gradle/wrapper/gradle-wrapper.properties"), "").unwrap();
        let r = mkroot(&d);
        let names: Vec<_> = r.entries.iter().map(|e| e.rel.as_str()).collect();
        assert!(names.contains(&"settings.gradle.kts"));
        assert!(names.contains(&"app/build.gradle.kts"));
        assert!(names.contains(&"gradlew"));
        assert!(names.contains(&"gradle/wrapper/gradle-wrapper.properties"));
        assert!(
            names.contains(&"app/src/main/kotlin/com/x/build/Info.kt"),
            "source package named build must sync: {names:?}"
        );
        assert!(names.contains(&".gitignore"));
        assert!(
            !names.iter().any(|n| n.starts_with("app/build/")
                || n.starts_with(".gradle/")
                || n.starts_with(".kotlin/")
                || n.starts_with(".git/")),
            "{names:?}"
        );
        assert!(!names.contains(&"secret.txt"));
        assert!(!names.contains(&"local.properties"));
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn secrets_are_not_uploaded_by_default() {
        let d = tmp("secrets");
        fs::write(d.join(".env"), "KEY=1").unwrap();
        fs::write(d.join("server.pem"), "x").unwrap();
        fs::write(d.join(".env.example"), "KEY=").unwrap();
        fs::write(d.join("main.rs"), "x").unwrap();
        let names = |cfg: &SyncCfg| {
            let mut v: Vec<String> = walk(&d, cfg, &mut vec![])
                .unwrap()
                .into_iter()
                .map(|e| e.rel)
                .collect();
            v.sort();
            v
        };
        assert_eq!(names(&SyncCfg::default()), vec![".env.example", "main.rs"]);
        let off = SyncCfg {
            default_excludes: Some(false),
            ..Default::default()
        };
        assert_eq!(
            names(&off),
            vec![".env", ".env.example", "main.rs", "server.pem"]
        );
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn excludes_apply() {
        let d = tmp("excl");
        fs::create_dir_all(d.join("node_modules/x")).unwrap();
        fs::write(d.join("node_modules/x/a.js"), "x").unwrap();
        fs::write(d.join("keep.rs"), "x").unwrap();
        let mut w = vec![];
        let cfg = SyncCfg {
            exclude: vec!["node_modules".into()],
            ..Default::default()
        };
        let e = walk(&d, &cfg, &mut w).unwrap();
        let names: Vec<_> = e.iter().map(|e| e.rel.as_str()).collect();
        assert_eq!(names, vec!["keep.rs"]);
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn plan_is_incremental() {
        let d = tmp("plan");
        fs::write(d.join("a.rs"), "aaaa").unwrap();
        fs::write(d.join("b.rs"), "bb").unwrap();
        // make them old so the hot-file rule does not apply
        let old = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
        for n in ["a.rs", "b.rs"] {
            File::options()
                .write(true)
                .open(d.join(n))
                .unwrap()
                .set_modified(old)
                .unwrap();
        }
        let r = mkroot(&d);
        let roots = vec![r];
        let now = crate::util::now_secs();

        let first = plan(&roots, &RemoteState::default(), now);
        assert_eq!(first.stats.files_sent, 2);
        assert_eq!(first.state.len(), 1);

        // remote now mirrors everything
        let mut rr = RemoteRoot::default();
        for e in &roots[0].entries {
            rr.files.insert(
                e.rel.clone(),
                RemoteFile {
                    kind: 'f',
                    size: e.size,
                    mtime: e.mtime,
                    mtime_nsec: e.mtime_nsec,
                    exec: false,
                    link: String::new(),
                },
            );
            rr.prev_list.push(e.rel.clone());
        }
        let mut rs = RemoteState {
            base: "/b".into(),
            ..Default::default()
        };
        rs.roots.insert(roots[0].key.clone(), rr);
        let second = plan(&roots, &rs, now);
        assert_eq!(second.stats.files_sent, 0, "nothing changed");
        assert!(second.state.is_empty());
        assert!(second.deletions.is_empty());

        // delete b.rs locally -> remote deletion; touch a.rs -> resend
        fs::remove_file(d.join("b.rs")).unwrap();
        fs::write(d.join("a.rs"), "aaaaa").unwrap();
        let roots2 = vec![mkroot(&d)];
        let third = plan(&roots2, &rs, now);
        assert_eq!(third.stats.files_sent, 1);
        assert_eq!(third.deletions, vec![format!("{}/b.rs", roots2[0].mirror)]);
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn remote_only_files_are_never_deleted() {
        let d = tmp("remote-only");
        fs::write(d.join("a.rs"), "a").unwrap();
        let roots = vec![mkroot(&d)];
        let mut rr = RemoteRoot::default();
        // Cargo.lock was generated remotely; it is not in prev_list
        rr.files.insert(
            "Cargo.lock".into(),
            RemoteFile {
                kind: 'f',
                size: 1,
                mtime: 1,
                mtime_nsec: 0,
                exec: false,
                link: String::new(),
            },
        );
        let mut rs = RemoteState {
            base: "/b".into(),
            ..Default::default()
        };
        rs.roots.insert(roots[0].key.clone(), rr);
        let p = plan(&roots, &rs, crate::util::now_secs());
        assert!(p.deletions.is_empty());
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn stream_roundtrip() {
        let d = tmp("stream");
        fs::create_dir_all(d.join("src")).unwrap();
        fs::write(d.join("src/main.rs"), "fn main(){}").unwrap();
        fs::write(d.join("run.sh"), "#!/bin/sh").unwrap();
        fs::set_permissions(
            d.join("run.sh"),
            std::os::unix::fs::PermissionsExt::from_mode(0o755),
        )
        .unwrap();
        std::os::unix::fs::symlink("src/main.rs", d.join("link")).unwrap();
        let roots = vec![mkroot(&d)];
        let p = plan(&roots, &RemoteState::default(), crate::util::now_secs());
        let mut buf = Vec::new();
        write_stream(&mut buf, "export A=1\n", &roots, &p).unwrap();

        let nl = buf.iter().position(|b| *b == b'\n').unwrap();
        let n: usize = std::str::from_utf8(&buf[..nl]).unwrap().parse().unwrap();
        assert_eq!(&buf[nl + 1..nl + 1 + n], b"export A=1\n");
        let out = tmp("stream-out");
        tar::Archive::new(&buf[nl + 1 + n..]).unpack(&out).unwrap();
        let m = &roots[0].mirror;
        assert_eq!(
            fs::read(out.join(m).join("src/main.rs")).unwrap(),
            b"fn main(){}"
        );
        let mode = fs::metadata(out.join(m).join("run.sh")).unwrap().mode();
        assert!(mode & 0o111 != 0, "exec bit preserved");
        assert_eq!(
            fs::read_link(out.join(m).join("link")).unwrap(),
            PathBuf::from("src/main.rs")
        );
        assert!(out
            .join(".state")
            .join(format!("{}.list", roots[0].key))
            .is_file());
        let _ = fs::remove_dir_all(&d);
        let _ = fs::remove_dir_all(&out);
    }

    #[test]
    fn parses_query_output() {
        let mut out = Vec::new();
        out.extend_from_slice(b"@@BASE\t/data/home/.rkotlin/ws/x\0");
        out.extend_from_slice(b"@@LIST\tk1\0a.rs\0b.rs\0");
        out.extend_from_slice(b"@@FIND\tk1\0");
        out.extend_from_slice(b"f\t4\t1731567890.1234500000\t755\t\ta.rs\0");
        out.extend_from_slice(b"l\t8\t1731567891.0000000000\t777\tsrc/x\tlnk\0");
        let st = parse_query(&out).unwrap();
        assert_eq!(st.base, "/data/home/.rkotlin/ws/x");
        let r = &st.roots["k1"];
        assert_eq!(r.prev_list, vec!["a.rs", "b.rs"]);
        assert_eq!(r.files["a.rs"].mtime, 1731567890);
        assert_eq!(r.files["a.rs"].mtime_nsec, 123_450_000);
        assert!(r.files["a.rs"].exec);
        assert_eq!(r.files["lnk"].link, "src/x");
        assert!(parse_query(b"").is_err());
    }
}
