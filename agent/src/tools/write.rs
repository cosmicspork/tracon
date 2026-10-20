use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};

use super::{args, edit::write_atomic, Context, Output, Tool};

pub struct Write;

#[derive(Deserialize)]
struct Args {
    path: String,
    content: String,
}

#[async_trait]
impl Tool for Write {
    fn name(&self) -> &'static str {
        "write"
    }

    fn description(&self) -> &'static str {
        "Create a file, or replace a file's whole content. Parent directories are \
         created. To change part of an existing file, use edit."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "File path, relative to the working directory."},
                "content": {"type": "string", "description": "The complete file content."}
            },
            "required": ["path", "content"]
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
            return Output::error(format!("{} is a directory", a.path));
        }
        let existed = path.exists();
        if let Some(parent) = path.parent() {
            if let Err(e) = tokio::fs::create_dir_all(parent).await {
                return Output::error(format!("{}: {e}", a.path));
            }
        }
        if let Err(e) = write_atomic(&path, a.content.as_bytes()).await {
            return Output::error(format!("{}: {e}", a.path));
        }
        Output::ok(format!(
            "{} {} ({} lines)",
            if existed { "Replaced" } else { "Created" },
            cx.display(&path),
            a.content.lines().count()
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn creates_parents_and_refuses_escape() {
        let dir = tempfile::tempdir().unwrap();
        let cx = Context::new(dir.path()).unwrap();
        let out = Write
            .run(&cx, json!({"path": "a/b/c.txt", "content": "hi\n"}))
            .await;
        assert_eq!(out, Output::ok("Created a/b/c.txt (1 lines)"));
        assert_eq!(
            std::fs::read_to_string(dir.path().join("a/b/c.txt")).unwrap(),
            "hi\n"
        );
        assert!(
            Write
                .run(&cx, json!({"path": "../x", "content": ""}))
                .await
                .is_error
        );
    }
}
