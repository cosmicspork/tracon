//! Splitting a document into the pieces that get embedded.
//!
//! A memory is one fact and embeds whole. A document is long, and a single
//! vector for a whole guide says only "this is about the workspace" — the
//! paragraph that actually answers the question is averaged away. So documents
//! split, and each piece carries where it came from so a hit can show the text
//! it matched rather than the first line of the file.
//!
//! The split follows markdown headings, because a heading is the author's own
//! statement about where a topic starts, and falls back to paragraphs when a
//! section is longer than a model's context is worth spending.

/// One piece of a body, and where it sits in it.
#[derive(Debug, Clone, PartialEq)]
pub struct Chunk {
    pub text: String,
    pub offset: usize,
    pub len: usize,
}

/// Roughly 400 tokens of English. Small enough that a chunk is about one
/// thing, large enough that it still has the context to mean something.
const MAX_CHARS: usize = 1600;
/// A section with less than this much text under its heading is folded into
/// the section that follows: a heading with nothing beneath it is not a
/// retrievable idea, and embedding it alone puts noise in the index.
const MIN_BODY: usize = 40;

/// Byte offset of every markdown heading ("# " at the start of a line, in the
/// ATX style the corpus uses), plus the end of the body.
fn heading_offsets(body: &str) -> Vec<usize> {
    let mut cuts = vec![0usize];
    let bytes = body.as_bytes();
    let mut i = 0;
    while i < body.len() {
        let line_end = body[i..].find('\n').map(|n| i + n).unwrap_or(body.len());
        if bytes[i] == b'#' && i != 0 {
            // A heading is `#`s then a space; `#hashtag` is not one.
            let hashes = body[i..line_end].bytes().take_while(|c| *c == b'#').count();
            if hashes <= 6 && body[i + hashes..].starts_with(' ') {
                cuts.push(i);
            }
        }
        i = line_end + 1;
    }
    cuts.push(body.len());
    cuts.dedup();
    cuts
}

/// The text of a section with its own heading line removed, which is what
/// decides whether the section says anything.
fn under_heading(section: &str) -> &str {
    let rest = match section.strip_prefix('#') {
        Some(_) => section.find('\n').map(|n| &section[n + 1..]).unwrap_or(""),
        None => section,
    };
    rest.trim()
}

/// Split `text` on blank lines when a section is too long to embed as one
/// piece, keeping every piece under `MAX_CHARS` where the paragraphs allow.
fn split_long(text: &str, base: usize, out: &mut Vec<Chunk>) {
    let mut start = 0usize;
    let mut cursor = 0usize;
    while cursor < text.len() {
        let next = text[cursor..]
            .find("\n\n")
            .map(|n| cursor + n + 2)
            .unwrap_or(text.len());
        if next - start > MAX_CHARS && next != start {
            // A single paragraph over the cap is still emitted whole: cutting
            // mid-sentence produces a vector for half an idea. And a cut that
            // would leave only a heading behind is not taken either — that is
            // the same bare-heading chunk the fold above exists to avoid.
            let end = if cursor - start < MIN_BODY {
                next
            } else {
                cursor
            };
            push(&text[start..end], base + start, out);
            start = end;
        }
        cursor = next;
    }
    if start < text.len() {
        push(&text[start..], base + start, out);
    }
}

fn push(slice: &str, offset: usize, out: &mut Vec<Chunk>) {
    let trimmed = slice.trim();
    if trimmed.is_empty() {
        return;
    }
    // Offsets stay those of the untrimmed slice: they index the stored body,
    // which is what a snippet is read back out of.
    out.push(Chunk {
        text: trimmed.to_string(),
        offset,
        len: slice.len(),
    });
}

/// A document body as the pieces to embed. An empty body yields nothing, which
/// is why a document with no text never enters the index.
pub fn document(body: &str) -> Vec<Chunk> {
    if body.trim().is_empty() {
        return Vec::new();
    }
    let cuts = heading_offsets(body);
    // Fold *forward*: a bare heading joins the section under it, so `# Title`
    // above `## Testing` does not become a chunk that says only "Title".
    let mut sections: Vec<(usize, usize)> = Vec::new();
    let mut pending: Option<usize> = None;
    for w in cuts.windows(2) {
        let (a, b) = (w[0], w[1]);
        let start = pending.unwrap_or(a);
        if under_heading(&body[a..b]).len() < MIN_BODY && b < body.len() {
            pending = Some(start);
            continue;
        }
        sections.push((start, b));
        pending = None;
    }
    if let Some(start) = pending {
        sections.push((start, body.len()));
    }
    let mut out = Vec::new();
    for (offset, end) in sections {
        let text = &body[offset..end];
        if text.len() > MAX_CHARS {
            split_long(text, offset, &mut out);
        } else {
            push(text, offset, &mut out);
        }
    }
    out
}

/// A memory is atomic by design — one fact, one lesson — so it is one chunk.
pub fn memory(body: &str) -> Vec<Chunk> {
    if body.trim().is_empty() {
        return Vec::new();
    }
    vec![Chunk {
        text: body.trim().to_string(),
        offset: 0,
        len: body.len(),
    }]
}

/// The hash recorded with a vector, so an edit re-embeds exactly the chunks
/// whose text changed and leaves the rest alone.
pub fn hash(text: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(text.as_bytes());
    hex::encode(h.finalize())[..16].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every chunk must be readable back out of the body it came from, or a
    /// search hit renders the wrong text.
    fn offsets_are_real(body: &str, chunks: &[Chunk]) {
        for c in chunks {
            let slice = &body[c.offset..c.offset + c.len];
            assert!(
                slice.contains(c.text.trim()),
                "chunk at {} is not its own text:\n  slice: {:?}\n  text:  {:?}",
                c.offset,
                slice,
                c.text
            );
        }
    }

    #[test]
    fn a_document_splits_at_its_headings() {
        let body = "# Title\n\nIntro paragraph that is long enough to stand on its own as a section of prose.\n\n\
                    ## Testing\n\nRun `just test`. It is the command for the whole workspace and nothing else.\n\n\
                    ## Deploying\n\nMerging to main deploys. That is the whole of it, and it is deliberate.\n";
        let chunks = document(body);
        assert_eq!(chunks.len(), 3, "{chunks:#?}");
        assert!(chunks[1].text.contains("just test"));
        offsets_are_real(body, &chunks);
    }

    /// A `#hashtag` at the start of a line is not a heading, and cutting there
    /// would split a paragraph in half.
    #[test]
    fn a_hash_without_a_space_is_not_a_heading() {
        let body =
            "# Real\n\nSome text here that runs on for a while so the section stands alone.\n\
                    #nothashtag still the same paragraph\n";
        assert_eq!(document(body).len(), 1);
    }

    #[test]
    fn a_lone_heading_folds_into_what_follows_it() {
        let body = "# A\n\n## B\n\nThe only real content in this document, which is long enough to be a section on its own.\n";
        let chunks = document(body);
        assert_eq!(chunks.len(), 1);
        assert!(chunks[0].text.contains("only real content"));
    }

    #[test]
    fn a_section_past_the_cap_splits_on_paragraphs() {
        let para = "This is a paragraph of a reasonable length that says something.\n\n";
        let body = format!("# Long\n\n{}", para.repeat(40));
        let chunks = document(&body);
        assert!(
            chunks.len() > 1,
            "a {}-char section stayed whole",
            body.len()
        );
        assert!(chunks.iter().all(|c| c.text.len() <= MAX_CHARS * 2));
        offsets_are_real(&body, &chunks);
    }

    /// Cutting mid-sentence would embed half an idea, so an oversized single
    /// paragraph is left whole even though it is over the cap.
    #[test]
    fn one_enormous_paragraph_is_left_whole() {
        let body = format!("# X\n\n{}", "word ".repeat(1000));
        let chunks = document(&body);
        assert_eq!(chunks.len(), 1);
        offsets_are_real(&body, &chunks);
    }

    #[test]
    fn a_memory_is_one_chunk_and_an_empty_body_is_none() {
        assert_eq!(memory("the test command is just test").len(), 1);
        assert!(memory("   \n ").is_empty());
        assert!(document("").is_empty());
    }

    #[test]
    fn the_hash_follows_the_text() {
        assert_eq!(hash("a"), hash("a"));
        assert_ne!(hash("a"), hash("b"));
    }

    /// Real content from this repo's own corpus, so the shape is checked
    /// against a document somebody actually wrote.
    #[test]
    fn a_realistic_guide_chunks_into_findable_pieces() {
        let body = std::fs::read_to_string("../README.md").unwrap_or_default();
        if body.is_empty() {
            return;
        }
        let chunks = document(&body);
        assert!(chunks.len() > 5, "{} chunks", chunks.len());
        offsets_are_real(&body, &chunks);
        assert!(chunks.iter().all(|c| !c.text.trim().is_empty()));
    }
}
