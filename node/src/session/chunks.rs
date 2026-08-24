//! Streaming text arrives in many small chunks. They are published live for the
//! interface and coalesced into one persisted event per message, so the log is
//! readable rather than a list of syllables.

#[derive(Default)]
pub struct ChunkBuffer {
    open: Option<Open>,
}

struct Open {
    kind: &'static str,
    message_id: Option<String>,
    text: String,
}

impl ChunkBuffer {
    /// Add a chunk. Returns a completed `(kind, message_id, text)` when this
    /// chunk belongs to a different message than the one being buffered.
    pub fn push(
        &mut self,
        kind: &'static str,
        message_id: Option<String>,
        text: &str,
    ) -> Option<(&'static str, Option<String>, String)> {
        match &mut self.open {
            Some(open) if open.kind == kind && open.message_id == message_id => {
                open.text.push_str(text);
                None
            }
            _ => {
                let finished = self.take();
                self.open = Some(Open {
                    kind,
                    message_id,
                    text: text.to_string(),
                });
                finished
            }
        }
    }

    fn take(&mut self) -> Option<(&'static str, Option<String>, String)> {
        self.open
            .take()
            .filter(|o| !o.text.is_empty())
            .map(|o| (o.kind, o.message_id, o.text))
    }

    /// Flush whatever is buffered. Called before any non-chunk event, at turn
    /// end, and when the session stops, so a crash loses at most one message.
    pub fn flush_all(&mut self, mut f: impl FnMut(&'static str, Option<String>, String)) {
        if let Some((kind, id, text)) = self.take() {
            f(kind, id, text);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MSG: &str = "message";
    const THOUGHT: &str = "thought";

    #[test]
    fn chunks_of_one_message_coalesce() {
        let mut b = ChunkBuffer::default();
        assert!(b.push(MSG, Some("m1".into()), "Hel").is_none());
        assert!(b.push(MSG, Some("m1".into()), "lo ").is_none());
        assert!(b.push(MSG, Some("m1".into()), "there").is_none());
        let mut out = Vec::new();
        b.flush_all(|_, _, t| out.push(t));
        assert_eq!(out, vec!["Hello there"]);
    }

    #[test]
    fn a_new_message_id_closes_the_previous_one() {
        let mut b = ChunkBuffer::default();
        b.push(MSG, Some("m1".into()), "first");
        let finished = b.push(MSG, Some("m2".into()), "second").unwrap();
        assert_eq!(finished.2, "first");
        let mut out = Vec::new();
        b.flush_all(|_, _, t| out.push(t));
        assert_eq!(out, vec!["second"]);
    }

    #[test]
    fn thoughts_and_messages_do_not_merge() {
        let mut b = ChunkBuffer::default();
        b.push(MSG, None, "visible");
        let finished = b.push(THOUGHT, None, "internal").unwrap();
        assert_eq!(finished.0, MSG);
        assert_eq!(finished.2, "visible");
    }

    #[test]
    fn flushing_twice_emits_nothing_the_second_time() {
        let mut b = ChunkBuffer::default();
        b.push(MSG, None, "x");
        let mut count = 0;
        b.flush_all(|_, _, _| count += 1);
        b.flush_all(|_, _, _| count += 1);
        assert_eq!(count, 1);
    }
}
