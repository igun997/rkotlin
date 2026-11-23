#![cfg(target_os = "linux")]
//! End-to-end through the real binary, with a fake `ssh` that runs the "remote" command locally and
//! a fake `gradlew` that prints Gradle-shaped output. Exercises: delta sync, incremental runs, path
//! rewriting, composite builds outside the project, deletion propagation, pulling outputs, adb
//! install, MCP, the guard hook and setup guidance. No JDK or Gradle is needed to run it.

use std::fs::{self, File};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, SystemTime};

/// Stands in for `./gradlew`: echoes how it was called, can fail like a Kotlin compile, and makes an apk.
const FAKE_GRADLEW: &str = r#"#!/bin/sh
echo "ARGS: $*"
echo "PWD: $(pwd)"
echo "JAVA_HOME: $JAVA_HOME"
echo "WRAPPED: $RKOTLIN"
if [ -f ../shared/marker.txt ]; then echo "SHARED: $(cat ../shared/marker.txt)"; fi
case "$*" in
  *failcompile*)
    echo "> Task :app:compileKotlin"
    echo "w: file://$(pwd)/app/src/Main.kt:1:1 Parameter 'x' is never used"
    echo "e: file://$(pwd)/app/src/Main.kt:3:5 Unresolved reference 'foo'"
    echo ""
    echo "> Task :app:compileKotlin FAILED"
    echo ""
    echo "FAILURE: Build failed with an exception."
    echo ""
    echo "* What went wrong:"
    echo "Execution failed for task ':app:compileKotlin'."
    echo "> Compilation error. See log for more details"
    echo ""
    echo "* Try:"
    echo "BUILD FAILED in 1s"
    exit 1 ;;
  *assembleDebug*)
    echo "> Task :app:assembleDebug"
    mkdir -p app/build/outputs/apk/debug
    echo "fake apk" > app/build/outputs/apk/debug/app-debug.apk ;;
  *wrapper*)
    mkdir -p gradle/wrapper
    echo "distributionUrl=fake" > gradle/wrapper/gradle-wrapper.properties ;;
esac
echo ""
echo "BUILD SUCCESSFUL in 1s"
echo "2 actionable tasks: 2 executed"
"#;

const FAKE_ADB: &str = r#"#!/bin/sh
echo "$@" >> "$ADB_LOG"
case "$*" in
  "devices -l") printf 'List of devices attached\nSERIAL1 device usb:1-1 model:Fake_Phone\n' ;;
  *getprop*) echo arm64-v8a ;;
  *" install "*) echo Success ;;
esac
"#;

struct Env {
    root: PathBuf,
    home: PathBuf,
    work: PathBuf,
}

impl Env {
    fn new(name: &str) -> Env {
        let root = std::env::temp_dir().join(format!("rkotlin-e2e-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let home = root.join("home");
        let work = root.join("work");
        fs::create_dir_all(&home).unwrap();
        fs::create_dir_all(work.join("hello")).unwrap();
        fs::create_dir_all(work.join("shared")).unwrap();
        let env = Env {
            root: root.canonicalize().unwrap(),
            home: home.canonicalize().unwrap(),
            work: work.canonicalize().unwrap(),
        };
        // the "remote" already has the JDK the config asks for
        env.write_exec(
            &env.home.join(".rkotlin/jdk-21/bin/java"),
            "#!/bin/sh\necho openjdk fake\n",
        );
        env.write(
            "hello/settings.gradle.kts",
            "rootProject.name = \"hello\"\nincludeBuild(\"../shared\")\ninclude(\":app\")\n",
        );
        env.write("hello/build.gradle.kts", "// root\n");
        env.write("hello/app/build.gradle.kts", "// app\n");
        env.write("hello/app/src/Main.kt", "fun main() {}\n");
        env.write(
            "shared/settings.gradle.kts",
            "rootProject.name = \"shared\"\n",
        );
        env.write("shared/marker.txt", "shared v1");
        env.write_exec(&env.project().join("gradlew"), FAKE_GRADLEW);
        env.write_config();
        env.age_files();
        env
    }

    fn project(&self) -> PathBuf {
        self.work.join("hello")
    }

    fn write(&self, rel: &str, content: &str) {
        let p = self.work.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(&p, content).unwrap();
    }

    fn write_exec(&self, p: &Path, content: &str) {
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, content).unwrap();
        fs::set_permissions(p, fs::Permissions::from_mode(0o755)).unwrap();
    }

    fn write_config(&self) {
        fs::write(
            self.project().join("rkotlin.toml"),
            "instance = \"fake\"\n[instances.fake]\nbackend = \"ssh\"\nhost = \"fake\"\ndescription = \"local stand-in\"\n[remote]\njdk = 21\n",
        )
        .unwrap();
    }

    /// Make every file old so the "recently modified" resend rule does not hide incremental behaviour.
    fn age_files(&self) {
        let old = SystemTime::now() - Duration::from_secs(7200);
        fn walk(d: &Path, old: SystemTime) {
            for e in fs::read_dir(d).unwrap() {
                let p = e.unwrap().path();
                if p.is_dir() {
                    walk(&p, old);
                } else {
                    File::options()
                        .write(true)
                        .open(&p)
                        .unwrap()
                        .set_modified(old)
                        .unwrap();
                }
            }
        }
        walk(&self.work, old);
    }

    fn cmd_in(&self, dir: &Path) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_rkotlin"));
        c.env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", &self.home)
            .env(
                "RKOTLIN_SSH",
                Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fake-ssh.sh"),
            )
            .env("RKOTLIN_CONFIG", self.root.join("global.toml"))
            .env("XDG_CACHE_HOME", self.root.join("cache"))
            .current_dir(dir);
        c
    }

    fn cmd(&self) -> Command {
        self.cmd_in(&self.project())
    }

    fn run(&self, args: &[&str]) -> Output {
        self.cmd().args(args).output().unwrap()
    }

    /// The remote workspace directory (there is exactly one).
    fn workspace(&self) -> PathBuf {
        fs::read_dir(self.home.join(".rkotlin/ws"))
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path()
    }
}

fn s(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

#[test]
fn sync_run_incremental_and_paths() {
    let env = Env::new("flow");
    env.write("hello/.gitignore", "secret.txt\nlocal-only/\n");
    env.write("hello/secret.txt", "do not upload");
    env.write("hello/local.properties", "sdk.dir=/home/me/Android\n");
    env.write("hello/app/keystore.properties", "storePassword=x\n");
    env.age_files();

    // 1. first run goes through the project's gradlew with plain console, on the configured JDK
    let o = env.run(&["assembleDebug"]);
    assert!(
        o.status.success(),
        "run failed:\nstdout:{}\nstderr:{}",
        s(&o.stdout),
        s(&o.stderr)
    );
    let out = s(&o.stdout);
    assert!(out.contains("ARGS: --console=plain assembleDebug"), "{out}");
    assert!(out.contains("WRAPPED: 1"), "{out}");
    assert!(
        out.contains(&format!(
            "JAVA_HOME: {}/.rkotlin/jdk-21",
            env.home.display()
        )),
        "{out}"
    );
    assert!(
        out.contains(&format!("PWD: {}", env.project().display())),
        "remote path must be rewritten to the local one:\n{out}"
    );
    // the included build outside the project root travelled along and is where ../shared points
    assert!(out.contains("SHARED: shared v1"), "{out}");
    assert!(s(&o.stderr).contains("sync: "), "{}", s(&o.stderr));

    // remote layout mirrors local absolute paths; outputs and excluded files stay local / remote only
    let ws = env.workspace();
    let mirror = ws.join(env.project().strip_prefix("/").unwrap());
    assert!(mirror
        .join("app/build/outputs/apk/debug/app-debug.apk")
        .is_file());
    assert!(mirror.join("gradlew").is_file());
    for hidden in ["secret.txt", "local.properties", "app/keystore.properties"] {
        assert!(
            !mirror.join(hidden).exists(),
            "{hidden} must not be uploaded"
        );
    }
    assert!(
        !env.project().join("app/build").exists(),
        "nothing may be built locally"
    );

    // 2. nothing changed: zero files sent (the remote build/ dir must not confuse the delta)
    let o = env.run(&["build"]);
    assert!(o.status.success(), "{}", s(&o.stderr));
    assert!(
        s(&o.stderr).contains("sync: 0 of"),
        "expected an incremental no-op sync:\n{}",
        s(&o.stderr)
    );

    // 3. one edit in the included build: only it is sent and the new content is seen
    env.write("shared/marker.txt", "shared v2");
    let o = env.run(&["build"]);
    assert!(
        s(&o.stdout).contains("SHARED: shared v2"),
        "{}",
        s(&o.stdout)
    );
    assert!(s(&o.stderr).contains("sync: 1 of"), "{}", s(&o.stderr));

    // 4. from a module directory: gradle gets -p <module>
    let o = env
        .cmd_in(&env.project().join("app"))
        .arg("test")
        .output()
        .unwrap();
    assert!(
        s(&o.stdout).contains("ARGS: --console=plain -p app test"),
        "{}{}",
        s(&o.stdout),
        s(&o.stderr)
    );

    // 5. the explicit form reaches Gradle commands rkotlin shadows
    let o = env.run(&["gradle", "init", "--type", "basic"]);
    assert!(
        s(&o.stdout).contains("ARGS: --console=plain init --type basic"),
        "{}",
        s(&o.stdout)
    );

    // 6. remote shell shows local paths
    let o = env.run(&["remote", "shell", "--", "pwd"]);
    assert_eq!(s(&o.stdout).trim(), env.project().to_str().unwrap());

    // 7. deletions propagate; remote-only files survive; a `build` source package is not mistaken for output
    env.write("hello/app/src/extra.kt", "// temp\n");
    env.write(
        "hello/app/src/main/com/x/build/Info.kt",
        "package com.x.build\n",
    );
    env.age_files();
    env.run(&["remote", "sync"]);
    assert!(mirror.join("app/src/extra.kt").is_file());
    assert!(mirror.join("app/src/main/com/x/build/Info.kt").is_file());
    fs::write(mirror.join("generated.txt"), "made remotely").unwrap();
    fs::remove_file(env.project().join("app/src/extra.kt")).unwrap();
    let o = env.run(&["remote", "sync"]);
    assert!(o.status.success(), "{}", s(&o.stderr));
    assert!(
        !mirror.join("app/src/extra.kt").exists(),
        "deleted locally => deleted remotely"
    );
    assert!(
        mirror.join("generated.txt").exists(),
        "files rkotlin never sent must not be deleted"
    );
    let o = env.run(&["remote", "sync"]);
    assert!(
        s(&o.stdout).contains("synced 0 of"),
        "the build source package must sync only once: {}",
        s(&o.stdout)
    );

    // 8. failures keep Gradle's exit code and never leak remote paths
    let o = env.run(&["failcompile"]);
    assert_eq!(o.status.code(), Some(1), "{}", s(&o.stderr));
    let all = format!("{}{}", s(&o.stdout), s(&o.stderr));
    assert!(
        all.contains(&format!(
            "e: file://{}/app/src/Main.kt:3:5",
            env.project().display()
        )),
        "{all}"
    );
    assert!(
        !all.contains(".rkotlin/ws"),
        "remote paths must not leak:\n{all}"
    );
    let _ = fs::remove_dir_all(&env.root);
}

#[test]
fn pull_copies_outputs_back_and_install_uses_local_adb() {
    let env = Env::new("pull");
    // `gradle wrapper` writes its files on the remote; they are copied back without --pull
    let wrapper = env
        .project()
        .join("gradle/wrapper/gradle-wrapper.properties");
    assert!(!wrapper.exists());
    let o = env.run(&["gradle", "wrapper", "--gradle-version", "8.10.2"]);
    assert!(o.status.success(), "{}", s(&o.stderr));
    assert_eq!(
        fs::read_to_string(&wrapper).unwrap().trim(),
        "distributionUrl=fake"
    );
    assert!(
        !env.project().join("app/build").exists(),
        "outputs are not pulled without --pull"
    );
    let o = env.run(&["--pull", "assembleDebug"]);
    assert!(o.status.success(), "{}", s(&o.stderr));
    let apk = env
        .project()
        .join("app/build/outputs/apk/debug/app-debug.apk");
    assert!(
        apk.is_file(),
        "apk should be pulled to {}\n{}",
        apk.display(),
        s(&o.stderr)
    );
    assert_eq!(fs::read_to_string(&apk).unwrap().trim(), "fake apk");
    fs::remove_file(&apk).unwrap();

    // --install without adb on PATH: a clear message, nothing else happens
    let o = env.run(&["--install", "assembleDebug"]);
    assert_eq!(o.status.code(), Some(2));
    assert!(
        s(&o.stderr).contains("adb was not found"),
        "{}",
        s(&o.stderr)
    );
    fs::remove_file(&apk).unwrap();

    // with adb: exactly one apk is installed on the only attached phone
    let bin = env.root.join("bin");
    env.write_exec(&bin.join("adb"), FAKE_ADB);
    let log = env.root.join("adb.log");
    let run_with_adb = |args: &[&str]| {
        env.cmd()
            .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
            .env("ADB_LOG", &log)
            .args(args)
            .output()
            .unwrap()
    };
    let o = run_with_adb(&["--install", "assembleDebug"]);
    assert!(o.status.success(), "{}", s(&o.stderr));
    let calls = fs::read_to_string(&log).unwrap();
    assert!(
        calls.contains(&format!("-s SERIAL1 install -r {}", apk.display())),
        "{calls}"
    );
    let o = run_with_adb(&["devices"]);
    assert!(
        s(&o.stdout).contains("SERIAL1") && s(&o.stdout).contains("Fake_Phone"),
        "{}",
        s(&o.stdout)
    );
    let _ = fs::remove_dir_all(&env.root);
}

#[test]
fn mcp_gradle_tool_returns_compact_result() {
    let env = Env::new("mcp");
    let mut child = env
        .cmd()
        .arg("mcp")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut out = BufReader::new(child.stdout.take().unwrap());
    let mut call = |id: u32, method: &str, params: serde_json::Value| -> serde_json::Value {
        let req = serde_json::json!({"jsonrpc":"2.0","id":id,"method":method,"params":params});
        writeln!(stdin, "{req}").unwrap();
        let mut line = String::new();
        out.read_line(&mut line).unwrap();
        serde_json::from_str(&line).unwrap_or_else(|e| panic!("bad JSON-RPC line `{line}`: {e}"))
    };
    let init = call(
        1,
        "initialize",
        serde_json::json!({"protocolVersion":"2025-06-18"}),
    );
    let instr = init["result"]["instructions"].as_str().unwrap().to_string();
    assert!(
        instr.contains("- fake [default] (ssh): local stand-in"),
        "instances must be advertised to agents:\n{instr}"
    );

    let r = call(
        2,
        "tools/call",
        serde_json::json!({"name":"gradle","arguments":{"args":["failcompile"]}}),
    );
    assert_eq!(r["result"]["isError"], true);
    let text = r["result"]["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("FAILED (exit 1)"), "{text}");
    assert!(
        text.contains("== diagnostics: 1 error(s), 1 warning(s)"),
        "{text}"
    );
    assert!(
        text.contains(&format!(
            "error: {}/app/src/Main.kt:3:5 Unresolved reference 'foo'",
            env.project().display()
        )),
        "{text}"
    );
    assert!(
        !text.contains("> Task"),
        "progress noise must be hidden:\n{text}"
    );
    assert!(!text.contains(".rkotlin/ws"), "{text}");

    let r = call(
        3,
        "tools/call",
        serde_json::json!({"name":"gradle","arguments":{"args":["assembleDebug"],"pull":true}}),
    );
    assert_eq!(r["result"]["isError"], false, "{r}");
    let text = r["result"]["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("BUILD SUCCESSFUL in 1s"), "{text}");
    assert!(text.contains("pulled 1 output file"), "{text}");
    assert!(env
        .project()
        .join("app/build/outputs/apk/debug/app-debug.apk")
        .is_file());

    // shell output is not filtered
    let r = call(
        4,
        "tools/call",
        serde_json::json!({"name":"shell","arguments":{"command":"ls app/build/outputs/apk"}}),
    );
    assert!(
        r["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("debug"),
        "{r}"
    );

    let r = call(
        5,
        "tools/call",
        serde_json::json!({"name":"instances","arguments":{"live":false}}),
    );
    assert!(r["result"]["content"][0]["text"]
        .as_str()
        .unwrap()
        .contains("fake"));
    drop(stdin);
    let _ = child.wait();
    let _ = fs::remove_dir_all(&env.root);
}

#[test]
fn missing_setup_is_explained_to_humans_and_agents() {
    let env = Env::new("setup");
    // no config, no instabox on PATH
    fs::remove_file(env.project().join("rkotlin.toml")).unwrap();
    let o = env.run(&["test"]);
    assert_eq!(o.status.code(), Some(3), "{}", s(&o.stderr));
    let err = s(&o.stderr);
    assert!(err.contains("SETUP REQUIRED"), "{err}");
    assert!(
        err.contains("instabox login")
            && err.contains("daytona login")
            && err.contains("--backend ssh"),
        "{err}"
    );
    assert!(err.contains("[USER]"), "{err}");

    // machine-readable for agents
    let o = env.run(&["onboard", "--json"]);
    assert_eq!(o.status.code(), Some(3));
    let v: serde_json::Value = serde_json::from_slice(&o.stdout).unwrap();
    assert_eq!(v["ready"], false);
    assert_eq!(v["setup"]["setup_required"], true);

    // several instances, no default => must choose, and the list is shown
    fs::write(
        env.project().join("rkotlin.toml"),
        "[instances.a]\nbackend=\"ssh\"\nhost=\"h\"\ndescription=\"small\"\n[instances.b]\nbackend=\"ssh\"\nhost=\"h\"\ndescription=\"huge\"\n",
    )
    .unwrap();
    let o = env.run(&["build"]);
    assert_eq!(o.status.code(), Some(3));
    let err = s(&o.stderr);
    assert!(
        err.contains("a (ssh): small") && err.contains("b (ssh): huge"),
        "{err}"
    );

    // daytona without a key
    fs::write(
        env.project().join("rkotlin.toml"),
        "[instances.d]\nbackend=\"daytona\"\n",
    )
    .unwrap();
    let o = env.run(&["build"]);
    assert_eq!(o.status.code(), Some(3));
    assert!(
        s(&o.stderr).contains("no Daytona login or API key found"),
        "{}",
        s(&o.stderr)
    );

    // not a Gradle project
    let empty = env.root.join("empty");
    fs::create_dir_all(&empty).unwrap();
    fs::write(
        env.root.join("global.toml"),
        "default_instance = \"fake\"\n[instances.fake]\nbackend = \"ssh\"\nhost = \"fake\"\n",
    )
    .unwrap();
    let o = env.cmd_in(&empty).arg("build").output().unwrap();
    assert_eq!(o.status.code(), Some(2));
    assert!(
        s(&o.stderr).contains("not inside a Gradle project"),
        "{}",
        s(&o.stderr)
    );
    let _ = fs::remove_dir_all(&env.root);
}

#[test]
fn guard_hook_blocks_local_gradle() {
    let env = Env::new("hook");
    let run = |cmd: &str| {
        let mut c = env.cmd();
        c.arg("hook")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut ch = c.spawn().unwrap();
        let payload = serde_json::json!({"tool_name":"Bash","tool_input":{"command":cmd}});
        ch.stdin
            .take()
            .unwrap()
            .write_all(payload.to_string().as_bytes())
            .unwrap();
        ch.wait_with_output().unwrap()
    };
    let o = run("cd app && ./gradlew :app:assembleDebug");
    assert_eq!(o.status.code(), Some(2));
    assert!(
        s(&o.stderr).contains("rkotlin :app:assembleDebug"),
        "{}",
        s(&o.stderr)
    );
    assert_eq!(run("./gradlew --version").status.code(), Some(0));
    assert_eq!(run("./gradlew tasks").status.code(), Some(0));
    assert_eq!(
        run("RKOTLIN_ALLOW_LOCAL=1 ./gradlew build").status.code(),
        Some(0)
    );
    let _ = fs::remove_dir_all(&env.root);
}

#[test]
fn bundled_kotlin_skills_install_offline() {
    let env = Env::new("skills");
    let o = env.run(&["skills"]);
    let list = s(&o.stdout);
    assert!(o.status.success(), "{}", s(&o.stderr));
    for n in [
        "kotlin-tooling-java-to-kotlin",
        "agp-9-upgrade",
        "r8-analyzer",
        "testing-setup",
    ] {
        assert!(list.contains(n), "{n} missing:\n{list}");
    }
    assert!(
        list.contains("JetBrains") && list.contains("Google (Android)"),
        "{list}"
    );
    assert!(
        list.contains("https://github.com/Kotlin/kotlin-agent-skills/tree/"),
        "{list}"
    );

    // `integrate --skill` installs the rkotlin skill plus every bundled one, in both agent dirs
    let o = env.run(&["integrate", "--skill"]);
    assert!(o.status.success(), "{}", s(&o.stderr));
    for root in [".claude/skills", ".agents/skills"] {
        let base = env.project().join(root);
        assert!(base.join("rkotlin/SKILL.md").is_file());
        assert!(base
            .join("kotlin-tooling-java-to-kotlin/SKILL.md")
            .is_file());
        assert!(base
            .join("kotlin-tooling-java-to-kotlin/LICENSE.txt")
            .is_file());
        assert!(base.join("r8-analyzer/scripts/analyze.py").is_file());
    }

    // opting out leaves only the rkotlin skill
    let env2 = Env::new("skills-min");
    let o = env2.run(&["integrate", "--skill", "--no-kotlin-skills"]);
    assert!(o.status.success(), "{}", s(&o.stderr));
    assert!(env2
        .project()
        .join(".agents/skills/rkotlin/SKILL.md")
        .is_file());
    assert!(!env2.project().join(".agents/skills/r8-analyzer").exists());

    // selective add, and a helpful error for a wrong name
    let o = env2.run(&["skills", "add", "r8-analyzer"]);
    assert!(o.status.success(), "{}", s(&o.stderr));
    assert!(env2
        .project()
        .join(".claude/skills/r8-analyzer/SKILL.md")
        .is_file());
    let o = env2.run(&["skills", "add", "nope"]);
    assert_eq!(o.status.code(), Some(2));
    assert!(s(&o.stderr).contains("Available:"), "{}", s(&o.stderr));
    let _ = fs::remove_dir_all(&env.root);
    let _ = fs::remove_dir_all(&env2.root);
}
