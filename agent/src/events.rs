//! One JSON object per line for everything the run does: requests and their
//! usage and speed, each tool call and result, the end. Standalone it is the
//! measurement; under the node it becomes `session/update` and `usage`.

use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::sync::Mutex;
use std::time::Instant;

use serde_json::{json, Value};

pub struct Events {
    out: Mutex<Box<dyn Write + Send>>,
    start: Instant,
}

impl Events {
    /// Appended to, never truncated: a process that replaces one that died
    /// carries on the same record.
    pub fn to_file(path: &Path) -> std::io::Result<Self> {
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)?;
        Ok(Self::new(Box::new(file)))
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

/// The highest `seq` an events file records for a session's updates, so a
/// process carrying the session on numbers from there.
pub fn last_seq(path: &Path, session_id: &str) -> u64 {
    let Ok(file) = std::fs::File::open(path) else {
        return 0;
    };
    BufReader::new(file)
        .lines()
        .map_while(Result::ok)
        .filter_map(|line| serde_json::from_str::<Value>(&line).ok())
        .filter(|e| e["event"] == "update" && e["session_id"] == session_id)
        .filter_map(|e| e["seq"].as_u64())
        .max()
        .unwrap_or(0)
}
