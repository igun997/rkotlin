//! Orchestration: pick instance → make it reachable → delta sync → run gradle/shell remotely,
//! streaming output with remote paths mapped back to local paths.

use anyhow::{bail, Result};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::mpsc;
use std::time::Instant;

use crate::backend;
use crate::config::{Instance, Loaded, RemoteCfg, SyncCfg};
use crate::project::{self, Project};
use crate::sync::{self, Root, SyncStats};
use crate::transport::{exit_code, Ssh};
use crate::util::{self, shq, strip_all};

pub struct Session {
    pub inst_name: String,
    pub inst: Instance,
    pub sync_cfg: SyncCfg,
    pub remote_cfg: RemoteCfg,
    pub project: Project,
    pub ssh: Ssh,
    pub roots: Vec<Root>,
    pub warnings: Vec<String>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Stream {
    Out,
    Err,
}

pub enum Cmd {
    Gradle(Vec<String>),
    Shell(String),
}

pub struct ExecSpec {
    pub cmd: Cmd,
    pub sync: bool,
}

pub struct ExecResult {
    pub code: i32,
    pub sync: SyncStats,
    pub sync_secs: f64,
    pub total_secs: f64,
}

/// Resolve the instance, make it reachable, scan the project.
pub fn open(loaded: &Loaded, requested: Option<&str>, cwd: &Path) -> Result<Session> {
    let (inst_name, inst) = loaded.resolve(requested)?;
    let project = project::discover(cwd)?;
    let backend = backend::make(&inst_name, &inst)?;
    info!("instance `{inst_name}` ({})", inst.backend);
    let ssh = backend.ensure()?;
    ssh.ensure_master();
    let mut warnings = vec![];
    let roots = project::collect_roots(&project, &loaded.cfg.sync, &mut warnings)?;
    Ok(Session {
        inst_name,
        inst,
        sync_cfg: loaded.cfg.sync.clone(),
        remote_cfg: loaded.cfg.remote.clone(),
        project,
        ssh,
        roots,
        warnings,
    })
}

impl Session {
    pub fn base_expr(&self) -> String {
        sync::base_expr(self.inst.base.as_deref(), &self.project.id)
    }

    fn mirror_cwd(&self) -> String {
        let m = sync::mirror_of(&self.project.root).unwrap_or_default();
        if self.project.cwd_rel.is_empty() {
            m
        } else {
            format!("{m}/{}", self.project.cwd_rel)
        }
    }

    fn env_block(&self, extra: &[(String, String)]) -> Result<String> {
        let mut vars: Vec<(String, String)> = vec![("RKOTLIN".into(), "1".into())];
        for (k, v) in self.inst.env.iter().chain(self.remote_cfg.env.iter()) {
            vars.push((k.clone(), v.clone()));
        }
        for k in self
            .inst
            .forward_env
            .iter()
            .chain(self.remote_cfg.forward_env.iter())
        {
            match std::env::var(k) {
                Ok(v) => vars.push((k.clone(), v)),
                Err(_) => info!("forward_env `{k}` is not set locally; skipping"),
            }
        }
        vars.extend(extra.iter().cloned());
        let mut s = String::new();
        for (k, v) in vars {
            if !util::valid_env_key(&k) {
                bail!("invalid environment variable name `{k}`");
            }
            s += &format!("export {k}={}\n", shq(&v));
        }
        Ok(s)
    }

    fn remote_script(&self, cmd: &Cmd) -> String {
        let origin = shq(&self.project.root.to_string_lossy());
        let root = shq(&sync::mirror_of(&self.project.root).unwrap_or_default());
        let exec_line = match cmd {
            Cmd::Gradle(args) => {
                let rel = &self.project.cwd_rel;
                let mut a: Vec<String> = vec![];
                if !args.iter().any(|x| x.starts_with("--console")) {
                    a.push("--console=plain".into());
                }
                if !rel.is_empty()
                    && !args
                        .iter()
                        .any(|x| x == "-p" || x.starts_with("--project-dir"))
                {
                    a.push("-p".into());
                    a.push(rel.clone());
                }
                a.extend(args.iter().cloned());
                let a: Vec<String> = a.iter().map(|x| shq(x)).collect();
                let pick = match self.remote_cfg.gradle.as_deref() {
                    Some(g) => format!("G={}", shq(g)),
                    None => r#"if [ -f ./gradlew ]; then chmod +x ./gradlew 2>/dev/null || true; G=./gradlew
elif command -v gradle >/dev/null 2>&1; then G=gradle
else echo "rkotlin: the project has no gradlew and the remote has no gradle. Run `rkotlin remote setup`." >&2; exit 127; fi"#
                        .to_string(),
                };
                format!("cd \"$B\"/{root}\n{pick}\nexec \"$G\" {}", a.join(" "))
            }
            Cmd::Shell(c) => format!("exec bash -lc {}", shq(c)),
        };
        let jdk = match self.remote_cfg.jdk {
            Some(v) => format!(
                "if [ -x \"$HOME/.rkotlin/jdk-{v}/bin/java\" ]; then export JAVA_HOME=\"$HOME/.rkotlin/jdk-{v}\"; export PATH=\"$JAVA_HOME/bin:$PATH\"; fi"
            ),
            None => String::new(),
        };
        format!(
            r#"set -e
IFS= read -r __n
eval "$(head -c "$__n")"
[ -f "$HOME/.rkotlin/env" ] && . "$HOME/.rkotlin/env"
{jdk}
B={base}
mkdir -p "$B/.state"
exec 9>"$B/.state/lock"
if ! flock -w 1800 9; then echo "rkotlin: gave up waiting for the remote workspace lock" >&2; exit 75; fi
cd "$B"
tar -xf - --no-same-owner
if [ -f .state/deletions ]; then
  while IFS= read -r -d '' p; do rm -f -- "$B/$p"; done < .state/deletions
  rm -f .state/deletions
fi
printf '%s\n' {origin} > .state/origin
touch .state/last-used
cd "$B"/{cwd}
{exec_line}
"#,
            base = self.base_expr(),
            cwd = shq(&self.mirror_cwd()),
        )
    }

    /// Install a JDK (and Gradle, optionally the Android SDK) on the remote if missing. Idempotent.
    pub fn bootstrap(&self, gradle: bool, android: bool) -> Result<()> {
        let script = setup_script(&self.remote_cfg, gradle, android);
        let (code, out, err) = self.ssh.capture(&script, None)?;
        if code != 0 {
            info!("{}", String::from_utf8_lossy(&err).trim());
            return Err(crate::guidance::toolchain_missing(&self.inst_name).into_err());
        }
        for l in String::from_utf8_lossy(&out).lines() {
            info!("remote: {l}");
        }
        Ok(())
    }

    /// Ask the remote what it already has (one ssh round trip).
    fn query(&self, with_find: bool) -> Result<sync::RemoteState> {
        let script = sync::query_script(
            &self.base_expr(),
            self.remote_cfg.jdk,
            &self.roots,
            self.sync_cfg.include_git,
            with_find,
        );
        let (code, out, err) = self.ssh.capture(&script, None)?;
        if code != 0 {
            bail!(
                "remote query failed (exit {code}): {}{}",
                String::from_utf8_lossy(&err).trim(),
                hint_for(code)
            );
        }
        sync::parse_query(&out)
    }

    pub fn sync_only(&self) -> Result<ExecResult> {
        self.exec(
            &ExecSpec {
                cmd: Cmd::Shell("true".into()),
                sync: true,
            },
            &mut |_, _| {},
        )
    }

    pub fn exec(
        &self,
        spec: &ExecSpec,
        on_line: &mut dyn FnMut(Stream, &[u8]),
    ) -> Result<ExecResult> {
        let t0 = Instant::now();
        for w in &self.warnings {
            info!("warning: {w}");
        }
        let remote = self.query(spec.sync)?;
        if matches!(spec.cmd, Cmd::Gradle(_))
            && (!remote.has_java || (!self.project.has_wrapper && !remote.has_gradle))
        {
            info!("first use: installing a JDK{} on `{}` (one time only, takes a minute or two)", if self.project.has_wrapper { "" } else { " and Gradle" }, self.inst_name);
            self.bootstrap(!self.project.has_wrapper, false)?;
        }
        let plan = if spec.sync {
            sync::plan(&self.roots, &remote, util::now_secs())
        } else {
            sync::Plan::default()
        };
        let script = self.remote_script(&spec.cmd);
        let env = self.env_block(&[])?;
        let base_bytes = remote.base.clone().into_bytes();
        let sync_secs = t0.elapsed().as_secs_f64();
        if spec.sync {
            info!(
                "sync: {} of {} files ({}){} in {:.1}s",
                plan.stats.files_sent,
                plan.stats.files_total,
                util::human_bytes(plan.stats.bytes_sent),
                if plan.stats.deleted > 0 {
                    format!(", {} removed", plan.stats.deleted)
                } else {
                    String::new()
                },
                sync_secs
            );
        }

        let mut child = self
            .ssh
            .remote(&script)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(crate::transport::spawn_err)?;
        let stdin = child.stdin.take().expect("piped stdin");
        let stdout = child.stdout.take().expect("piped stdout");
        let stderr = child.stderr.take().expect("piped stderr");
        let (tx, rx) = mpsc::channel::<(Stream, Vec<u8>)>();

        let roots = &self.roots;
        let plan_ref = &plan;
        let env_ref = &env;
        std::thread::scope(|s| {
            let writer = s.spawn(move || sync::write_stream(stdin, env_ref, roots, plan_ref));
            for (stream, pipe) in [
                (
                    Stream::Out,
                    Box::new(stdout) as Box<dyn std::io::Read + Send>,
                ),
                (
                    Stream::Err,
                    Box::new(stderr) as Box<dyn std::io::Read + Send>,
                ),
            ] {
                let tx = tx.clone();
                s.spawn(move || {
                    let mut r = BufReader::new(pipe);
                    loop {
                        let mut buf = Vec::new();
                        match r.read_until(b'\n', &mut buf) {
                            Ok(0) | Err(_) => break,
                            Ok(_) => {
                                if tx.send((stream, buf)).is_err() {
                                    break;
                                }
                            }
                        }
                    }
                });
            }
            drop(tx);
            for (stream, line) in rx {
                let mapped = strip_all(&line, &base_bytes);
                on_line(stream, &mapped);
            }
            match writer.join() {
                Ok(Ok(())) => {}
                // remote exited before reading everything (e.g. cargo not found): not our error
                Ok(Err(e)) if e.kind() == std::io::ErrorKind::BrokenPipe => {}
                Ok(Err(e)) => info!("sync stream error: {e}"),
                Err(_) => info!("sync writer panicked"),
            }
        });
        let status = child.wait()?;
        Ok(ExecResult {
            code: exit_code(status),
            sync: plan.stats.clone(),
            sync_secs,
            total_secs: t0.elapsed().as_secs_f64(),
        })
    }

    /// Fetch files the remote produced into the same places locally: build outputs (apk/aab/aar,
    /// jars, distributions), or with `wrapper` only the files `gradle wrapper` writes
    /// (gradlew, gradlew.bat, gradle/wrapper/*). Returns the files written.
    pub fn pull(&self, wrapper: bool) -> Result<Vec<PathBuf>> {
        let root = &self.project.root;
        let m = sync::mirror_of(root)?;
        let what = if wrapper {
            "-path './gradlew' -o -path './gradlew.bat' -o -path './gradle/wrapper/*'"
        } else {
            "-path '*/build/outputs/apk/*' -o -path '*/build/outputs/bundle/*' -o -path '*/build/outputs/aar/*' -o -path '*/build/libs/*' -o -path '*/build/distributions/*'"
        };
        let script = format!(
            "cd {base}/{m} 2>/dev/null || exit 0\nfind . -type f \\( {what} \\) -print0 | tar --null -T - -cf - 2>/dev/null || true\n",
            base = self.base_expr(),
            m = shq(&m)
        );
        let mut child = self
            .ssh
            .remote(&script)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()?;
        let mut files: Vec<PathBuf> = vec![];
        {
            let mut ar = tar::Archive::new(child.stdout.take().unwrap());
            for entry in ar.entries()? {
                let mut entry = entry?;
                // tar stores `./app/build/...`; drop the `.` so the path is clean (adb prints it)
                let rel: PathBuf = entry
                    .path()?
                    .components()
                    .filter(|c| !matches!(c, std::path::Component::CurDir))
                    .collect();
                if rel
                    .components()
                    .any(|c| matches!(c, std::path::Component::ParentDir))
                    || !entry.header().entry_type().is_file()
                {
                    continue;
                }
                let dest = root.join(&rel);
                if !dest.starts_with(root) {
                    continue;
                }
                if let Some(d) = dest.parent() {
                    std::fs::create_dir_all(d)?;
                }
                let mode = entry.header().mode().unwrap_or(0o644);
                let _ = std::fs::remove_file(&dest);
                entry.unpack(&dest)?;
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    std::fs::set_permissions(
                        &dest,
                        std::fs::Permissions::from_mode(mode & 0o7777),
                    )?;
                }
                info!("pulled {}", dest.display());
                files.push(dest);
            }
        }
        let st = child.wait()?;
        if !st.success() {
            bail!("remote tar failed (exit {})", exit_code(st));
        }
        Ok(files)
    }
}

pub fn hint_for(code: i32) -> &'static str {
    match code {
        127 => "\nhint: no JDK/Gradle was found on the remote. Run `rkotlin remote setup` to install them.",
        255 => "\nhint: ssh could not connect. Run `rkotlin remote doctor`.",
        75 => "\nhint: another build holds this workspace's lock; retry shortly.",
        _ => "",
    }
}

/// Script that lists every workspace on the instance: id, origin, size, last-used epoch.
pub fn workspaces_script(base_cfg: Option<&str>) -> String {
    let root = match base_cfg {
        Some(b) => shq(b.trim_end_matches('/')),
        None => "\"$HOME/.rkotlin/ws\"".into(),
    };
    format!(
        r#"R={root}
[ -d "$R" ] || exit 0
for d in "$R"/*/; do
  [ -d "$d" ] || continue
  id=$(basename "$d")
  origin=$(cat "$d/.state/origin" 2>/dev/null || echo '?')
  kb=$(du -sk "$d" 2>/dev/null | cut -f1)
  last=$(stat -c %Y "$d/.state/last-used" 2>/dev/null || echo 0)
  printf '%s\t%s\t%s\t%s\n' "$id" "$origin" "${{kb:-0}}" "$last"
done
"#
    )
}

pub struct WorkspaceInfo {
    pub id: String,
    pub origin: String,
    pub kib: u64,
    pub last_used: i64,
}

pub fn parse_workspaces(out: &str) -> Vec<WorkspaceInfo> {
    out.lines()
        .filter_map(|l| {
            let mut it = l.splitn(4, '\t');
            Some(WorkspaceInfo {
                id: it.next()?.to_string(),
                origin: it.next()?.to_string(),
                kib: it.next()?.parse().ok()?,
                last_used: it.next()?.parse().ok()?,
            })
        })
        .collect()
}

/// Shell script that installs a JDK (Temurin, no root needed), optionally Gradle and the Android
/// SDK, and records them in `~/.rkotlin/env`, which every remote command sources. Idempotent.
pub fn setup_script(cfg: &crate::config::RemoteCfg, gradle: bool, android: bool) -> String {
    let jdk = cfg.jdk.unwrap_or(crate::config::DEFAULT_JDK);
    let gradle_version = cfg.gradle_version.as_deref().unwrap_or("");
    let packages: Vec<String> = if cfg.android_packages.is_empty() {
        ["platform-tools", "platforms;android-35", "build-tools;35.0.0"]
            .iter()
            .map(|s| s.to_string())
            .collect()
    } else {
        cfg.android_packages.clone()
    };
    let pk: Vec<String> = packages.iter().map(|p| shq(p)).collect();
    SETUP_TEMPLATE
        .replace("__JDK__", &jdk.to_string())
        .replace("__GRADLE__", if gradle { "1" } else { "0" })
        .replace("__GRADLE_VERSION__", &shq(gradle_version))
        .replace("__ANDROID__", if android { "1" } else { "0" })
        .replace("__PACKAGES__", &pk.join(" "))
}

const SETUP_TEMPLATE: &str = r#"set -e
S=""; [ "$(id -u)" = 0 ] || S="sudo"
need=0
for t in curl tar unzip flock find git; do
  command -v $t >/dev/null 2>&1 || need=1
done
if [ "$need" = 1 ]; then
  echo "installing base tools (curl, unzip, git, util-linux)..."
  $S apt-get update -y >/dev/null && $S apt-get install -y curl ca-certificates tar unzip util-linux findutils git >/dev/null
fi
H="$HOME/.rkotlin"; mkdir -p "$H"
CURL="curl -fsSL --retry 3 --connect-timeout 20"
JDK=__JDK__
J="$H/jdk-$JDK"
# Some sandboxes only allow a few hosts (api.adoptium.net and services.gradle.org are often
# blocked while github.com and the distro mirrors work), so every download has a fallback.
fetch_jdk() {
  case "$(uname -m)" in x86_64) a=x64 ;; aarch64|arm64) a=aarch64 ;; *) echo "unsupported CPU $(uname -m)" >&2; return 1 ;; esac
  rm -f "$J.tgz"
  $CURL -o "$J.tgz" "https://api.adoptium.net/v3/binary/latest/$JDK/ga/linux/$a/jdk/hotspot/normal/eclipse" && return 0
  echo "api.adoptium.net is not reachable, trying the Temurin release on GitHub..."
  url=$($CURL "https://api.github.com/repos/adoptium/temurin$JDK-binaries/releases/latest" 2>/dev/null | grep -o "\"browser_download_url\": *\"[^\"]*jdk_${a}_linux_hotspot_[^\"]*\.tar\.gz\"" | head -1 | cut -d'"' -f4)
  [ -n "$url" ] && $CURL -o "$J.tgz" "$url" && return 0
  return 1
}
if [ ! -x "$J/bin/java" ]; then
  echo "installing JDK $JDK (Temurin)..."
  if fetch_jdk; then
    rm -rf "$J.tmp"; mkdir -p "$J.tmp"
    tar -xzf "$J.tgz" -C "$J.tmp" --strip-components=1
    rm -f "$J.tgz"; mv "$J.tmp" "$J"
  else
    echo "Temurin is not downloadable here, trying the distro package openjdk-$JDK-jdk-headless..."
    $S apt-get update -y >/dev/null && $S apt-get install -y "openjdk-$JDK-jdk-headless" >/dev/null
    ln -sfn "/usr/lib/jvm/java-$JDK-openjdk-$(dpkg --print-architecture)" "$J"
  fi
  [ -x "$J/bin/java" ] || { echo "could not install JDK $JDK" >&2; exit 1; }
fi
EXTRA=""
if [ "__GRADLE__" = 1 ] && [ ! -x "$H/gradle/bin/gradle" ]; then
  GV=__GRADLE_VERSION__
  [ -n "$GV" ] || GV=$($CURL https://services.gradle.org/versions/current 2>/dev/null | grep -o '"version"[^,]*' | head -1 | cut -d'"' -f4)
  [ -n "$GV" ] || GV=$($CURL https://api.github.com/repos/gradle/gradle-distributions/releases/latest | grep -o '"tag_name": *"[^"]*"' | head -1 | cut -d'"' -f4 | sed 's/^v//')
  [ -n "$GV" ] || { echo "could not determine a Gradle version; set remote.gradle_version" >&2; exit 1; }
  echo "installing Gradle $GV..."
  $CURL -o "$H/gradle.zip" "https://services.gradle.org/distributions/gradle-$GV-bin.zip" \
    || $CURL -o "$H/gradle.zip" "https://github.com/gradle/gradle-distributions/releases/download/v$GV/gradle-$GV-bin.zip"
  rm -rf "$H/gradle" "$H/gradle-$GV"; unzip -q -o "$H/gradle.zip" -d "$H"; mv "$H/gradle-$GV" "$H/gradle"; rm -f "$H/gradle.zip"
fi
[ -x "$H/gradle/bin/gradle" ] && EXTRA="$H/gradle/bin:"
A="$HOME/Android/Sdk"
if [ "__ANDROID__" = 1 ]; then
  SM="$A/cmdline-tools/latest/bin/sdkmanager"
  if [ ! -x "$SM" ]; then
    echo "installing Android command-line tools..."
    mkdir -p "$A/cmdline-tools"
    curl -fsSL -o "$H/cmdtools.zip" https://dl.google.com/android/repository/commandlinetools-linux-11076708_latest.zip
    rm -rf "$A/cmdline-tools/cmdline-tools" "$A/cmdline-tools/latest"
    unzip -q -o "$H/cmdtools.zip" -d "$A/cmdline-tools"; mv "$A/cmdline-tools/cmdline-tools" "$A/cmdline-tools/latest"; rm -f "$H/cmdtools.zip"
  fi
  export JAVA_HOME="$J"; export PATH="$J/bin:$PATH"
  yes | "$SM" --licenses >/dev/null 2>&1 || true
  echo "installing Android packages..."
  "$SM" --install __PACKAGES__ >/dev/null
fi
{
  echo "export JAVA_HOME=\"$J\""
  echo "export PATH=\"$J/bin:$EXTRA\$PATH\""
  if [ -d "$A/cmdline-tools/latest" ]; then
    echo "export ANDROID_HOME=\"$A\""; echo "export ANDROID_SDK_ROOT=\"$A\""
    echo "export PATH=\"$A/cmdline-tools/latest/bin:$A/platform-tools:\$PATH\""
  fi
} > "$H/env"
. "$H/env"
java -version 2>&1 | head -1
[ -x "$H/gradle/bin/gradle" ] && "$H/gradle/bin/gradle" --version | grep '^Gradle' || true
[ -d "$A/cmdline-tools/latest" ] && echo "Android SDK: $A" || true
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workspace_listing_parses() {
        let w = parse_workspaces("a-1\t/home/x/a\t2048\t1731567890\nbad line\nb-2\t?\t10\t0\n");
        assert_eq!(w.len(), 2);
        assert_eq!(w[0].origin, "/home/x/a");
        assert_eq!(w[0].kib, 2048);
        assert_eq!(w[1].last_used, 0);
    }

    #[test]
    fn hints() {
        assert!(hint_for(127).contains("setup"));
        assert!(hint_for(255).contains("doctor"));
        assert_eq!(hint_for(1), "");
    }
}
