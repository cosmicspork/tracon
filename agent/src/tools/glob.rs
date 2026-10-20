use async_trait::async_trait;
use globset::{GlobBuilder, GlobMatcher};
use serde::Deserialize;
use serde_json::{json, Value};

use super::{args, Context, Output, Tool};

const MAX_RESULTS: usize = 200;

pub struct Glob;

#[derive(Deserialize)]
struct Args {
    pattern: String,
    path: Option<String>,
}

/// A pattern without a `/` matches file names anywhere, as `rg -g` does; one
/// with a `/` matches the path relative to the search directory.
pub(super) struct Matcher {
    glob: GlobMatcher,
    by_name: bool,
}

impl Matcher {
    pub(super) fn new(pattern: &str) -> Result<Self, String> {
        let pattern = pattern.trim().trim_start_matches("./");
        // `*` stops at `/`, so `src/*.py` is one level and `src/**/*.py` any.
        let glob = GlobBuilder::new(pattern)
            .literal_separator(true)
            .build()
            .map_err(|e| format!("bad glob `{pattern}`: {e}"))?
            .compile_matcher();
        Ok(Self {
            glob,
            by_name: !pattern.contains('/'),
        })
    }

    pub(super) fn matches(&self, rel: &std::path::Path) -> bool {
        if self.by_name {
            rel.file_name().is_some_and(|n| self.glob.is_match(n))
        } else {
            self.glob.is_match(rel)
        }
    }
}

#[async_trait]
impl Tool for Glob {
    fn name(&self) -> &'static str {
        "glob"
    }

    fn description(&self) -> &'static str {
        "List files matching a glob, e.g. `*.py` (any depth) or `src/**/*.rs`. \
         Respects .gitignore. Use `*` to list everything."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": {"type": "string", "description": "Glob pattern."},
                "path": {"type": "string", "description": "Directory to search. Default: the working directory."}
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
        let matcher = match Matcher::new(&a.pattern) {
            Ok(m) => m,
            Err(e) => return Output::error(e),
        };
        let mut found = Vec::new();
        for entry in ignore::WalkBuilder::new(&base).build().flatten() {
            if !entry.file_type().is_some_and(|t| t.is_file()) {
                continue;
            }
            let rel = entry.path().strip_prefix(&base).unwrap_or(entry.path());
            if matcher.matches(rel) {
                found.push(cx.display(entry.path()));
            }
        }
        found.sort();
        if found.is_empty() {
            return Output::ok(format!("no files match `{}`", a.pattern));
        }
        let total = found.len();
        found.truncate(MAX_RESULTS);
        let mut out = found.join("\n");
        if total > MAX_RESULTS {
            out.push_str(&format!(
                "\n[{MAX_RESULTS} of {total} shown; narrow the pattern]"
            ));
        }
        Output::ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn matches_names_at_depth_and_paths() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src/deep")).unwrap();
        for f in ["a.py", "src/b.py", "src/deep/c.py", "src/d.rs"] {
            std::fs::write(dir.path().join(f), "").unwrap();
        }
        let cx = Context::new(dir.path()).unwrap();
        let out = Glob.run(&cx, json!({"pattern": "*.py"})).await;
        assert_eq!(out.content, "a.py\nsrc/b.py\nsrc/deep/c.py");
        let out = Glob.run(&cx, json!({"pattern": "src/*.py"})).await;
        assert_eq!(out.content, "src/b.py");
        let out = Glob
            .run(&cx, json!({"pattern": "*", "path": "src/deep"}))
            .await;
        assert_eq!(out.content, "src/deep/c.py");
    }

    #[tokio::test]
    async fn caps_results() {
        let dir = tempfile::tempdir().unwrap();
        for i in 0..(MAX_RESULTS + 5) {
            std::fs::write(dir.path().join(format!("f{i:03}.txt")), "").unwrap();
        }
        let cx = Context::new(dir.path()).unwrap();
        let out = Glob.run(&cx, json!({"pattern": "*.txt"})).await;
        assert_eq!(out.content.lines().count(), MAX_RESULTS + 1);
        assert!(out.content.ends_with("narrow the pattern]"));
    }
}
