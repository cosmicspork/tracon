use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};

use super::{args, is_binary, Context, Output, Tool};
use crate::hashline::{self, Text};

/// Enough of a file for a small context window to hold several.
const DEFAULT_LIMIT: usize = 400;
const MAX_LINE: usize = 1000;

pub struct Read;

#[derive(Deserialize)]
struct Args {
    path: String,
    /// 1-based first line.
    offset: Option<usize>,
    limit: Option<usize>,
}

#[async_trait]
impl Tool for Read {
    fn name(&self) -> &'static str {
        "read"
    }

    fn description(&self) -> &'static str {
        "Read a text file. Each line is shown as `LINE:HASH|text`; pass `LINE:HASH` \
         anchors to `edit`. Use offset and limit for long files."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "File path, relative to the working directory."},
                "offset": {"type": "integer", "description": "First line to show, 1-based. Default 1."},
                "limit": {"type": "integer", "description": "Number of lines to show. Default 400."}
            },
            "required": ["path"]
        })
    }

    async fn run(&self, cx: &Context, input: Value) -> Output {
        let a: Args = match args(input) {
            Ok(a) => a,
            Err(e) => return e,
        };
        let path = match cx.resolve(&a.path) {
            Ok(p) => p,
            Err(e) => return Output::error(e),
        };
        if path.is_dir() {
            return Output::error(format!("{} is a directory; use glob to list files", a.path));
        }
        let bytes = match tokio::fs::read(&path).await {
            Ok(b) => b,
            Err(e) => return Output::error(format!("{}: {e}", a.path)),
        };
        if is_binary(&bytes) {
            return Output::error(format!("{} is a binary file", a.path));
        }
        let text = Text::parse(&String::from_utf8_lossy(&bytes));
        let total = text.lines.len();
        if total == 0 {
            return Output::ok(format!("{} is empty", a.path));
        }
        let start = a.offset.unwrap_or(1).max(1);
        if start > total {
            return Output::error(format!(
                "offset {start} is past the end; {} has {total} lines",
                a.path
            ));
        }
        let end = (start - 1 + a.limit.unwrap_or(DEFAULT_LIMIT).max(1)).min(total);
        Output::ok(render(&text, start, end, total))
    }
}

/// Lines `start..=end` (1-based) as hashlines, with a note when there is more.
fn render(text: &Text, start: usize, end: usize, total: usize) -> String {
    let mut out = lines(text, start, end);
    if start > 1 || end < total {
        out.push_str(&format!(
            "[lines {start}-{end} of {total}; use offset to read more]\n"
        ));
    }
    out
}

pub(super) fn lines(text: &Text, start: usize, end: usize) -> String {
    let mut out = String::new();
    for (i, line) in text.lines[start - 1..end].iter().enumerate() {
        let n = start + i;
        // The anchor hashes the whole line even when the display is cut.
        let anchor = hashline::anchor(n, line);
        if line.chars().count() > MAX_LINE {
            let cut: String = line.chars().take(MAX_LINE).collect();
            out.push_str(&format!("{anchor}|{cut}… [line truncated]\n"));
        } else {
            out.push_str(&format!("{anchor}|{line}\n"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn reads_hashlines_with_a_window() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("f.txt"), "a\nb\nc\n").unwrap();
        let cx = Context::new(dir.path()).unwrap();
        let out = Read
            .run(&cx, json!({"path": "f.txt", "offset": 2, "limit": 1}))
            .await;
        assert!(!out.is_error);
        assert_eq!(
            out.content,
            format!(
                "{}\n[lines 2-2 of 3; use offset to read more]\n",
                hashline::format(2, "b")
            )
        );
    }

    #[tokio::test]
    async fn refuses_binary_and_directories() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("b"), [0u8, 1, 2]).unwrap();
        let cx = Context::new(dir.path()).unwrap();
        assert!(Read.run(&cx, json!({"path": "b"})).await.is_error);
        assert!(Read.run(&cx, json!({"path": "."})).await.is_error);
    }
}
