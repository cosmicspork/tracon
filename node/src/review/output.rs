//! What a check printed, reduced to what says why it failed.
//!
//! A check's record keeps the last few KiB of its output. That is the right
//! end of a log only when the failure is the last thing printed, and it often
//! is not: `cargo test` prints its failures and summary to stdout and every
//! `Compiling …` line to stderr, so a log assembled as stdout-then-stderr ends
//! in compiler progress, and a `just` recipe that runs clippy, then tests, then
//! a frontend check buries the first failure under everything after it. So a
//! check's two streams are merged where they are written ([`merged_shell`]),
//! and the lines that carry a failure are lifted out of the whole output
//! ([`failure_lines`]) before it is cut to a tail.

/// How much of the failure lines is kept, in bytes and in lines.
const MAX_BYTES: usize = 2048;
const MAX_LINES: usize = 40;
/// A single line is cut here: a failure line is a headline, not a dump.
const MAX_LINE: usize = 240;
/// Lines kept after a headline (a panic's message, a compiler error's
/// location, an assertion's expected and received values): see [`continues`].
const CONTEXT: usize = 3;

/// The argv that runs `command` under a login shell with stderr written into
/// stdout, so the captured output is in the order it was printed.
///
/// The redirection is made by an outer shell that then `exec`s the same
/// `sh -lc <command>` the check always ran, so the command's shell sees the
/// command text unchanged: its own diagnostics (`sh: 1: just: not found`,
/// which `missing_tool` reads) keep their line numbers.
pub fn merged_shell(command: &str) -> Vec<String> {
    vec![
        "sh".into(),
        "-c".into(),
        r#"exec 2>&1; exec sh -lc "$1""#.into(),
        "sh".into(),
        command.into(),
    ]
}

/// The lines of `output` that say what failed, in the order they were
/// printed, bounded in size. Empty when nothing recognisable failed.
///
/// Recognised: test runners' per-test failures and summaries (cargo test,
/// nextest, bun, vitest/jest, pytest, go, Pest), panics with their messages,
/// compiler and linter errors with their locations (rustc, clippy, tsc,
/// svelte-check), and the generic `error:`/`Error:`/`fatal:` lines that a
/// failing tool or a `just` recipe ends with.
pub fn failure_lines(output: &str) -> String {
    let lines: Vec<String> = output.split('\n').map(clean).collect();
    let mut kept: Vec<usize> = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i].trim();
        if line == "failures:" {
            // cargo test lists the failed tests' names, indented, under a
            // second `failures:`; the first is followed by a blank line.
            let mut j = i + 1;
            let mut names = Vec::new();
            while j < lines.len()
                && lines[j].starts_with([' ', '\t'])
                && !lines[j].trim().is_empty()
            {
                names.push(j);
                j += 1;
            }
            if !names.is_empty() {
                kept.push(i);
                kept.extend(names);
            }
            i = j.max(i + 1);
            continue;
        }
        if !is_failure(line) {
            i += 1;
            continue;
        }
        // svelte-check and some linters print the location on the line
        // before the error.
        if line.starts_with("Error") {
            if let Some(prev) = i.checked_sub(1) {
                if is_location(lines[prev].trim()) {
                    kept.push(prev);
                }
            }
        }
        kept.push(i);
        let mut j = i + 1;
        let mut taken = 0;
        let mut skipped_blank = false;
        while j < lines.len() && taken < CONTEXT {
            let next = lines[j].trim();
            if next.is_empty() {
                // One blank line sits between bun's `error:` and its
                // expected and received values.
                if skipped_blank || taken > 0 {
                    break;
                }
                skipped_blank = true;
                j += 1;
                continue;
            }
            if !continues(line, next) {
                break;
            }
            kept.push(j);
            taken += 1;
            j += 1;
        }
        i = j.max(i + 1);
    }

    // Each line once: nextest repeats a failed test in its summary, and a
    // retried or nested runner says the same thing twice.
    let mut seen = std::collections::HashSet::new();
    let picked: Vec<String> = kept
        .into_iter()
        .map(|index| cut(lines[index].trim()))
        .filter(|line| seen.insert(line.clone()))
        .collect();
    bound(picked)
}

/// The text an agent or a reader is given for a failed check: the failure
/// lines first, when there are any, then the tail.
pub fn with_failures(failures: &str, tail: &str) -> String {
    if failures.is_empty() {
        tail.to_string()
    } else {
        format!("What failed:\n{failures}\n\nEnd of the output:\n{tail}")
    }
}

fn is_failure(line: &str) -> bool {
    let lower_start = |prefix: &str| {
        line.get(..prefix.len())
            .is_some_and(|start| start.eq_ignore_ascii_case(prefix))
    };
    // rustc, clippy, cargo, just, bun, git, and most CLIs.
    if lower_start("error:") || line.starts_with("error[") || lower_start("fatal:") {
        return true;
    }
    if line.starts_with("ERROR") || line.starts_with("panic:") || line.starts_with("npm ERR!") {
        return true;
    }
    // A panic names the test thread and where.
    if line.starts_with("thread '") && line.contains("panicked at") {
        return true;
    }
    // cargo test.
    if line.starts_with("test ") && line.ends_with(" FAILED") {
        return true;
    }
    if line.starts_with("test result: FAILED") {
        return true;
    }
    // nextest, vitest/jest, pytest, Pest, go.
    for prefix in [
        "FAIL ",
        "FAIL\t",
        "FAILED ",
        "FAILED:",
        "SIGSEGV ",
        "SIGABRT ",
        "SIGKILL ",
        "TIMEOUT ",
        "LEAK-FAIL ",
        "--- FAIL",
        "(fail)",
        "✗ ",
        "✕ ",
        "× ",
        "⨯ ",
    ] {
        if line.starts_with(prefix) {
            return true;
        }
    }
    if line.starts_with("Summary [") && (line.contains(" failed") || line.contains("timed out")) {
        return true;
    }
    // pytest's closing banner: `==== 1 failed, 2 passed in 0.12s ====`.
    if line.starts_with('=') && line.contains(" failed") {
        return true;
    }
    // bun's closing count, ` 1 fail`.
    if let Some(count) = line.strip_suffix(" fail") {
        if !count.is_empty() && count.bytes().all(|b| b.is_ascii_digit()) {
            return true;
        }
    }
    // tsc, `src/x.ts(3,7): error TS2322: …`.
    if line.contains(": error TS") {
        return true;
    }
    // svelte-check, `svelte-check found 2 errors and 0 warnings`.
    if line.starts_with("svelte-check found ") && !line.starts_with("svelte-check found 0 error") {
        return true;
    }
    false
}

/// Whether `next` belongs to the failure `line` printed just before it: a
/// panic's message and assertion values, a compiler error's location, a test
/// framework's expected and received values. Nothing else is taken on trust,
/// since what follows a tool's last error is usually the next tool's output.
fn continues(line: &str, next: &str) -> bool {
    if is_failure(next) {
        return false;
    }
    if line.starts_with("thread '") {
        return !next.starts_with("note:") && !next.starts_with("stack backtrace");
    }
    let error = line
        .get(..6)
        .is_some_and(|start| start.eq_ignore_ascii_case("error:"))
        || line.starts_with("error[");
    error
        && (next.starts_with("-->") || next.starts_with("Expected") || next.starts_with("Received"))
}

/// `path:line:col` alone on a line.
fn is_location(line: &str) -> bool {
    let mut parts = line.rsplitn(3, ':');
    let col = parts.next().unwrap_or_default();
    let row = parts.next().unwrap_or_default();
    let path = parts.next().unwrap_or_default();
    !path.is_empty()
        && !path.contains(' ')
        && !row.is_empty()
        && row.bytes().all(|b| b.is_ascii_digit())
        && !col.is_empty()
        && col.bytes().all(|b| b.is_ascii_digit())
}

/// The line as a terminal would have left it: colour codes dropped, and only
/// what the last carriage return wrote.
fn clean(raw: &str) -> String {
    let raw = raw.strip_suffix('\r').unwrap_or(raw);
    let raw = raw.rsplit('\r').next().unwrap_or(raw);
    let mut out = String::with_capacity(raw.len());
    let mut chars = raw.chars();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            // CSI: ESC [ parameters final-byte.
            if chars.clone().next() == Some('[') {
                chars.next();
                for c in chars.by_ref() {
                    if ('@'..='~').contains(&c) {
                        break;
                    }
                }
            }
            continue;
        }
        out.push(c);
    }
    out
}

fn cut(line: &str) -> String {
    if line.len() <= MAX_LINE {
        return line.to_string();
    }
    let mut at = MAX_LINE;
    while !line.is_char_boundary(at) {
        at -= 1;
    }
    format!("{}…", &line[..at])
}

/// Keep the first failures and the last: the first is usually the cause, and
/// the last is the summary that names everything that failed.
fn bound(lines: Vec<String>) -> String {
    let size = |lines: &[String]| lines.iter().map(|line| line.len() + 1).sum::<usize>();
    if lines.len() <= MAX_LINES && size(&lines) <= MAX_BYTES {
        return lines.join("\n");
    }
    let half_bytes = MAX_BYTES / 2;
    let half_lines = MAX_LINES / 2;
    let mut tail = Vec::new();
    let mut tail_bytes = 0;
    for line in lines.iter().rev() {
        if tail.len() == half_lines || tail_bytes + line.len() + 1 > half_bytes {
            break;
        }
        tail_bytes += line.len() + 1;
        tail.push(line.clone());
    }
    tail.reverse();
    let rest = lines.len() - tail.len();
    let mut head = Vec::new();
    let mut head_bytes = 0;
    for line in &lines[..rest] {
        if head.len() == half_lines || head_bytes + line.len() + 1 > half_bytes {
            break;
        }
        head_bytes += line.len() + 1;
        head.push(line.clone());
    }
    let omitted = rest - head.len();
    let mut out = head;
    if omitted > 0 {
        out.push(format!("… {omitted} more"));
    }
    out.extend(tail);
    out.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `cargo test` on a failing assertion, as `just check` printed it with
    /// both streams merged: the failure is mid-log, cargo's verdict at the end.
    const CARGO_TEST: &str = "   Compiling demo v0.1.0 (/work)
    Finished `test` profile [unoptimized + debuginfo] target(s) in 0.35s
     Running unittests src/lib.rs (target/debug/deps/demo-a618f739376f8207)

running 3 tests
test tests::passes_too ... ok
test tests::adds ... ok
test tests::refills_to_capacity ... FAILED

failures:

---- tests::refills_to_capacity stdout ----

thread 'tests::refills_to_capacity' (903584) panicked at src/lib.rs:6:40:
assertion `left == right` failed: bucket overfilled
  left: 4
 right: 5
note: run with `RUST_BACKTRACE=1` environment variable to display a backtrace


failures:
    tests::refills_to_capacity

test result: FAILED. 2 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s

error: test failed, to rerun pass `--lib`
";

    const NEXTEST: &str = "    Finished `test` profile [unoptimized + debuginfo] target(s) in 0.00s
────────────
 Nextest run ID a12c89c2-492f-460f-ab64-3d3f565b128e with nextest profile: default
    Starting 3 tests across 1 binary
        FAIL [   0.004s] (1/3) demo tests::refills_to_capacity
  stdout ───

    running 1 test
    test tests::refills_to_capacity ... FAILED

    failures:

    failures:
        tests::refills_to_capacity

    test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 2 filtered out; finished in 0.00s

  stderr ───

    thread 'tests::refills_to_capacity' (903691) panicked at src/lib.rs:6:40:
    assertion `left == right` failed: bucket overfilled
      left: 4
     right: 5
    note: run with `RUST_BACKTRACE=1` environment variable to display a backtrace

  Cancelling due to test failure: 2 tests still running
        PASS [   0.004s] (2/3) demo tests::adds
        PASS [   0.004s] (3/3) demo tests::passes_too
────────────
     Summary [   0.005s] 3 tests run: 2 passed, 1 failed, 0 skipped
        FAIL [   0.004s] (1/3) demo tests::refills_to_capacity
error: test run failed
error: recipe `test` failed on line 4 with exit code 100
";

    /// `just check` running clippy with `-D warnings` over code that also
    /// fails to compile its tests.
    const JUST_CLIPPY: &str = "cargo clippy --all-targets -- -D warnings
    Checking demo v0.1.0 (/work)
error: unused variable: `unused`
 --> src/lib.rs:1:41
  |
1 | pub fn add(a: u32, b: u32) -> u32 { let unused = 3; a + b }
  |                                         ^^^^^^ help: if this is intentional, prefix it with an underscore: `_unused`
  |
  = note: `-D unused-variables` implied by `-D warnings`
  = help: to override `-D warnings` add `#[allow(unused_variables)]`

error: could not compile `demo` (lib) due to 1 previous error
warning: build failed, waiting for other jobs to finish...
error[E0599]: no method named `advance` found for struct `Limiter` in the current scope
 --> src/lib.rs:6:33
  |
2 | pub struct Limiter;
  | ------------------ method `advance` not found for this struct
...
6 |     #[test] fn adds() { Limiter.advance(1); assert_eq!(add(2, 2), 4); }
  |                                 ^^^^^^^ method not found in `Limiter`

For more information about this error, try `rustc --explain E0599`.
error: could not compile `demo` (lib test) due to 2 previous errors
error: recipe `check` failed on line 2 with exit code 101
";

    const BUN: &str = "bun test v1.3.14 (0d9b296a)

src/lib/format.test.ts:
1 | import { test, expect, describe } from 'bun:test'
2 | describe('format', () => {
3 |   test('names an agent by its id', () => { expect('abc').toBe('abd') })
                                                             ^
error: expect(received).toBe(expected)

Expected: \"abd\"
Received: \"abc\"

      at <anonymous> (/work/spa/src/lib/format.test.ts:3:58)
(fail) format > names an agent by its id [0.73ms]

 1 pass
 1 fail
 2 expect() calls
Ran 2 tests across 1 file. [12.00ms]
";

    const SVELTE_CHECK: &str = "====================================
Loading svelte-check in workspace: /work/spa
Getting Svelte diagnostics...

/work/spa/src/routes/Review.svelte:612:18
Error: Property 'failures' does not exist on type 'CandidateCheckRun'. (ts)
                <pre>{run.failures}</pre>

====================================
svelte-check found 1 error and 0 warnings in 1 file
error: script \"check\" exited with code 1
";

    #[test]
    fn cargo_test_names_the_test_its_panic_and_the_verdict() {
        let failures = failure_lines(CARGO_TEST);
        assert_eq!(
            failures,
            "test tests::refills_to_capacity ... FAILED
thread 'tests::refills_to_capacity' (903584) panicked at src/lib.rs:6:40:
assertion `left == right` failed: bucket overfilled
left: 4
right: 5
failures:
tests::refills_to_capacity
test result: FAILED. 2 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
error: test failed, to rerun pass `--lib`"
        );
        assert!(!failures.contains("Compiling"));
    }

    #[test]
    fn nextest_keeps_the_failed_test_once_its_panic_and_its_summary() {
        let failures = failure_lines(NEXTEST);
        assert_eq!(
            failures
                .matches("FAIL [   0.004s] (1/3) demo tests::refills_to_capacity")
                .count(),
            1,
            "{failures}"
        );
        for expected in [
            "panicked at src/lib.rs:6:40:",
            "assertion `left == right` failed: bucket overfilled",
            "Summary [   0.005s] 3 tests run: 2 passed, 1 failed, 0 skipped",
            "error: recipe `test` failed on line 4 with exit code 100",
        ] {
            assert!(failures.contains(expected), "{expected} in\n{failures}");
        }
        assert!(!failures.contains("PASS"), "{failures}");
        assert!(!failures.contains("Cancelling"), "{failures}");
    }

    #[test]
    fn compiler_errors_keep_their_location_and_not_their_excerpt() {
        let failures = failure_lines(JUST_CLIPPY);
        assert_eq!(
            failures,
            "error: unused variable: `unused`
--> src/lib.rs:1:41
error: could not compile `demo` (lib) due to 1 previous error
error[E0599]: no method named `advance` found for struct `Limiter` in the current scope
--> src/lib.rs:6:33
error: could not compile `demo` (lib test) due to 2 previous errors
error: recipe `check` failed on line 2 with exit code 101"
        );
    }

    #[test]
    fn bun_keeps_the_expectation_and_the_failed_test() {
        assert_eq!(
            failure_lines(BUN),
            "error: expect(received).toBe(expected)
Expected: \"abd\"
Received: \"abc\"
(fail) format > names an agent by its id [0.73ms]
1 fail"
        );
    }

    #[test]
    fn svelte_check_keeps_the_location_before_its_error() {
        assert_eq!(
            failure_lines(SVELTE_CHECK),
            "/work/spa/src/routes/Review.svelte:612:18
Error: Property 'failures' does not exist on type 'CandidateCheckRun'. (ts)
svelte-check found 1 error and 0 warnings in 1 file
error: script \"check\" exited with code 1"
        );
    }

    #[test]
    fn a_failure_buried_under_later_output_is_still_found() {
        // What the live session saw: the tail held only compiler progress.
        let mut log = String::from(CARGO_TEST);
        for n in 0..500 {
            log.push_str(&format!("   Compiling crate-{n} v1.0.0\n"));
        }
        let failures = failure_lines(&log);
        assert!(failures.contains("tests::refills_to_capacity ... FAILED"));
        assert!(!failures.contains("Compiling"));
    }

    #[test]
    fn colour_and_carriage_returns_are_read_as_a_terminal_shows_them() {
        let log = "\u{1b}[1m\u{1b}[31m        FAIL\u{1b}[0m [   0.004s] demo tests::x\r\n\
                   progress 10%\rprogress 100%\rerror: it broke\n";
        assert_eq!(
            failure_lines(log),
            "FAIL [   0.004s] demo tests::x\nerror: it broke"
        );
    }

    #[test]
    fn a_passing_log_and_ordinary_words_yield_nothing() {
        let log = "   Compiling demo v0.1.0 (/work)
running 2 tests
test tests::error_paths ... ok
test tests::fails_closed ... ok
test result: ok. 2 passed; 0 failed
     Summary [   0.005s] 2 tests run: 2 passed, 0 skipped
svelte-check found 0 errors and 0 warnings
";
        assert_eq!(failure_lines(log), "");
    }

    #[test]
    fn many_failures_keep_the_first_and_the_last_within_the_bound() {
        let mut log = String::new();
        for n in 0..400 {
            log.push_str(&format!("test tests::case_{n:03} ... FAILED\n"));
        }
        log.push_str("test result: FAILED. 0 passed; 400 failed\n");
        let failures = failure_lines(&log);
        assert!(failures.len() <= MAX_BYTES + 40, "{}", failures.len());
        assert!(failures.lines().count() <= MAX_LINES + 1);
        assert!(failures.starts_with("test tests::case_000 ... FAILED"));
        assert!(failures.ends_with("test result: FAILED. 0 passed; 400 failed"));
        assert!(failures.contains(" more\n"), "{failures}");
    }

    #[test]
    fn a_long_line_is_cut_on_a_character_boundary() {
        let log = format!("error: {}\n", "é".repeat(400));
        let failures = failure_lines(&log);
        assert!(failures.len() <= MAX_LINE + "…".len());
        assert!(failures.ends_with('…'));
    }

    #[test]
    fn the_text_given_for_a_failure_leads_with_what_failed() {
        assert_eq!(with_failures("", "tail"), "tail");
        assert_eq!(
            with_failures("error: x", "tail"),
            "What failed:\nerror: x\n\nEnd of the output:\ntail"
        );
    }

    /// The merged shell runs the command exactly as `sh -lc` would, with
    /// stderr landing in stdout in the order it was written.
    #[tokio::test]
    async fn the_merged_shell_interleaves_both_streams_in_order() {
        let argv = merged_shell("echo one; echo two >&2; echo three; exit 7");
        let output = tokio::process::Command::new(&argv[0])
            .args(&argv[1..])
            .output()
            .await
            .unwrap();
        assert_eq!(output.status.code(), Some(7));
        assert_eq!(String::from_utf8_lossy(&output.stdout), "one\ntwo\nthree\n");
        assert!(output.stderr.is_empty());
    }
}
