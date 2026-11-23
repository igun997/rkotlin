//! Embeds `skills/kotlin/**` (vendored, Apache-2.0 agent skills from JetBrains and Google) into the
//! binary as a static table, so `rkotlin init` / `rkotlin skills add` work offline.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
    let mut entries: Vec<PathBuf> = std::fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("reading {}: {e}", dir.display()))
        .map(|e| e.unwrap().path())
        .collect();
    entries.sort();
    for p in entries {
        if p.is_dir() {
            walk(&p, out);
        } else {
            out.push(p);
        }
    }
}

fn main() {
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let root = manifest.join("skills/kotlin");
    println!("cargo:rerun-if-changed=skills/kotlin");
    let mut files = vec![];
    walk(&root, &mut files);

    let mut src = String::from("pub static FILES: &[(&str, &[u8], bool)] = &[\n");
    for f in files {
        let rel = f
            .strip_prefix(&root)
            .unwrap()
            .to_string_lossy()
            .replace('\\', "/");
        // SOURCES.tsv is read separately; only skill folders are installable
        if !rel.contains('/') {
            continue;
        }
        let exec = {
            use std::os::unix::fs::PermissionsExt;
            std::fs::metadata(&f).unwrap().permissions().mode() & 0o111 != 0
        };
        writeln!(
            src,
            "    ({rel:?}, include_bytes!({:?}), {exec}),",
            f.to_string_lossy()
        )
        .unwrap();
    }
    src.push_str("];\n");
    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap()).join("bundled_skills.rs");
    std::fs::write(out, src).unwrap();
}
