use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};

use super::{args, is_binary, read, Context, Output, Tool};
use crate::hashline::{self, Text};

/// Lines of context shown around each edited region in the result, so the
/// model has fresh anchors for a follow-up edit without reading again.
const CONTEXT: usize = 2;

pub struct Edit;

#[derive(Deserialize)]
struct Args {
    path: String,
    edits: Vec<Op>,
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum Kind {
    Replace,
    InsertAfter,
    InsertBefore,
    Delete,
}

#[derive(Deserialize)]
struct Op {
    op: Kind,
    start: String,
    end: Option<String>,
    #[serde(default)]
    text: String,
}

/// An op reduced to "at line index `at`, remove `remove` lines, put `lines`".
struct Splice {
    at: usize,
    remove: usize,
    lines: Vec<String>,
}

#[async_trait]
impl Tool for Edit {
    fn name(&self) -> &'static str {
        "edit"
    }

    fn description(&self) -> &'static str {
        "Edit a file by the `LINE:HASH` anchors that `read` shows. Ops: `replace` lines \
         start..end (inclusive) with `text`, where `end` may be left out only when `text` \
         is a single line; `insert_after` or \
         `insert_before` the start line; `delete` lines start..end. `text` is the new \
         lines, without anchors. If any anchor is stale the whole edit is refused and \
         nothing is written: read the file again and retry with the new anchors."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "File path, relative to the working directory."},
                "edits": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "op": {"type": "string", "enum": ["replace", "insert_after", "insert_before", "delete"]},
                            "start": {"type": "string", "description": "Anchor `LINE:HASH` from read, e.g. `12:a3f9`."},
                            "end": {"type": "string", "description": "Last anchor of the range for replace and delete."},
                            "text": {"type": "string", "description": "The new lines for replace and insert ops."}
                        },
                        "required": ["op", "start"]
                    }
                }
            },
            "required": ["path", "edits"]
        })
    }

    async fn run(&self, cx: &Context, input: Value) -> Output {
        let a: Args = match args(input) {
            Ok(a) => a,
            Err(e) => return e,
        };
        if a.edits.is_empty() {
            return Output::error("no edits given");
        }
        let path = match cx.resolve(&a.path) {
            Ok(p) => p,
            Err(e) => return Output::error(e),
        };
        let bytes = match tokio::fs::read(&path).await {
            Ok(b) => b,
            Err(e) => return Output::error(format!("{}: {e}; use write to create a file", a.path)),
        };
        if is_binary(&bytes) {
            return Output::error(format!("{} is a binary file", a.path));
        }
        let mut text = Text::parse(&String::from_utf8_lossy(&bytes));
        let splices = match plan(&text, &a.edits) {
            Ok(s) => s,
            Err(e) => return Output::error(format!("{}: nothing was written.\n{e}", a.path)),
        };
        let regions = apply(&mut text, splices);
        if let Err(e) = write_atomic(&path, text.render().as_bytes()).await {
            return Output::error(format!("{}: {e}", a.path));
        }
        Output::ok(summary(&a.path, &text, &regions, a.edits.len()))
    }
}

/// Checks every anchor and range before anything changes; one stale anchor
/// refuses the whole edit.
fn plan(text: &Text, ops: &[Op]) -> Result<Vec<Splice>, String> {
    let mut problems = Vec::new();
    let mut splices = Vec::new();
    for (i, op) in ops.iter().enumerate() {
        let n = i + 1;
        let mut anchor = |label: &str, s: &str| match hashline::parse(s) {
            Ok(a) => match text.check(&a) {
                None => Some(a.line),
                Some(why) => {
                    problems.push(format!("edit {n} {label}: {why}"));
                    None
                }
            },
            Err(why) => {
                problems.push(format!("edit {n} {label}: {why}"));
                None
            }
        };
        let start = anchor("start", &op.start);
        let end = match (&op.op, &op.end) {
            (Kind::Replace | Kind::Delete, Some(end)) => anchor("end", end),
            _ => start,
        };
        let (Some(start), Some(end)) = (start, end) else {
            continue;
        };
        if end < start {
            problems.push(format!(
                "edit {n}: end line {end} is before start line {start}"
            ));
            continue;
        }
        // Models read a replace naming only `start` as "replace the block that
        // starts here" and send the whole new block, which leaves the old one
        // below it. Seen live on Qwen3-Coder-Next three times in one session.
        let text_lines = op.text.lines().count();
        if matches!(op.op, Kind::Replace) && op.end.is_none() && text_lines > 1 {
            problems.push(format!(
                "edit {n}: a replace without `end` replaces only line {start}, but `text` \
                 has {text_lines} lines. Name the last line being replaced in `end`; to \
                 turn one line into several, give `end` the same anchor as `start`"
            ));
            continue;
        }
        let lines = || op.text.lines().map(str::to_string).collect();
        splices.push(match op.op {
            Kind::Replace => Splice {
                at: start - 1,
                remove: end - start + 1,
                lines: lines(),
            },
            Kind::Delete => Splice {
                at: start - 1,
                remove: end - start + 1,
                lines: Vec::new(),
            },
            Kind::InsertAfter => Splice {
                at: start,
                remove: 0,
                lines: lines(),
            },
            Kind::InsertBefore => Splice {
                at: start - 1,
                remove: 0,
                lines: lines(),
            },
        });
    }
    if !problems.is_empty() {
        return Err(format!(
            "{}\nRetry with anchors copied from the latest read; read the file again if it may have changed.",
            problems.join("\n")
        ));
    }
    // Inserts at an index sort before a replacement starting there, and keep
    // their given order among themselves.
    splices.sort_by_key(|s| (s.at, s.remove > 0));
    for pair in splices.windows(2) {
        if pair[0].at + pair[0].remove > pair[1].at {
            return Err(format!(
                "edits overlap around line {}; combine them into one replace",
                pair[1].at + 1
            ));
        }
    }
    Ok(splices)
}

/// Applies bottom-up so earlier indices stay valid; returns each edited
/// region as 0-based `(start, len)` in the new file.
fn apply(text: &mut Text, splices: Vec<Splice>) -> Vec<(usize, usize)> {
    let mut regions = Vec::new();
    let mut shift: isize = 0;
    for s in &splices {
        regions.push(((s.at as isize + shift) as usize, s.lines.len()));
        shift += s.lines.len() as isize - s.remove as isize;
    }
    for s in splices.into_iter().rev() {
        text.lines.splice(s.at..s.at + s.remove, s.lines);
    }
    regions
}

fn summary(path: &str, text: &Text, regions: &[(usize, usize)], count: usize) -> String {
    let total = text.lines.len();
    let mut out = format!(
        "Edited {path}: {count} edit{}, now {total} lines.",
        if count == 1 { "" } else { "s" }
    );
    if total == 0 {
        return out;
    }
    // Merge the context windows so overlapping ones print once.
    let mut windows: Vec<(usize, usize)> = Vec::new();
    for &(at, len) in regions {
        let lo = at.saturating_sub(CONTEXT);
        let hi = (at + len.max(1) + CONTEXT).min(total);
        match windows.last_mut() {
            Some(last) if lo <= last.1 => last.1 = last.1.max(hi),
            _ => windows.push((lo, hi)),
        }
    }
    out.push_str(" Current anchors around the change:\n");
    for (lo, hi) in windows {
        if lo >= hi {
            continue;
        }
        out.push_str(&read::lines(text, lo + 1, hi));
        out.push_str("…\n");
    }
    out
}

pub(super) async fn write_atomic(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let tmp = path.with_file_name(format!(".{name}.tracon-agent.tmp"));
    tokio::fs::write(&tmp, bytes).await?;
    if let Ok(meta) = tokio::fs::metadata(path).await {
        tokio::fs::set_permissions(&tmp, meta.permissions()).await?;
    }
    if let Err(e) = tokio::fs::rename(&tmp, path).await {
        let _ = tokio::fs::remove_file(&tmp).await;
        return Err(e);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hashline::anchor;

    fn setup(content: &str) -> (tempfile::TempDir, Context) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("f.py"), content).unwrap();
        let cx = Context::new(dir.path()).unwrap();
        (dir, cx)
    }

    fn file(dir: &tempfile::TempDir) -> String {
        std::fs::read_to_string(dir.path().join("f.py")).unwrap()
    }

    #[tokio::test]
    async fn replace_insert_delete_in_one_call() {
        let (dir, cx) = setup("a\nb\nc\nd\n");
        let out = Edit
            .run(
                &cx,
                json!({"path": "f.py", "edits": [
                    {"op": "delete", "start": anchor(4, "d")},
                    {"op": "replace", "start": anchor(2, "b"), "end": anchor(3, "c"), "text": "B\nC\nC2\n"},
                    {"op": "insert_before", "start": anchor(1, "a"), "text": "top"},
                    {"op": "insert_after", "start": anchor(1, "a"), "text": "a2"}
                ]}),
            )
            .await;
        assert!(!out.is_error, "{}", out.content);
        assert_eq!(file(&dir), "top\na\na2\nB\nC\nC2\n");
        assert!(
            out.content.contains(&hashline::format(4, "B")),
            "{}",
            out.content
        );
    }

    #[tokio::test]
    async fn a_stale_anchor_refuses_the_whole_edit() {
        let (dir, cx) = setup("a\nb\n");
        let out = Edit
            .run(
                &cx,
                json!({"path": "f.py", "edits": [
                    {"op": "replace", "start": anchor(1, "a"), "text": "A"},
                    {"op": "replace", "start": anchor(2, "old b"), "text": "B"}
                ]}),
            )
            .await;
        assert!(out.is_error);
        assert!(
            out.content.contains("line 2 changed since it was read"),
            "{}",
            out.content
        );
        assert!(out.content.contains(&hashline::format(2, "b")));
        assert_eq!(file(&dir), "a\nb\n");
    }

    #[tokio::test]
    async fn overlapping_edits_are_refused() {
        let (dir, cx) = setup("a\nb\nc\n");
        let out = Edit
            .run(
                &cx,
                json!({"path": "f.py", "edits": [
                    {"op": "replace", "start": anchor(1, "a"), "end": anchor(2, "b"), "text": "x"},
                    {"op": "delete", "start": anchor(2, "b")}
                ]}),
            )
            .await;
        assert!(out.is_error);
        assert!(out.content.contains("overlap"));
        assert_eq!(file(&dir), "a\nb\nc\n");
    }

    #[tokio::test]
    async fn a_block_replace_without_end_is_refused() {
        let (dir, cx) = setup("def f():\n    return 1\n");
        let out = Edit
            .run(
                &cx,
                json!({"path": "f.py", "edits": [
                    {"op": "replace", "start": anchor(1, "def f():"), "text": "def f():\n    return 2"}
                ]}),
            )
            .await;
        assert!(out.is_error);
        assert!(
            out.content
                .contains("replaces only line 1, but `text` has 2 lines"),
            "{}",
            out.content
        );
        assert_eq!(file(&dir), "def f():\n    return 1\n");
    }

    #[tokio::test]
    async fn one_line_becomes_several_when_end_names_it() {
        let (dir, cx) = setup("a\nb\nc\n");
        let out = Edit
            .run(
                &cx,
                json!({"path": "f.py", "edits": [
                    {"op": "replace", "start": anchor(2, "b"), "end": anchor(2, "b"), "text": "b1\nb2"}
                ]}),
            )
            .await;
        assert!(!out.is_error, "{}", out.content);
        assert_eq!(file(&dir), "a\nb1\nb2\nc\n");
    }

    #[tokio::test]
    async fn keeps_crlf_and_mode() {
        use std::os::unix::fs::PermissionsExt;
        let (dir, cx) = setup("a\r\nb\r\n");
        let p = dir.path().join("f.py");
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        let out = Edit
            .run(
                &cx,
                json!({"path": "f.py", "edits": [{"op": "replace", "start": anchor(2, "b"), "text": "B"}]}),
            )
            .await;
        assert!(!out.is_error, "{}", out.content);
        assert_eq!(file(&dir), "a\r\nB\r\n");
        assert_eq!(
            std::fs::metadata(&p).unwrap().permissions().mode() & 0o777,
            0o755
        );
    }
}
