//! One JSON object per line for everything the run does: requests and their
//! usage and speed, each tool call and result, the end. Standalone it is the
//! measurement; under the node it becomes `session/update` and `usage`.

use std::io::Write;
use std::path::Path;
use std::sync::Mutex;
use std::time::Instant;

use serde_json::{json, Value};

pub struct Events {
    out: Mutex<Box<dyn Write + Send>>,
    start: Instant,
}

impl Events {
    pub fn to_file(path: &Path) -> std::io::Result<Self> {
        Ok(Self::new(Box::new(std::fs::File::create(path)?)))
    }

    pub fn to_stderr() -> Self {
        Self::new(Box::new(std::io::stderr()))
    }

    pub fn new(out: Box<dyn Write + Send>) -> Self {
        Self {
            out: Mutex::new(out),
            start: Instant::now(),
        }
    }

    pub fn emit(&self, event: &str, data: Value) {
        let mut line = json!({
            "t_ms": self.start.elapsed().as_millis() as u64,
            "event": event,
        });
        if let (Some(line), Value::Object(data)) = (line.as_object_mut(), data) {
            line.extend(data);
        }
        let mut out = self.out.lock().unwrap_or_else(|e| e.into_inner());
        // Losing a measurement line is not worth stopping the run for.
        let _ = writeln!(out, "{line}");
        let _ = out.flush();
    }
}
