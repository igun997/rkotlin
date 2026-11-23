use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};

static QUIET: AtomicBool = AtomicBool::new(false);

pub fn set_quiet(q: bool) {
    QUIET.store(q, Ordering::Relaxed);
}

pub fn is_quiet() -> bool {
    QUIET.load(Ordering::Relaxed)
}

/// Progress/log line on stderr. Never touches stdout (stdout belongs to cargo / MCP).
macro_rules! info {
    ($($t:tt)*) => {
        if !$crate::util::is_quiet() {
            eprintln!("rkotlin: {}", format_args!($($t)*));
        }
    };
}

/// POSIX single-quote a string for `sh`.
pub fn shq(s: &str) -> String {
    if !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_-./=:@%+,".contains(&b))
    {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', "'\\''"))
    }
}

pub fn fnv64(b: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &x in b {
        h ^= x as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

pub fn home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"))
}

fn xdg(var: &str, fallback: &str) -> PathBuf {
    std::env::var_os(var)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join(fallback))
}

pub fn cache_dir() -> PathBuf {
    xdg("XDG_CACHE_HOME", ".cache").join("rkotlin")
}

pub fn config_dir() -> PathBuf {
    xdg("XDG_CONFIG_HOME", ".config").join("rkotlin")
}

/// Remove every occurrence of `needle` from `hay`.
pub fn strip_all(hay: &[u8], needle: &[u8]) -> Vec<u8> {
    if needle.is_empty() {
        return hay.to_vec();
    }
    let mut out = Vec::with_capacity(hay.len());
    let mut i = 0;
    while i < hay.len() {
        if hay[i..].starts_with(needle) {
            i += needle.len();
        } else {
            out.push(hay[i]);
            i += 1;
        }
    }
    out
}

pub fn human_bytes(n: u64) -> String {
    const U: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1024.0 && i < U.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", U[i])
    }
}

pub fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

pub fn valid_env_key(k: &str) -> bool {
    let mut c = k.chars();
    matches!(c.next(), Some(f) if f.is_ascii_alphabetic() || f == '_')
        && c.all(|x| x.is_ascii_alphanumeric() || x == '_')
}

pub fn on_path(bin: &str) -> bool {
    std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).any(|d| d.join(bin).is_file()))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quoting() {
        assert_eq!(shq("abc-1.2/x"), "abc-1.2/x");
        assert_eq!(shq(""), "''");
        assert_eq!(shq("a b"), "'a b'");
        assert_eq!(shq("it's"), "'it'\\''s'");
        assert_eq!(shq("$(rm -rf /)"), "'$(rm -rf /)'");
    }

    #[test]
    fn strip() {
        assert_eq!(
            strip_all(
                b"error at /b/ws/home/x/src/a.rs:1 and /b/ws/home/x",
                b"/b/ws"
            ),
            b"error at /home/x/src/a.rs:1 and /home/x".to_vec()
        );
        assert_eq!(strip_all(b"abc", b""), b"abc".to_vec());
    }

    #[test]
    fn env_keys() {
        assert!(valid_env_key("GH_TOKEN"));
        assert!(!valid_env_key("1X"));
        assert!(!valid_env_key("A-B"));
        assert!(!valid_env_key(""));
    }

    #[test]
    fn bytes() {
        assert_eq!(human_bytes(10), "10 B");
        assert_eq!(human_bytes(2048), "2.0 KiB");
    }
}
