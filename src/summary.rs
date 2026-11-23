//! Compact, token-cheap rendering of a Gradle run for AI agents: compiler diagnostics first,
//! then the build failure reason, failing tests, and a bounded tail. Progress noise is dropped.

use crate::run::Stream;

#[derive(Default)]
pub struct Capture {
    /// `Some(true)` after `BUILD SUCCESSFUL`, `Some(false)` after `BUILD FAILED`.
    pub build_success: Option<bool>,
    lines: Vec<String>,
}

/// Lines Gradle prints only to say what it is doing.
fn is_progress(line: &str) -> bool {
    let t = line.trim_start();
    t.starts_with("> Task ")
        || t.starts_with("> Configure project")
        || t.starts_with("> Transform ")
        || t.starts_with("Download ")
        || t.starts_with("Downloading ")
        || t.starts_with("Starting a Gradle Daemon")
        || t.starts_with("Daemon will be stopped")
        || t.starts_with("Calculating task graph")
        || t.starts_with("Configuration cache entry")
        || t.starts_with("Reusing configuration cache")
        || t.starts_with("Welcome to Gradle")
        || t.starts_with("Unzipping ")
        || t.starts_with("Initialized native services")
        || t.starts_with("To honour the JVM settings")
}

/// `e: file:///p/A.kt:3:5 message` (Kotlin), reported as a path an editor can open.
fn kotlin_diag(line: &str) -> Option<(bool, String)> {
    let (is_err, rest) = match line.strip_prefix("e: ") {
        Some(r) => (true, r),
        None => (false, line.strip_prefix("w: ")?),
    };
    let rest = rest.strip_prefix("file://").unwrap_or(rest);
    let tag = if is_err { "error" } else { "warning" };
    Some((is_err, format!("{tag}: {rest}")))
}

/// `/p/A.java:12: error: message` (javac); the source line and caret that follow are kept by the caller.
fn javac_diag(line: &str) -> Option<bool> {
    let idx = line.find(".java:")?;
    let rest = &line[idx + 6..];
    let digits = rest.chars().take_while(|c| c.is_ascii_digit()).count();
    if digits == 0 {
        return None;
    }
    let after = rest[digits..].strip_prefix(": ")?;
    if after.starts_with("error:") {
        Some(true)
    } else if after.starts_with("warning:") {
        Some(false)
    } else {
        None
    }
}

fn is_build_line(l: &str) -> bool {
    l.starts_with("BUILD SUCCESSFUL") || l.starts_with("BUILD FAILED")
}

fn is_summary_line(l: &str) -> bool {
    is_build_line(l) || l.contains("actionable task")
}

impl Capture {
    pub fn feed(&mut self, _stream: Stream, raw: &[u8]) {
        let line = String::from_utf8_lossy(raw)
            .trim_end_matches(['\n', '\r'])
            .to_string();
        if line.starts_with("BUILD SUCCESSFUL") {
            self.build_success = Some(true);
        } else if line.starts_with("BUILD FAILED") {
            self.build_success = Some(false);
        }
        self.lines.push(line);
    }

    /// Diagnostics in order of appearance, deduplicated: (is_error, text).
    fn diagnostics(&self) -> Vec<(bool, String)> {
        let mut out: Vec<(bool, String)> = vec![];
        let mut seen = std::collections::HashSet::new();
        let mut i = 0;
        while i < self.lines.len() {
            let l = &self.lines[i];
            let diag = if let Some((e, t)) = kotlin_diag(l) {
                Some((e, t))
            } else if let Some(e) = javac_diag(l) {
                // javac prints the source line and a caret under it
                let mut text = l.clone();
                for extra in self.lines[i + 1..].iter().take(3) {
                    if extra.trim().is_empty() || extra.starts_with("> ") || is_build_line(extra) {
                        break;
                    }
                    text += "\n";
                    text += extra;
                }
                Some((e, text))
            } else {
                None
            };
            if let Some((e, t)) = diag {
                if seen.insert(t.clone()) {
                    out.push((e, t));
                }
            }
            i += 1;
        }
        out
    }

    /// The `* What went wrong:` block of a failed build.
    fn failure(&self) -> Option<String> {
        let start = self
            .lines
            .iter()
            .position(|l| l.trim_start().starts_with("* What went wrong:"))?;
        let mut block = vec![];
        for l in &self.lines[start + 1..] {
            if l.starts_with("* Try:") || l.starts_with("* Exception is:") || is_build_line(l) {
                break;
            }
            block.push(l.as_str());
        }
        while block.last().is_some_and(|l| l.trim().is_empty()) {
            block.pop();
        }
        if block.is_empty() {
            None
        } else {
            Some(block.join("\n"))
        }
    }

    /// Failed test headers (`FooTest > bar() FAILED`) with their indented details, plus counts.
    fn tests(&self) -> (Vec<String>, Vec<String>) {
        let mut blocks = vec![];
        let mut counts = vec![];
        let mut i = 0;
        while i < self.lines.len() {
            let l = &self.lines[i];
            if l.ends_with(" FAILED") && l.contains(" > ") && !l.starts_with("> Task") {
                let mut b = vec![l.as_str()];
                for d in self.lines[i + 1..].iter().take(12) {
                    if d.starts_with("    ") {
                        b.push(d.as_str());
                    } else {
                        break;
                    }
                }
                i += b.len();
                blocks.push(b.join("\n"));
                continue;
            }
            if l.contains(" tests completed,") || l.contains("test completed,") {
                counts.push(l.clone());
            }
            i += 1;
        }
        (blocks, counts)
    }

    pub fn render(&self, code: i32, max_chars: usize) -> String {
        let mut s = String::new();
        let budget = |frac: f64| ((max_chars as f64) * frac) as usize;

        let diags = self.diagnostics();
        let errors = diags.iter().filter(|d| d.0).count();
        let warnings = diags.len() - errors;
        if !diags.is_empty() {
            s += &format!("== diagnostics: {errors} error(s), {warnings} warning(s) ==\n");
            let mut used = 0;
            let mut omitted = 0;
            let ordered = diags
                .iter()
                .filter(|d| d.0)
                .chain(diags.iter().filter(|d| !d.0));
            for (_, text) in ordered {
                if used + text.len() > budget(0.5) && used > 0 {
                    omitted += 1;
                    continue;
                }
                used += text.len();
                s += text;
                s += "\n";
            }
            if omitted > 0 {
                s += &format!("(+{omitted} more diagnostics omitted; fix the above first)\n");
            }
            s += "\n";
        }

        let (blocks, counts) = self.tests();
        if !blocks.is_empty() || !counts.is_empty() {
            s += "== tests ==\n";
            let mut used = 0;
            for b in &blocks {
                if used + b.len() > budget(0.3) {
                    s += "(more failing tests omitted)\n";
                    break;
                }
                used += b.len();
                s += b;
                s += "\n";
            }
            for c in &counts {
                s += c;
                s += "\n";
            }
            s += "\n";
        }

        let mut explained = !diags.is_empty() || !blocks.is_empty();
        if code != 0 {
            if let Some(f) = self.failure() {
                explained = true;
                // a compile failure only repeats what the diagnostics already said
                let compile_only = (f.lines().count() <= 3 && f.contains("Compilation error"))
                    || f.contains("Compilation failed");
                if !compile_only || diags.is_empty() {
                    s += "== failure ==\n";
                    let f: String = f.chars().take(budget(0.25)).collect();
                    s += &f;
                    s += "\n\n";
                } else {
                    s += "== failure ==\ncompilation failed (see diagnostics above)\n\n";
                }
            }
        }
        if code == 0 || !explained {
            // successful runs of `tasks`, `dependencies`, `--version`... print their result; failures
            // nothing above recognised keep a tail of the raw output
            let rest: Vec<&String> = self
                .lines
                .iter()
                .filter(|l| {
                    !is_progress(l)
                        && !l.trim().is_empty()
                        && !is_summary_line(l)
                        && kotlin_diag(l).is_none()
                        && javac_diag(l).is_none()
                })
                .collect();
            if !rest.is_empty() {
                s += "== output (tail) ==\n";
                s += &tail(&rest, budget(0.5));
                s += "\n\n";
            }
        }

        let hidden = self.lines.iter().filter(|l| is_progress(l)).count();
        for l in self.lines.iter().filter(|l| is_summary_line(l)) {
            s += l;
            s += "\n";
        }
        if hidden > 0 {
            s += &format!("({hidden} progress lines hidden)\n");
        }
        s.trim_end().to_string()
    }

    /// Unfiltered output (for the `shell` tool): the tail of every line, within `max_chars`.
    pub fn raw(&self, max_chars: usize) -> String {
        let all: Vec<&String> = self.lines.iter().collect();
        tail(&all, max_chars)
    }
}

fn tail(lines: &[&String], max: usize) -> String {
    let mut kept: Vec<&str> = vec![];
    let mut used = 0;
    for l in lines.iter().rev() {
        if used + l.len() + 1 > max && !kept.is_empty() {
            break;
        }
        used += l.len() + 1;
        kept.push(l);
    }
    kept.reverse();
    let omitted = lines.len() - kept.len();
    let mut s = String::new();
    if omitted > 0 {
        s += &format!("... ({omitted} earlier lines omitted)\n");
    }
    s += &kept.join("\n");
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(lines: &[&str]) -> Capture {
        let mut c = Capture::default();
        for l in lines {
            c.feed(Stream::Out, format!("{l}\n").as_bytes());
        }
        c
    }

    #[test]
    fn kotlin_errors_come_first_and_lose_file_scheme() {
        let c = run(&[
            "> Task :app:compileKotlin",
            "w: file:///home/u/p/app/src/Main.kt:3:5 Parameter 'x' is never used",
            "e: file:///home/u/p/app/src/Main.kt:9:13 Unresolved reference 'foo'",
            "e: file:///home/u/p/app/src/Main.kt:9:13 Unresolved reference 'foo'",
            "",
            "> Task :app:compileKotlin FAILED",
            "",
            "FAILURE: Build failed with an exception.",
            "",
            "* What went wrong:",
            "Execution failed for task ':app:compileKotlin'.",
            "> Compilation error. See log for more details",
            "",
            "* Try:",
            "> Run with --stacktrace option to get the stack trace.",
            "",
            "BUILD FAILED in 4s",
            "1 actionable task: 1 executed",
        ]);
        assert_eq!(c.build_success, Some(false));
        let r = c.render(1, 10_000);
        let e = r.find("error: /home/u/p/app/src/Main.kt:9:13").unwrap();
        let w = r.find("warning: /home/u/p/app/src/Main.kt:3:5").unwrap();
        assert!(e < w, "{r}");
        assert_eq!(r.matches("Unresolved reference").count(), 1, "{r}");
        assert!(r.contains("1 error(s), 1 warning(s)"), "{r}");
        assert!(r.contains("compilation failed (see diagnostics above)"), "{r}");
        assert!(r.contains("BUILD FAILED in 4s"));
        assert!(!r.contains("> Task"), "{r}");
        assert!(r.contains("progress lines hidden"));
    }

    #[test]
    fn javac_errors_keep_source_and_caret() {
        let c = run(&[
            "/p/src/A.java:12: error: cannot find symbol",
            "        foo();",
            "        ^",
            "  symbol:   method foo()",
            "",
            "BUILD FAILED in 1s",
        ]);
        let r = c.render(1, 10_000);
        assert!(r.contains("A.java:12: error: cannot find symbol\n        foo();\n        ^"), "{r}");
        assert!(r.contains("1 error(s)"), "{r}");
    }

    #[test]
    fn failing_tests_are_listed_with_details() {
        let c = run(&[
            "> Task :test",
            "",
            "MathTest > adds() FAILED",
            "    org.opentest4j.AssertionFailedError: expected: <4> but was: <5>",
            "        at MathTest.adds(MathTest.kt:10)",
            "",
            "3 tests completed, 1 failed",
            "",
            "> Task :test FAILED",
            "",
            "FAILURE: Build failed with an exception.",
            "",
            "* What went wrong:",
            "Execution failed for task ':test'.",
            "> There were failing tests. See the report at: file:///p/build/reports/tests/test/index.html",
            "",
            "* Try:",
            "BUILD FAILED in 2s",
        ]);
        let r = c.render(1, 10_000);
        assert!(r.contains("== tests =="), "{r}");
        assert!(r.contains("MathTest > adds() FAILED\n    org.opentest4j"), "{r}");
        assert!(r.contains("3 tests completed, 1 failed"), "{r}");
        assert!(r.contains("There were failing tests"), "{r}");
    }

    #[test]
    fn unknown_failures_show_the_what_went_wrong_block() {
        let c = run(&[
            "FAILURE: Build failed with an exception.",
            "",
            "* What went wrong:",
            "A problem occurred configuring root project 'x'.",
            "> SDK location not found. Define a valid SDK location with an ANDROID_HOME environment variable",
            "",
            "* Try:",
            "BUILD FAILED in 1s",
        ]);
        let r = c.render(1, 10_000);
        assert!(r.contains("== failure ==\nA problem occurred configuring root project 'x'."), "{r}");
        assert!(r.contains("SDK location not found"), "{r}");
    }

    #[test]
    fn success_is_one_line_plus_summary() {
        let c = run(&[
            "> Task :compileKotlin",
            "> Task :build",
            "",
            "BUILD SUCCESSFUL in 12s",
            "3 actionable tasks: 3 executed",
        ]);
        assert_eq!(c.build_success, Some(true));
        let r = c.render(0, 10_000);
        assert_eq!(
            r,
            "BUILD SUCCESSFUL in 12s\n3 actionable tasks: 3 executed\n(2 progress lines hidden)"
        );
    }

    #[test]
    fn diagnostics_are_bounded() {
        let many: Vec<String> = (0..200)
            .map(|i| format!("e: file:///p/A.kt:{i}:1 Unresolved reference 'x{i}'"))
            .collect();
        let refs: Vec<&str> = many.iter().map(String::as_str).collect();
        let r = run(&refs).render(1, 2_000);
        assert!(r.len() < 3_000, "{}", r.len());
        assert!(r.contains("more diagnostics omitted"));
    }
}
