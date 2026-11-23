//! Putting a remotely built apk on the user's phone with the user's own local adb.
//! Only ever driven by an explicit `--install` or `rkotlin devices` from the CLI.

use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Debug, PartialEq, Eq)]
pub struct Device {
    pub serial: String,
    /// `device`, `unauthorized`, `offline` ...
    pub state: String,
    pub detail: String,
}

/// `adb` from PATH, else the Android SDK's platform-tools (ANDROID_HOME / ANDROID_SDK_ROOT / ~/Android/Sdk).
fn adb_bin() -> Option<PathBuf> {
    if crate::util::on_path("adb") {
        return Some(PathBuf::from("adb"));
    }
    let mut sdks: Vec<PathBuf> = ["ANDROID_HOME", "ANDROID_SDK_ROOT"]
        .iter()
        .filter_map(std::env::var_os)
        .map(PathBuf::from)
        .collect();
    if let Some(h) = std::env::var_os("HOME") {
        sdks.push(Path::new(&h).join("Android/Sdk"));
    }
    sdks.into_iter()
        .map(|s| s.join("platform-tools/adb"))
        .find(|p| p.is_file())
}

fn adb() -> Result<PathBuf> {
    adb_bin().context(
        "adb was not found. Install the Android platform-tools on this computer (https://developer.android.com/tools/releases/platform-tools) and put adb on PATH, or set ANDROID_HOME",
    )
}

pub fn parse_devices(out: &str) -> Vec<Device> {
    out.lines()
        .skip_while(|l| !l.starts_with("List of devices"))
        .skip(1)
        .filter_map(|l| {
            let mut it = l.split_whitespace();
            let serial = it.next()?.to_string();
            let state = it.next()?.to_string();
            if serial.starts_with('*') {
                return None; // "* daemon not running" noise
            }
            Some(Device {
                serial,
                state,
                detail: it.collect::<Vec<_>>().join(" "),
            })
        })
        .collect()
}

fn list_devices(adb: &Path) -> Result<Vec<Device>> {
    let out = Command::new(adb)
        .args(["devices", "-l"])
        .output()
        .context("running adb devices")?;
    Ok(parse_devices(&String::from_utf8_lossy(&out.stdout)))
}

pub fn devices() -> Result<i32> {
    let adb = adb()?;
    let ds = list_devices(&adb)?;
    if ds.is_empty() {
        println!("No devices. Plug in the phone, enable USB debugging (or wireless debugging) and allow this computer.");
        return Ok(1);
    }
    for d in &ds {
        println!("{:<44} {:<13} {}", d.serial, d.state, d.detail);
    }
    Ok(0)
}

fn pick_device<'a>(ds: &'a [Device], want: Option<&str>) -> Result<&'a Device> {
    let ready: Vec<&Device> = ds.iter().filter(|d| d.state == "device").collect();
    if let Some(w) = want {
        let m: Vec<&&Device> = ready.iter().filter(|d| d.serial == w).collect();
        return match m.as_slice() {
            [d] => Ok(**d),
            _ => {
                let others: Vec<String> = ds
                    .iter()
                    .map(|d| format!("{} ({})", d.serial, d.state))
                    .collect();
                bail!(
                    "device `{w}` is not ready. adb sees: {}",
                    if others.is_empty() {
                        "nothing".into()
                    } else {
                        others.join(", ")
                    }
                )
            }
        };
    }
    match ready.as_slice() {
        [d] => Ok(*d),
        [] => {
            let seen: Vec<String> = ds
                .iter()
                .map(|d| format!("{} ({})", d.serial, d.state))
                .collect();
            bail!(
                "no phone is ready for install. adb sees: {}. For `unauthorized`, accept the prompt on the phone; otherwise plug it in with USB debugging on (`rkotlin devices`)",
                if seen.is_empty() { "nothing".into() } else { seen.join(", ") }
            )
        }
        many => bail!(
            "{} phones are attached ({}); pick one with --install=SERIAL",
            many.len(),
            many.iter()
                .map(|d| d.serial.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

/// The one apk to install out of everything that was pulled. Outputs of earlier builds stay on
/// the remote and are pulled again, so only the apks from the newest build (within 10 s of the
/// newest one) count.
pub fn pick_apk(files: &[PathBuf], abi: Option<&str>) -> Result<PathBuf> {
    let mtime = |p: &PathBuf| {
        std::fs::metadata(p)
            .and_then(|m| m.modified())
            .unwrap_or(std::time::UNIX_EPOCH)
    };
    let mut apks: Vec<&PathBuf> = files
        .iter()
        .filter(|p| {
            let s = p.to_string_lossy();
            s.ends_with(".apk") && s.contains("/build/outputs/apk/") && !s.contains("androidTest")
        })
        .collect();
    if let Some(newest) = apks.iter().map(|p| mtime(p)).max() {
        let window = std::time::Duration::from_secs(10);
        apks.retain(|p| newest.duration_since(mtime(p)).is_ok_and(|d| d <= window));
    }
    if apks.len() > 1 {
        if let Some(a) = abi {
            let by_abi: Vec<&PathBuf> = apks
                .iter()
                .copied()
                .filter(|p| {
                    p.file_name()
                        .is_some_and(|n| n.to_string_lossy().contains(a))
                })
                .collect();
            if by_abi.len() == 1 {
                apks = by_abi;
            }
        }
    }
    match apks.as_slice() {
        [one] => Ok((*one).clone()),
        [] => bail!("--install: the build produced no apk (try `assembleDebug`)"),
        many => bail!(
            "--install: the build produced {} apks, not one: {}. Build a single variant, e.g. `assembleDebug`",
            many.len(),
            many.iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join(", ")
        ),
    }
}

/// `adb install -r` the apk found in `files` on the chosen phone. Returns adb's exit code.
pub fn install(files: &[PathBuf], serial: Option<&str>) -> Result<i32> {
    let adb = adb()?;
    let ds = list_devices(&adb)?;
    let dev = pick_device(&ds, serial)?;
    let abi = Command::new(&adb)
        .args(["-s", &dev.serial, "shell", "getprop", "ro.product.cpu.abi"])
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty());
    let apk = pick_apk(files, abi.as_deref())?;
    info!("installing {} on {}", apk.display(), dev.serial);
    let st = Command::new(&adb)
        .args(["-s", &dev.serial, "install", "-r"])
        .arg(&apk)
        .status()
        .context("running adb install")?;
    Ok(st.code().unwrap_or(1))
}

#[cfg(test)]
mod tests {
    use super::*;

    const OUT: &str = "* daemon not running; starting now at tcp:5037\n* daemon started successfully\nList of devices attached\nRR8R device usb:1-1 product:a22nnxx model:SM_A225F device:a22 transport_id:3\nemulator-5554 offline\nabc unauthorized usb:1-2\n\n";

    #[test]
    fn parses_adb_devices() {
        let d = parse_devices(OUT);
        assert_eq!(d.len(), 3);
        assert_eq!(d[0].serial, "RR8R");
        assert_eq!(d[0].state, "device");
        assert!(d[0].detail.contains("SM_A225F"));
        assert_eq!(d[1].state, "offline");
        assert_eq!(d[2].state, "unauthorized");
    }

    #[test]
    fn device_choice_needs_exactly_one_ready_phone() {
        let d = parse_devices(OUT);
        assert_eq!(pick_device(&d, None).unwrap().serial, "RR8R");
        assert!(pick_device(&d, Some("abc")).is_err());
        assert_eq!(pick_device(&d, Some("RR8R")).unwrap().serial, "RR8R");
        assert!(pick_device(&[], None).is_err());
        let two = parse_devices("List of devices attached\na device\nb device\n");
        let e = pick_device(&two, None).unwrap_err().to_string();
        assert!(e.contains("--install=SERIAL"), "{e}");
    }

    #[test]
    fn apk_choice() {
        let d = std::env::temp_dir().join(format!("rkotlin-apk-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        let mk = |rel: &str, age_secs: u64| {
            let p = d.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, "x").unwrap();
            let t = std::time::SystemTime::now() - std::time::Duration::from_secs(age_secs);
            std::fs::File::options()
                .write(true)
                .open(&p)
                .unwrap()
                .set_modified(t)
                .unwrap();
            p
        };
        let debug = mk("app/build/outputs/apk/debug/app-debug.apk", 5);
        let meta = mk("app/build/outputs/apk/debug/output-metadata.json", 5);
        let test = mk(
            "app/build/outputs/apk/androidTest/debug/app-debug-androidTest.apk",
            5,
        );
        assert_eq!(pick_apk(&[debug.clone(), meta, test], None).unwrap(), debug);

        // an older release apk from an earlier build is ignored
        let old = mk(
            "app/build/outputs/apk/release/app-release-unsigned.apk",
            3600,
        );
        assert_eq!(
            pick_apk(&[old.clone(), debug.clone()], None).unwrap(),
            debug
        );

        // two apks from the same build are ambiguous
        let new_release = mk("app/build/outputs/apk/release/app-release-unsigned.apk", 5);
        assert!(pick_apk(&[new_release, debug], None).is_err());

        let a = mk("x/build/outputs/apk/debug/x-arm64-v8a-debug.apk", 2);
        let b = mk("x/build/outputs/apk/debug/x-x86_64-debug.apk", 2);
        assert_eq!(pick_apk(&[a.clone(), b], Some("arm64-v8a")).unwrap(), a);

        let jar = mk("app/build/libs/a.jar", 1);
        assert!(pick_apk(&[jar], None).is_err());
        let _ = std::fs::remove_dir_all(&d);
    }
}
