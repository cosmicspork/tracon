//! The in-boundary tools, and nothing else: `read`, `edit`, `write`, `glob`,
//! `grep`, `shell`. Node tools arrive over MCP once there is a node.
//!
//! Every path a tool touches is resolved inside the root. Inside a session the
//! container is the real boundary; this keeps a standalone run from wandering
//! out of its scratch directory, and gives the model a plain refusal rather
//! than a surprise.

use std::path::{Component, Path, PathBuf};

use async_trait::async_trait;
use serde::de::DeserializeOwned;
use serde_json::{json, Value};

mod edit;
mod glob;
mod grep;
mod read;
mod shell;
mod write;

pub use edit::Edit;
pub use glob::Glob;
pub use grep::Grep;
pub use read::Read;
pub use shell::Shell;
pub use write::Write;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Output {
    pub content: String,
    pub is_error: bool,
}

impl Output {
    pub fn ok(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: false,
        }
    }

    pub fn error(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: true,
        }
    }
}

pub struct Context {
    /// Canonical.
    pub root: PathBuf,
}

impl Context {
    pub fn new(root: &Path) -> std::io::Result<Self> {
        Ok(Self {
            root: root.canonicalize()?,
        })
    }

    /// Resolves a path the model named to one inside the root, or says why
    /// not. Absolute paths are accepted when they are inside it: models copy
    /// them out of shell output.
    pub fn resolve(&self, path: &str) -> Result<PathBuf, String> {
        let path = path.trim();
        let joined = if Path::new(path).is_absolute() {
            PathBuf::from(path)
        } else {
            self.root.join(path)
        };
        let mut normal = PathBuf::new();
        for part in joined.components() {
            match part {
                Component::ParentDir => {
                    normal.pop();
                }
                Component::CurDir => {}
                other => normal.push(other),
            }
        }
        // Symlinks are judged by where they lead: canonicalise the deepest
        // part that exists and put the rest back on. A dangling link counts as
        // existing, so it fails to canonicalise rather than being written through.
        let mut existing = normal.as_path();
        let mut rest = Vec::new();
        while existing.symlink_metadata().is_err() {
            match (existing.parent(), existing.file_name()) {
                (Some(parent), Some(name)) => {
                    rest.push(name.to_owned());
                    existing = parent;
                }
                _ => break,
            }
        }
        let mut resolved = existing
            .canonicalize()
            .map_err(|e| format!("{path}: {e}"))?;
        for name in rest.into_iter().rev() {
            resolved.push(name);
        }
        if resolved.starts_with(&self.root) {
            Ok(resolved)
        } else {
            Err(format!(
                "{path} is outside the working directory; paths must stay under it"
            ))
        }
    }

    /// The path as the model should see it: relative to the root.
    pub fn display(&self, path: &Path) -> String {
        let rel = path.strip_prefix(&self.root).unwrap_or(path);
        if rel.as_os_str().is_empty() {
            ".".into()
        } else {
            rel.display().to_string()
        }
    }
}

#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &'static str;
    fn description(&self) -> &'static str;
    /// JSON Schema for the arguments.
    fn parameters(&self) -> Value;
    async fn run(&self, cx: &Context, input: Value) -> Output;
}

/// Arguments the schema promised but the model did not keep to come back as a
/// tool error naming the problem, so the model can correct the call.
pub fn args<T: DeserializeOwned>(input: Value) -> Result<T, Output> {
    serde_json::from_value(input).map_err(|e| Output::error(format!("invalid arguments: {e}")))
}

pub struct Registry {
    tools: Vec<Box<dyn Tool>>,
}

impl Default for Registry {
    fn default() -> Self {
        Self {
            tools: vec![
                Box::new(Read),
                Box::new(Edit),
                Box::new(Write),
                Box::new(Glob),
                Box::new(Grep),
                Box::new(Shell),
            ],
        }
    }
}

impl Registry {
    pub fn get(&self, name: &str) -> Option<&dyn Tool> {
        self.tools
            .iter()
            .find(|t| t.name() == name)
            .map(|t| t.as_ref())
    }

    pub fn names(&self) -> Vec<&'static str> {
        self.tools.iter().map(|t| t.name()).collect()
    }

    /// Tool specs in a neutral shape; each encoder wraps them for its wire.
    pub fn specs(&self) -> Vec<Value> {
        self.tools
            .iter()
            .map(|t| {
                json!({
                    "name": t.name(),
                    "description": t.description(),
                    "parameters": t.parameters(),
                })
            })
            .collect()
    }

    pub async fn run(&self, cx: &Context, name: &str, input: Value) -> Output {
        let Some(tool) = self.get(name) else {
            return Output::error(format!(
                "unknown tool `{name}`; the tools are: {}",
                self.names().join(", ")
            ));
        };
        if let Value::String(raw) = &input {
            return Output::error(format!(
                "the arguments were not valid JSON: {raw}\nCall {name} again with a JSON object."
            ));
        }
        tool.run(cx, input).await
    }
}

/// Files that are binary to the model: a NUL in the first 8 KiB, as git judges.
pub(crate) fn is_binary(bytes: &[u8]) -> bool {
    bytes.iter().take(8192).any(|b| *b == 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_stays_inside_the_root() {
        let dir = tempfile::tempdir().unwrap();
        let cx = Context::new(dir.path()).unwrap();
        std::fs::create_dir(dir.path().join("src")).unwrap();
        assert_eq!(cx.resolve("src/a.rs").unwrap(), cx.root.join("src/a.rs"));
        assert_eq!(cx.resolve("./src/../b").unwrap(), cx.root.join("b"));
        assert_eq!(
            cx.resolve(cx.root.join("new/dir/c").to_str().unwrap())
                .unwrap(),
            cx.root.join("new/dir/c")
        );
        assert!(cx.resolve("../escape").is_err());
        assert!(cx.resolve("src/../../escape").is_err());
        assert!(cx.resolve("/etc/passwd").is_err());
    }

    #[test]
    fn resolve_judges_a_symlink_by_its_target() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let cx = Context::new(dir.path()).unwrap();
        std::os::unix::fs::symlink(outside.path(), dir.path().join("out")).unwrap();
        assert!(cx.resolve("out").is_err());
        assert!(cx.resolve("out/new-file").is_err());
    }
}
