//! Server-sent events, decoded from bytes as they arrive.
//!
//! Adapted from the OpenCode adapter's reader, with `[DONE]` recognised: the
//! OpenAI-shaped streams end on it, and a stream that stops without it was cut.

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Frame {
    Data(String),
    Done,
}

#[derive(Debug, Default)]
pub struct Decoder {
    buffer: Vec<u8>,
    pending: Option<String>,
}

impl Decoder {
    /// Feed a chunk; returns every frame it completed. A chunk may end in the
    /// middle of a line, or of a UTF-8 sequence, and the rest waits for the next.
    pub fn push(&mut self, chunk: &[u8]) -> Vec<Frame> {
        self.buffer.extend_from_slice(chunk);
        let mut out = Vec::new();
        while let Some(at) = self.buffer.iter().position(|b| *b == b'\n') {
            let line = self.buffer.drain(..=at).collect::<Vec<_>>();
            let line = String::from_utf8_lossy(&line);
            let line = line.trim_end_matches(['\r', '\n']);
            if let Some(data) = line.strip_prefix("data:") {
                let data = data.strip_prefix(' ').unwrap_or(data);
                match &mut self.pending {
                    Some(pending) => {
                        pending.push('\n');
                        pending.push_str(data);
                    }
                    None => self.pending = Some(data.to_string()),
                }
            } else if line.is_empty() {
                if let Some(data) = self.pending.take() {
                    out.push(if data == "[DONE]" {
                        Frame::Done
                    } else {
                        Frame::Data(data)
                    });
                }
            }
            // `event:`, `id:`, `retry:` and comments carry nothing these
            // streams use.
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_split_across_chunks_and_done() {
        let mut d = Decoder::default();
        assert!(d.push(b"data: {\"a\":").is_empty());
        assert!(d.push(b"1}\r\n").is_empty());
        assert_eq!(d.push(b"\r\n"), vec![Frame::Data("{\"a\":1}".into())]);
        assert_eq!(
            d.push(b": keepalive\n\ndata: {\"b\":2}\n\ndata: [DONE]\n\n"),
            vec![Frame::Data("{\"b\":2}".into()), Frame::Done]
        );
    }

    #[test]
    fn utf8_split_mid_sequence() {
        let mut d = Decoder::default();
        let bytes = "data: \"é\"\n\n".as_bytes();
        let (a, b) = bytes.split_at(8);
        assert!(d.push(a).is_empty());
        assert_eq!(d.push(b), vec![Frame::Data("\"é\"".into())]);
    }
}
