use async_trait::async_trait;
use regex::RegexBuilder;
use serde::Deserialize;
use serde_json::{json, Value};

use super::{args, glob::Matcher, is_binary, Context, Output, Tool};
use crate::hashline;

const MAX_MATCHES: usize = 200;
const MAX_LINE: usize = 300;

pub struct Grep;

#[derive(Deserialize)]
struct Args {
    pattern: String,
    path: Option<String>,
    glob: Option<String>,
    #[serde(default, deserialize_with = "super::null_as_default")]
    case_insensitive: bool,
}

#[async_trait]
impl Tool for Grep {
    fn name(&self) -> &'static str {
        "grep"
    }

    fn description(&self) -> &'static str {
        "Search file contents with a regular expression. Matches are shown as \
         `path:LINE:HASH|text`; the `LINE:HASH` anchors work with edit. Respects .gitignore."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": {"type": "string", "description": "Regular expression (Rust syntax)."},
                "path": {"type": "string", "description": "File or directory to search. Default: the working directory."},
                "glob": {"type": "string", "description": "Only search files matching this glob, e.g. `*.py`."},
                "case_insensitive": {"type": "boolean"}
            },
            "required": ["pattern"]
        })
    }

    async fn run(&self, cx: &Context, input: Value) -> Output {
        let a: Args = match args(input) {
            Ok(a) => a,
            Err(e) => return e,
        };
        let base = match cx.resolve(a.path.as_deref().unwrap_or(".")) {
            Ok(p) => p,
            Err(e) => return Output::error(e),
        };
        let re = match RegexBuilder::new(&a.pattern)
            .case_insensitive(a.case_insensitive)
            .build()
        {
            Ok(r) => r,
            Err(e) => return Output::error(format!("bad regex: {e}")),
        };
        let filter = match a.glob.as_deref().map(Matcher::new).transpose() {
            Ok(m) => m,
            Err(e) => return Output::error(e),
        };
        let mut files: Vec<_> = ignore::WalkBuilder::new(&base)
            .build()
            .flatten()
            .filter(|e| e.file_type().is_some_and(|t| t.is_file()))
            .map(|e| e.into_path())
            .filter(|p| {
                filter
                    .as_ref()
                    .is_none_or(|m| m.matches(p.strip_prefix(&base).unwrap_or(p)))
            })
            .collect();
        files.sort();
        let mut out = Vec::new();
        let mut total = 0;
        for file in files {
            let Ok(bytes) = std::fs::read(&file) else {
                continue;
            };
            if is_binary(&bytes) {
                continue;
            }
            let text = String::from_utf8_lossy(&bytes);
            for (i, line) in text.lines().enumerate() {
                if !re.is_match(line) {
                    continue;
                }
                total += 1;
                if out.len() < MAX_MATCHES {
                    let shown: String = line.chars().take(MAX_LINE).collect();
                    out.push(format!(
                        "{}:{}|{shown}",
                        cx.display(&file),
                        hashline::anchor(i + 1, line)
                    ));
                }
            }
        }
        if out.is_empty() {
            return Output::ok(format!("no matches for `{}`", a.pattern));
        }
        let mut out = out.join("\n");
        if total > MAX_MATCHES {
            out.push_str(&format!(
                "\n[{MAX_MATCHES} of {total} matches shown; narrow the search]"
            ));
        }
        Output::ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn finds_with_anchors_and_filters() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.py"), "x = 1\ndef foo():\n").unwrap();
        std::fs::write(dir.path().join("b.rs"), "fn foo() {}\n").unwrap();
        let cx = Context::new(dir.path()).unwrap();
        let out = Grep
            .run(&cx, json!({"pattern": "foo", "glob": "*.py"}))
            .await;
        assert_eq!(
            out.content,
            format!("a.py:{}", hashline::format(2, "def foo():"))
        );
        let out = Grep
            .run(&cx, json!({"pattern": "FOO", "case_insensitive": true}))
            .await;
        assert_eq!(out.content.lines().count(), 2);
    }

    #[tokio::test]
    async fn caps_matches() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a"), "hit\n".repeat(MAX_MATCHES + 3)).unwrap();
        let cx = Context::new(dir.path()).unwrap();
        let out = Grep.run(&cx, json!({"pattern": "hit"})).await;
        assert_eq!(out.content.lines().count(), MAX_MATCHES + 1);
        assert!(out
            .content
            .contains(&format!("of {} matches", MAX_MATCHES + 3)));
    }
}
