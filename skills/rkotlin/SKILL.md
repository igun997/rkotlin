---
name: rkotlin
description: Use for ANY Kotlin, Android or JVM Gradle build, test, lint or assemble (gradle, ./gradlew, assembleDebug, bundleRelease, test, lint) in a project with settings.gradle(.kts) or build.gradle(.kts). Runs Gradle on a remote machine (instabox, Daytona or an SSH server) instead of the user's computer, which may be a small laptop that freezes on local builds. Also use when asked to set up remote Kotlin/Gradle builds, add or choose a build instance, install the Android SDK on the build machine, or when a command prints "SETUP REQUIRED".
---

# rkotlin: run Gradle on a remote machine, not on the user's laptop

The user's computer is often small (think 8 GB RAM). A local `./gradlew build` starts a Gradle daemon plus a Kotlin compile daemon and can freeze it. **Never run `gradle`, `./gradlew`, `kotlinc` or `mvn` builds locally.** Use rkotlin: it syncs the working tree (uncommitted and private code, plus `includeBuild("../x")` composite builds) to a remote machine over SSH, runs Gradle there with persistent Gradle caches and `build/` directories, and streams the result back. No GitHub access is involved. Light commands (`./gradlew tasks`, `--version`, `wrapper`, `clean`, `dependencies`) may run locally.

A JDK (Temurin 21 by default, `remote.jdk` overrides) and, for projects without a wrapper, Gradle install themselves on the remote at first use. The Android SDK does not: see below.

## How to call it

Prefer the MCP server (`rkotlin mcp`) when its tools are available:

| Tool | Use |
|---|---|
| `gradle` | `args` = what follows `gradle`/`./gradlew`: `["build"]`, `[":app:assembleDebug"]`, `["test","--tests","*FooTest"]`, `["lintDebug"]`, `["tasks"]`. Output is compact: compiler errors (`e:`/javac) first, the Gradle failure reason, failing tests, progress noise removed. `pull: true` copies apk/aab/jar outputs into the local `build/` directories after success. |
| `shell` | inspect the remote workspace (`ls app/build/outputs/apk`, `cat app/build/reports/...`). Not for building. |
| `sync` | upload changes without running anything |
| `instances` | list instances with descriptions and live state |
| `setup` | readiness check; returns exact steps for whatever is missing |
| `power` | `up` / `down` an instance |
| `workspaces` | disk usage per project on an instance |

Without MCP, use the CLI: `rkotlin build`, `rkotlin test`, `rkotlin :app:assembleDebug`, `rkotlin -i NAME build`. Anything that is not an rkotlin command is passed to Gradle and run remotely. rkotlin's own names win, so Gradle's `init` is `rkotlin gradle init`. Gradle options go after the task: `rkotlin build --info`, not `rkotlin --info build`. Add `--no-sync` (before the task) to skip uploading.

## Running the app on the user's phone: only the user can do it

`rkotlin --install assembleDebug` builds remotely, copies the apk back and installs it with the **user's local adb** (`--install=SERIAL` when several phones are attached; `rkotlin devices` lists them). It changes the user's phone: **do not run it yourself unless the user asked**. The MCP `gradle` tool has no install option; build with it (`pull: true`) and tell the user the apk path under `app/build/outputs/apk/`, or tell them the command. If it reports no device, the steps are for the user (USB or wireless debugging, accept the prompt on the phone).

## Android projects

The remote needs the Android SDK once: the user (or you, when asked) runs `rkotlin remote setup --android` (command-line tools, platform-tools, `platforms;android-35`, `build-tools;35.0.0`; `remote.android_packages` in `rkotlin.toml` overrides). It is about 600 MB, plus roughly 350 MB for the JDK and 1 GB of Gradle caches after the first build, so check disk first (`rkotlin remote doctor`; a 10 GB Daytona sandbox is enough). `local.properties` is never uploaded (it holds the user's own `sdk.dir`); the remote gets `ANDROID_HOME` from the setup. If a build says `SDK location not found`, the SDK is not installed on that instance. Missing platform or build-tools: add the package to `remote.android_packages` and rerun setup.

Some sandboxes only allow a few hosts (Daytona's free tiers block `services.gradle.org`, `plugins.gradle.org` and `api.adoptium.net`). Setup falls back to GitHub and the distro mirrors for the JDK and Gradle, but a gradle **wrapper** still downloads Gradle from `distributionUrl`: if `rkotlin remote doctor` prints the `services.gradle.org is not reachable` note, point `distributionUrl` in `gradle/wrapper/gradle-wrapper.properties` at `https://github.com/gradle/gradle-distributions/releases/download/vX.Y.Z/gradle-X.Y.Z-bin.zip` (tell the user; it changes their repo).

## Bundled Kotlin and Android skills

`rkotlin init` also installs official skills next to this one: from JetBrains (`kotlin-tooling-java-to-kotlin`, `kotlin-tooling-agp9-migration`, `kotlin-tooling-native-build-performance`, `kotlin-tooling-cocoapods-spm-migration`, `kotlin-tooling-immutable-collections-0-5-x-migration`, `kotlin-backend-jpa-entity-mapping`, `kotlin-tooling-kotlin-toolchain*`) and from Google (`agp-9-upgrade`, `r8-analyzer`, `testing-setup`); `rkotlin skills` lists them with their upstream URL, `rkotlin skills add NAME` installs one. Follow them for the Kotlin/Android work itself, with one rule from this skill on top: **wherever they say to run `./gradlew ...` or `gradle ...`, run `rkotlin ...` (or the `gradle` tool) instead.** Their helper scripts (`scripts/*.sh`, `analyze.py`) read project files: run them locally only when they do not start Gradle, otherwise through `shell` on the remote.

## Release builds and signing

`*.jks`, `*.keystore`, `key.properties`, `keystore.properties`, `local.properties` and `.env` files are never uploaded by default. A signed release build needs the user to opt in in `rkotlin.toml`:

```toml
[sync]
default_excludes = false          # uploads ALL of the above, including .env and *.pem
[remote]
forward_env = ["KEYSTORE_PASSWORD", "KEY_ALIAS_PASSWORD"]   # copied from the user's shell at run time, never written to disk
```

Explain that this copies those files to the build machine and let the user decide. Never read, print or paste signing secrets yourself.

## Several instances

A user may have more than one machine (for example a small default and a big one for release builds). Always check the list before choosing: the `instances` tool, or `rkotlin instances`.

- Omit `instance` to use the default.
- If there is **no default** the call fails with SETUP REQUIRED and the list; pick by description or ask the user.
- Choose a bigger instance only for heavy work (release builds, large projects) and only when its description says it is bigger.
- **Never** create, delete or reconfigure instances yourself. Suggest it to the user.
- When the session is over and the user is done building, `power` `down` saves money (builds start it automatically).

## When something is missing: SETUP REQUIRED

If rkotlin (CLI exit code 3, or an MCP result with `isError` and `structuredContent.setup_required`) prints **SETUP REQUIRED**, it lists numbered steps. Follow them literally:

1. Run steps **not** marked `[USER]` yourself.
2. For `[USER]` steps (browser login, API keys, installing software, dashboard) show the user the exact command or URL in plain words, then **wait** for their confirmation. Do not retry in a loop.
3. Never invent, guess, print or ask the user to paste credentials into the chat. Keys go into the environment or the provider's own login.
4. When the user says it is done, call `setup` (or `rkotlin onboard --live`) to verify, then retry the original command.

Typical first-time path for a new user (explain it simply, they may not know Gradle or servers):

- No instance yet, simplest: `npx instabox@latest` (signs in and installs the instabox CLI; a free tier exists), then `rkotlin instances add main --backend instabox --default --description "general purpose builder"`.
- Daytona: the user runs `daytona login` once, or sets `DAYTONA_API_KEY`. Register with `--set snapshot=daytona-large`: Daytona's default sandbox (1 vCPU / 1 GB) is too small for Gradle.
- Own server: `rkotlin instances add vps --backend ssh --set host=user@server`; key-based SSH must already work. Debian/Ubuntu with sudo or root.
- One-command project setup: `rkotlin init` (writes `rkotlin.toml`, this skill, the MCP registration and a guard that blocks local `gradle`/`./gradlew` builds in Claude Code).

## Reading results

- The header line shows the instance, SUCCESS/FAILED with the exit code and sync stats.
- Fix compile errors top to bottom (`error: /abs/path/File.kt:12:5 message`); warnings only if asked. Rerun the same `gradle` call to verify.
- Only as many diagnostics as fit are shown; fix those and rerun.
- Exit 75 / "gave up waiting for the remote workspace lock": another build runs in this workspace; retry shortly.
- `== failure ==` is Gradle's own "What went wrong" block (missing dependency, SDK, wrong JDK...).
- Wrong JDK for the project: set `remote.jdk = 17` in `rkotlin.toml`; the matching Temurin installs on the next run of `rkotlin remote setup` (or automatically at first use).
- Private maven repositories need a token on the remote: add the variable name to `forward_env` in `rkotlin.toml`.
- Disk full on the remote: `rkotlin remote ls`, then `rkotlin remote gc --days 7 --yes` (ask first) or `clean` through the `gradle` tool.
- **Never** run `rkotlin remote destroy`, `rkotlin remote rm` or `rkotlin remote gc --all` on your own initiative: they delete the user's remote machine or work. Offer them and wait for an explicit yes.
