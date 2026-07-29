//! Workaround detector — scans agent output and git diffs for shortcuts.
//!
//! The watchdog agent calls [`detect_workarounds`] every monitoring cycle,
//! passing the recent PTY output and the latest `git diff`. The detector
//! returns a list of [`WorkaroundWarning`] entries that the watchdog folds
//! into its dedication score and ultimatum logic.
//!
//! ## What is detected
//!
//! Code patterns are matched ONLY against newly added diff lines (lines
//! beginning with `+`, excluding the `+++ b/file` header), so pre-existing
//! surrounding code never produces warnings:
//!
//! | # | Pattern                              | Severity |
//! |---|--------------------------------------|----------|
//! | 1 | `.unwrap()` / `.expect(`             | high     |
//! | 2 | `#[allow(dead_code)]` / `#[allow(clippy:*)]` | high |
//! | 3 | `#[ignore]` on tests                 | high     |
//! | 4 | `todo!()` / `unimplemented!()`       | critical |
//! | 5 | `// TODO` / `// FIXME`               | medium   |
//! | 6 | `println!` / `dbg!`                  | medium   |
//! | 7 | same shell command 3+ times (spin)   | high     |
//! | 8 | empty function body                  | high     |
//!
//! Spin detection (#7) operates on `recent_output`: it extracts the command
//! text following each shell prompt (`$`/`#`/`%` + whitespace) and flags any
//! command repeated three or more times.
//!
//! Implemented in: P3 (this file).
//! Tested by: `detector::tests` (15 unit tests).

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// A single workaround detected in agent output or a git diff.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorkaroundWarning {
    /// Machine-readable pattern id, e.g. `unwrap_call`, `spin`,
    /// `empty_function_body`.
    pub pattern: String,
    /// One of `critical`, `high`, `medium`, `low`.
    pub severity: String,
    /// File the warning applies to, when derivable from the diff hunk header.
    pub file: Option<String>,
    /// 1-based line number in the new file, when derivable from the diff.
    pub line: Option<u32>,
    /// Human-readable explanation.
    pub message: String,
}

/// Detect workarounds in agent output and git diff.
///
/// `recent_output` is the PTY output text from the agent.
/// `git_diff` is the staged/committed changes (output of `git diff`).
///
/// Only lines added by the diff (those beginning with `+`, excluding the
/// `+++ b/file` header) are scanned for code patterns, so clean surrounding
/// code never triggers a warning. Spin detection counts repeated shell
/// commands extracted from `recent_output`.
pub fn detect_workarounds(recent_output: &str, git_diff: &str) -> Vec<WorkaroundWarning> {
    let mut warnings = Vec::new();
    scan_diff(git_diff, &mut warnings);
    let mut spin = detect_spin(recent_output);
    warnings.append(&mut spin);
    warnings
}

// ===========================================================================
// Diff scanning
// ===========================================================================

fn scan_diff(git_diff: &str, warnings: &mut Vec<WorkaroundWarning>) {
    let mut current_file: Option<String> = None;
    let mut new_line: u32 = 0;
    // Previous non-blank added line, for multi-line empty-body detection.
    let mut prev_added: Option<String> = None;

    for raw in git_diff.lines() {
        // Hunk / file headers set state and never carry code patterns.
        if let Some(path) = raw.strip_prefix("+++ ") {
            current_file = parse_new_file(path).map(str::to_string);
            prev_added = None;
            continue;
        }
        if raw.starts_with("@@") {
            if let Some(n) = parse_hunk_new_start(raw) {
                new_line = n;
            }
            prev_added = None;
            continue;
        }
        if raw.starts_with("--- ")
            || raw.starts_with("diff ")
            || raw.starts_with("index ")
            || raw.starts_with("similarity ")
            || raw.starts_with("rename ")
            || raw.starts_with("copy ")
            || raw.starts_with("new file ")
            || raw.starts_with("deleted file ")
            || raw.starts_with("old mode ")
            || raw.starts_with("new mode ")
            || raw.starts_with("Binary files ")
            || raw.starts_with("GIT binary patch")
        {
            prev_added = None;
            continue;
        }

        if let Some(content) = raw.strip_prefix('+') {
            // Added line. `content` excludes the leading `+`.
            check_added_line(content, current_file.as_deref(), new_line, warnings);

            // Multi-line empty body: a `}` closing immediately after a fn
            // opening (only blank added lines may sit between them, since any
            // context/removed line resets `prev_added`).
            if content.trim() == "}" {
                if let Some(prev) = prev_added.as_deref() {
                    if is_fn_opening(prev.trim()) {
                        warnings.push(WorkaroundWarning {
                            pattern: "empty_function_body".to_string(),
                            severity: "high".to_string(),
                            file: current_file.clone(),
                            line: Some(new_line),
                            message: "new code adds an empty function body".to_string(),
                        });
                    }
                }
            }

            new_line = new_line.saturating_add(1);
            if !content.trim().is_empty() {
                prev_added = Some(content.to_string());
            }
        } else if raw.starts_with('-') {
            // Removed line — not present in the new file.
            prev_added = None;
        } else if raw.starts_with(' ') {
            // Context line — present in the new file.
            new_line = new_line.saturating_add(1);
            prev_added = None;
        } else {
            // `\ No newline at end of file`, blank lines, or anything else.
            prev_added = None;
        }
    }
}

/// Extract the new-file path from the content of a `+++ b/path` header.
fn parse_new_file(header: &str) -> Option<&str> {
    // `header` is everything after `+++ `. git prefixes tracked paths with
    // `b/`; `/dev/null` is used for deletions.
    let stripped = header.strip_prefix("b/").unwrap_or(header);
    let trimmed = stripped.trim_end();
    if trimmed.is_empty() || trimmed == "/dev/null" {
        None
    } else {
        Some(trimmed)
    }
}

/// Parse the new-file start line number from a `@@ -a,b +c,d @@` hunk header.
fn parse_hunk_new_start(line: &str) -> Option<u32> {
    let rest = line.trim_start().strip_prefix("@@")?;
    let rest = rest.trim_start();
    let rest = rest.strip_prefix('-')?;
    // Skip the old range: digits and an optional `,count`.
    let space = rest.find(' ')?;
    let after_old = rest[space..].trim_start();
    let after_plus = after_old.strip_prefix('+')?;
    let end = after_plus
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(after_plus.len());
    after_plus[..end].parse::<u32>().ok()
}

fn check_added_line(
    content: &str,
    file: Option<&str>,
    line_no: u32,
    warnings: &mut Vec<WorkaroundWarning>,
) {
    // 1. unwrap() / expect( — high
    if contains_isolated(content, "unwrap()") {
        warnings.push(warn(
            "unwrap_call",
            "high",
            file,
            line_no,
            "new code calls .unwrap() — will panic on None/Err",
        ));
    }
    if contains_isolated(content, "expect(") {
        warnings.push(warn(
            "expect_call",
            "high",
            file,
            line_no,
            "new code calls .expect() — will panic on None/Err",
        ));
    }

    // 2. #[allow(dead_code)] / #[allow(clippy:...)] — high
    if content.contains("allow(dead_code") {
        warnings.push(warn(
            "allow_dead_code",
            "high",
            file,
            line_no,
            "new code suppresses the dead_code lint",
        ));
    }
    if content.contains("allow(clippy:") {
        warnings.push(warn(
            "allow_clippy",
            "high",
            file,
            line_no,
            "new code suppresses a clippy lint",
        ));
    }

    // 3. #[ignore] on tests — high
    if has_ignore_attr(content) {
        warnings.push(warn(
            "ignored_test",
            "high",
            file,
            line_no,
            "new test is marked #[ignore]",
        ));
    }

    // 4. todo!() / unimplemented!() — critical
    if contains_isolated(content, "todo!(") {
        warnings.push(warn(
            "todo_macro",
            "critical",
            file,
            line_no,
            "new code uses todo!() — unfinished implementation",
        ));
    }
    if contains_isolated(content, "unimplemented!(") {
        warnings.push(warn(
            "unimplemented_macro",
            "critical",
            file,
            line_no,
            "new code uses unimplemented!() — unfinished implementation",
        ));
    }

    // 5. // TODO or // FIXME — medium
    if has_todo_comment(content) {
        warnings.push(warn(
            "todo_comment",
            "medium",
            file,
            line_no,
            "new code adds a TODO/FIXME comment",
        ));
    }

    // 6. println! / dbg! — medium
    if contains_isolated(content, "println!(") {
        warnings.push(warn(
            "println",
            "medium",
            file,
            line_no,
            "new code uses println! — prefer tracing",
        ));
    }
    if contains_isolated(content, "dbg!(") {
        warnings.push(warn(
            "dbg",
            "medium",
            file,
            line_no,
            "new code uses dbg! — debug leftover",
        ));
    }

    // 8. empty function body (single-line) — high
    if has_empty_fn_body(content) {
        warnings.push(warn(
            "empty_function_body",
            "high",
            file,
            line_no,
            "new code adds an empty function body",
        ));
    }
}

fn warn(
    pattern: &str,
    severity: &str,
    file: Option<&str>,
    line_no: u32,
    message: &str,
) -> WorkaroundWarning {
    WorkaroundWarning {
        pattern: pattern.to_string(),
        severity: severity.to_string(),
        file: file.map(str::to_string),
        line: Some(line_no),
        message: message.to_string(),
    }
}

// ===========================================================================
// Pattern helpers
// ===========================================================================

/// True if `needle` occurs in `line` and is not immediately preceded by an
/// identifier character (`[A-Za-z0-9_]`). This prevents matching suffixes of
/// larger identifiers, e.g. `unwrap()` inside `foo_unwrap()` or `println!(`
/// inside `eprintln!(`.
fn contains_isolated(line: &str, needle: &str) -> bool {
    let hay = line.as_bytes();
    let needle_b = needle.as_bytes();
    if needle_b.is_empty() || needle_b.len() > hay.len() {
        return false;
    }
    let last = hay.len() - needle_b.len();
    let mut i = 0;
    while i <= last {
        if hay[i..].starts_with(needle_b) {
            let prev_ok = if i == 0 {
                true
            } else {
                let c = hay[i - 1];
                !(c.is_ascii_alphanumeric() || c == b'_')
            };
            if prev_ok {
                return true;
            }
        }
        i += 1;
    }
    false
}

/// True if the line contains a `#[ignore]`-style attribute. Matches
/// `#[ignore]`, `#[ignore = "reason"]`, `#[ignore="..."]`, and whitespace
/// variants such as `#[ ignore ]`, but not `#[ignored]` or `#[ignorefoo]`.
fn has_ignore_attr(line: &str) -> bool {
    let bytes = line.as_bytes();
    let mut i = 0;
    while i + 1 < bytes.len() {
        if bytes[i] == b'#' && bytes[i + 1] == b'[' {
            let mut j = i + 2;
            while j < bytes.len() && bytes[j].is_ascii_whitespace() {
                j += 1;
            }
            if line[j..].starts_with("ignore") {
                j += "ignore".len();
                while j < bytes.len() && bytes[j].is_ascii_whitespace() {
                    j += 1;
                }
                if j < bytes.len() && (bytes[j] == b']' || bytes[j] == b'=') {
                    return true;
                }
            }
        }
        i += 1;
    }
    false
}

/// True if the line contains a `// TODO` or `// FIXME` comment marker. The
/// `//` must be preceded by whitespace or start-of-line so that `http://TODO`
/// inside a string does not match.
fn has_todo_comment(line: &str) -> bool {
    let bytes = line.as_bytes();
    let mut i = 0;
    while i + 1 < bytes.len() {
        if bytes[i] == b'/' && bytes[i + 1] == b'/' {
            let prev_ok = i == 0 || bytes[i - 1].is_ascii_whitespace();
            if prev_ok {
                let after = &line[i + 2..];
                let trimmed = after.trim_start();
                if let Some(rest) = trimmed.strip_prefix("TODO") {
                    if is_word_boundary(rest) {
                        return true;
                    }
                } else if let Some(rest) = trimmed.strip_prefix("FIXME") {
                    if is_word_boundary(rest) {
                        return true;
                    }
                }
            }
        }
        i += 1;
    }
    false
}

fn is_word_boundary(rest: &str) -> bool {
    match rest.chars().next() {
        None => true,
        Some(c) => !(c.is_ascii_alphanumeric() || c == '_'),
    }
}

/// Find the byte index of a `fn` keyword (word-bounded) in the line.
fn find_fn_keyword(line: &str) -> Option<usize> {
    let bytes = line.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i..].starts_with(b"fn") {
            let before_ok = i == 0 || {
                let c = bytes[i - 1];
                !(c.is_ascii_alphanumeric() || c == b'_')
            };
            // A function declaration has a name after `fn`, so `fn` must be
            // followed by whitespace (`fn()` / `fnname` are not declarations).
            let after_ok = i + 2 < bytes.len() && bytes[i + 2].is_ascii_whitespace();
            if before_ok && after_ok {
                return Some(i);
            }
        }
        i += 1;
    }
    None
}

/// True if the line is a single-line function with an empty body, e.g.
/// `fn foo() {}`, `pub fn bar(x: i32) -> u32 {}`, `async fn go() {}`.
fn has_empty_fn_body(line: &str) -> bool {
    let Some(fn_idx) = find_fn_keyword(line) else {
        return false;
    };
    let rest = &line[fn_idx + 2..];
    let Some(brace) = rest.find('{') else {
        return false;
    };
    let sig = &rest[..brace];
    if !(sig.contains('(') && sig.contains(')')) {
        return false;
    }
    // The body is empty if the first non-whitespace char after `{` is `}`.
    rest[brace + 1..].trim_start().starts_with('}')
}

/// True if the line opens a function body and nothing else, e.g.
/// `fn foo() {`. Used for multi-line empty-body detection.
fn is_fn_opening(line: &str) -> bool {
    let trimmed = line.trim();
    if !trimmed.ends_with('{') {
        return false;
    }
    find_fn_keyword(trimmed).is_some() && trimmed.contains('(') && trimmed.contains(')')
}

// ===========================================================================
// Spin detection
// ===========================================================================

fn detect_spin(recent_output: &str) -> Vec<WorkaroundWarning> {
    let mut counts: HashMap<&str, u32> = HashMap::new();
    for line in recent_output.lines() {
        if let Some(cmd) = extract_command(line) {
            *counts.entry(cmd).or_insert(0) += 1;
        }
    }
    let mut out: Vec<WorkaroundWarning> = counts
        .into_iter()
        .filter(|(_, n)| *n >= 3)
        .map(|(cmd, n)| WorkaroundWarning {
            pattern: "spin".to_string(),
            severity: "high".to_string(),
            file: None,
            line: None,
            message: format!(
                "Spin detected: command {:?} repeated {} times without progress",
                cmd, n
            ),
        })
        .collect();
    // Deterministic order for stable tests/reports.
    out.sort_by(|a, b| a.message.cmp(&b.message));
    out
}

/// Extract a shell command from a PTY line by taking the text after the last
/// prompt terminator (`$`, `#`, or `%` followed by whitespace). Lines without
/// a prompt are treated as command output and ignored.
fn extract_command(line: &str) -> Option<&str> {
    let bytes = line.as_bytes();
    let mut prompt_end: Option<usize> = None;
    let mut i = 0;
    while i + 1 < bytes.len() {
        let c = bytes[i];
        if (c == b'$' || c == b'#' || c == b'%') && bytes[i + 1].is_ascii_whitespace() {
            prompt_end = Some(i + 1);
        }
        i += 1;
    }
    let start = prompt_end?;
    let mut j = start;
    while j < bytes.len() && bytes[j].is_ascii_whitespace() {
        j += 1;
    }
    let cmd = line[j..].trim();
    if cmd.is_empty() {
        None
    } else {
        Some(cmd)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a minimal unified diff that adds the given lines to `file`.
    fn diff(file: &str, added: &[&str]) -> String {
        let mut s = String::new();
        s.push_str("diff --git a/");
        s.push_str(file);
        s.push_str(" b/");
        s.push_str(file);
        s.push('\n');
        s.push_str("--- a/");
        s.push_str(file);
        s.push('\n');
        s.push_str("+++ b/");
        s.push_str(file);
        s.push('\n');
        s.push_str("@@ -1,1 +1,N @@\n");
        for line in added {
            s.push('+');
            s.push_str(line);
            s.push('\n');
        }
        s
    }

    fn has_pattern(ws: &[WorkaroundWarning], pattern: &str) -> bool {
        ws.iter().any(|w| w.pattern == pattern)
    }

    fn count_pattern(ws: &[WorkaroundWarning], pattern: &str) -> usize {
        ws.iter().filter(|w| w.pattern == pattern).count()
    }

    // --- One test per pattern type -----------------------------------------

    // 1. unwrap() / expect(
    #[test]
    fn pattern_1_unwrap_and_expect() {
        let d = diff(
            "src/a.rs",
            &[
                "    let x = foo.unwrap();",
                "    let y = bar.expect(\"oops\");",
            ],
        );
        let ws = detect_workarounds("", &d);
        assert_eq!(count_pattern(&ws, "unwrap_call"), 1);
        assert_eq!(count_pattern(&ws, "expect_call"), 1);
        assert!(ws
            .iter()
            .filter(|w| w.pattern == "unwrap_call" || w.pattern == "expect_call")
            .all(|w| w.severity == "high"));
    }

    // 2. #[allow(dead_code)] / #[allow(clippy:*)]
    #[test]
    fn pattern_2_allow_attributes() {
        let d = diff(
            "src/a.rs",
            &[
                "#[allow(dead_code)]",
                "#[allow(clippy::all)] struct Foo;",
            ],
        );
        let ws = detect_workarounds("", &d);
        assert_eq!(count_pattern(&ws, "allow_dead_code"), 1);
        assert_eq!(count_pattern(&ws, "allow_clippy"), 1);
        assert!(ws.iter().all(|w| w.severity == "high"));
    }

    // 3. #[ignore] on tests
    #[test]
    fn pattern_3_ignore_test() {
        let d = diff("src/a.rs", &["#[ignore]", "#[ignore = \"flaky\"]"]);
        let ws = detect_workarounds("", &d);
        assert_eq!(count_pattern(&ws, "ignored_test"), 2);
        assert!(ws
            .iter()
            .filter(|w| w.pattern == "ignored_test")
            .all(|w| w.severity == "high"));
    }

    // 4. todo!() / unimplemented!()
    #[test]
    fn pattern_4_todo_and_unimplemented() {
        let d = diff(
            "src/a.rs",
            &[
                "    todo!()",
                "    unimplemented!()",
                "    let x = todo!(\"finish this\");",
            ],
        );
        let ws = detect_workarounds("", &d);
        assert_eq!(count_pattern(&ws, "todo_macro"), 2);
        assert_eq!(count_pattern(&ws, "unimplemented_macro"), 1);
        assert!(ws
            .iter()
            .filter(|w| w.pattern == "todo_macro" || w.pattern == "unimplemented_macro")
            .all(|w| w.severity == "critical"));
    }

    // 5. // TODO / // FIXME
    #[test]
    fn pattern_5_todo_finite_comments() {
        let d = diff(
            "src/a.rs",
            &[
                "    // TODO: fix this later",
                "    // FIXME: broken",
                "    let url = \"http://example.com\";", // must NOT match
            ],
        );
        let ws = detect_workarounds("", &d);
        assert_eq!(count_pattern(&ws, "todo_comment"), 2);
        assert!(ws
            .iter()
            .filter(|w| w.pattern == "todo_comment")
            .all(|w| w.severity == "medium"));
    }

    // 6. println! / dbg!
    #[test]
    fn pattern_6_println_and_dbg() {
        let d = diff(
            "src/a.rs",
            &[
                "    println!(\"debug value\");",
                "    dbg!(x);",
                "    eprintln!(\"err\");", // must NOT match println!
            ],
        );
        let ws = detect_workarounds("", &d);
        assert_eq!(count_pattern(&ws, "println"), 1);
        assert_eq!(count_pattern(&ws, "dbg"), 1);
        assert_eq!(
            ws.iter()
                .filter(|w| w.pattern == "println" || w.pattern == "dbg")
                .count(),
            2
        );
        assert!(ws
            .iter()
            .filter(|w| w.pattern == "println" || w.pattern == "dbg")
            .all(|w| w.severity == "medium"));
    }

    // 7. Spin: same command 3+ times
    #[test]
    fn pattern_7_spin_detection() {
        let recent = "$ cargo test\n\
                      Compiling crate...\n\
                      $ cargo test\n\
                      Compiling crate...\n\
                      $ cargo test\n";
        let ws = detect_workarounds(recent, "");
        let spins: Vec<&WorkaroundWarning> =
            ws.iter().filter(|w| w.pattern == "spin").collect();
        assert_eq!(spins.len(), 1);
        let spin = spins[0];
        assert_eq!(spin.severity, "high");
        assert!(spin.file.is_none() && spin.line.is_none());
        assert!(spin.message.contains("cargo test"));
        assert!(spin.message.contains("3 times"));
    }

    // 8. Empty function body (single-line)
    #[test]
    fn pattern_8_empty_function_body() {
        let d = diff(
            "src/a.rs",
            &[
                "fn empty() {}",
                "pub fn with_args(x: i32) -> u32 {}",
                "async fn go() {}",
            ],
        );
        let ws = detect_workarounds("", &d);
        assert_eq!(count_pattern(&ws, "empty_function_body"), 3);
        assert!(ws
            .iter()
            .filter(|w| w.pattern == "empty_function_body")
            .all(|w| w.severity == "high"));
    }

    // --- Clean-code negative test ------------------------------------------

    #[test]
    fn clean_code_no_false_positives() {
        let d = diff(
            "src/clean.rs",
            &[
                "    let x = 42;",
                "    let y = x + 1;",
                "    let s = \"hello world\";",
                "    foo.unwrap_or_default();",
                "    bar.unwrap_or_else(|| 0);",
                "    eprintln!(\"err\");",
                "    debug_assert!(x > 0);",
                "    let url = \"https://example.com/path\";",
                "    // a normal comment explaining the logic",
                "    let todo_list = vec![1, 2, 3];",
            ],
        );
        let recent = "$ cargo build\n   Compiling crate\n$ cargo test\n   Finished\n$ ls\nsrc\n";
        let ws = detect_workarounds(recent, &d);
        assert!(ws.is_empty(), "expected no warnings, got: {:?}", ws);
    }

    // --- Extra robustness tests --------------------------------------------

    #[test]
    fn no_false_positives_on_lookalike_identifiers() {
        // Identifiers/strings that contain pattern substrings but are not the
        // pattern themselves.
        let d = diff(
            "src/a.rs",
            &[
                "    foo.unwrap_or_default();",
                "    bar.unwrap_or_else(|| 0);",
                "    eprintln!(\"err\");",
                "    debug_assert!(x > 0);",
                "    let _ = unexpected!(\"x\");",
                "    // see https://example.com for docs",
                "    let todo_list = vec![1, 2, 3];",
            ],
        );
        let ws = detect_workarounds("", &d);
        assert!(ws.is_empty(), "unexpected warnings: {:?}", ws);
    }

    #[test]
    fn empty_function_body_multiline() {
        let d = "diff --git a/src/a.rs b/src/a.rs\n\
                 --- a/src/a.rs\n\
                 +++ b/src/a.rs\n\
                 @@ -1,0 +1,2 @@\n\
                 +fn empty() {\n\
                 +}\n";
        let ws = detect_workarounds("", d);
        assert_eq!(count_pattern(&ws, "empty_function_body"), 1);
    }

    #[test]
    fn empty_function_body_multiline_with_blank_line() {
        // A blank added line between `{` and `}` is still an empty body.
        let d = "diff --git a/src/a.rs b/src/a.rs\n\
                 --- a/src/a.rs\n\
                 +++ b/src/a.rs\n\
                 @@ -1,0 +1,3 @@\n\
                 +fn empty() {\n\
                 +\n\
                 +}\n";
        let ws = detect_workarounds("", d);
        assert_eq!(count_pattern(&ws, "empty_function_body"), 1);
    }

    #[test]
    fn spin_requires_three_repetitions() {
        // Only two repetitions must not trigger a spin warning.
        let recent = "$ cargo test\n$ cargo test\n";
        let ws = detect_workarounds(recent, "");
        assert!(!has_pattern(&ws, "spin"));
    }

    #[test]
    fn file_and_line_extracted_from_diff() {
        // Raw string so the leading space on context lines (` fn bar() {`,
        // ` }`) is preserved exactly as a real unified diff would have it.
        let d = "diff --git a/src/foo.rs b/src/foo.rs\n\
                 --- a/src/foo.rs\n\
                 +++ b/src/foo.rs\n\
                 @@ -5,2 +5,3 @@\n";
        let d = format!(
            "{d}{}\n{}\n{}\n{}\n",
            " fn bar() {", "+    foo.unwrap();", "+    let y = 2;", " }"
        );
        let ws = detect_workarounds("", &d);
        let matching: Vec<&WorkaroundWarning> =
            ws.iter().filter(|w| w.pattern == "unwrap_call").collect();
        assert_eq!(matching.len(), 1);
        assert_eq!(matching[0].file.as_deref(), Some("src/foo.rs"));
        assert_eq!(matching[0].line, Some(6));
    }

    #[test]
    fn only_added_lines_are_scanned() {
        // An unwrap call in a context line and a removed line must NOT trigger;
        // only the clean added line is scanned.
        let d = "diff --git a/src/a.rs b/src/a.rs\n\
                 --- a/src/a.rs\n\
                 +++ b/src/a.rs\n\
                 @@ -1,3 +1,3 @@\n";
        let d = format!(
            "{d}{}\n{}\n{}\n",
            " let x = old.unwrap();", "-let y = gone.unwrap();", "+let z = 5;"
        );
        let ws = detect_workarounds("", &d);
        assert!(ws.is_empty(), "got unexpected warnings: {:?}", ws);
    }
}
