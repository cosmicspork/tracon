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

/// The most bytes a chunk may hold for an endpoint that embeds at most
/// `context_tokens` tokens in one input. Bytes, because no tokenizer the
/// corpus meets makes more tokens than there are bytes (byte-level BPE and
/// SentencePiece with byte fallback alike), so a chunk under this fits
/// whatever the text is: prose, a diff, or CJK. An eighth is held back for
/// the special tokens and any instruction the server adds around the input.
/// Never below `MIN_CAP`, so a misconfigured context still makes progress.
pub fn cap(context_tokens: usize) -> usize {
    (context_tokens - context_tokens / 8).max(MIN_CAP)
}

/// Enough for any UTF-8 character, so a cut always moves forward.
const MIN_CAP: usize = 64;

/// Split `text` on blank lines when a section is too long to embed as one
/// piece, packing paragraphs into pieces of at most `target` bytes. A
/// paragraph longer than `cap` on its own goes to `split_lines`.
fn split_long(text: &str, base: usize, target: usize, cap: usize, out: &mut Vec<Chunk>) {
    let mut start = 0usize;
    let mut cursor = 0usize;
    while cursor < text.len() {
        let next = text[cursor..]
            .find("\n\n")
            .map(|n| cursor + n + 2)
            .unwrap_or(text.len());
        if next - start > target && next != start {
            // A single paragraph over the target is emitted whole while it
            // fits the cap: cutting mid-sentence produces a vector for half an
            // idea. And a cut that would leave only a heading behind is not
            // taken either — that is the same bare-heading chunk the fold
            // above exists to avoid.
            let end = if cursor - start < MIN_BODY {
                next
            } else {
                cursor
            };
            emit(&text[start..end], base + start, cap, out);
            start = end;
        }
        cursor = next;
    }
    if start < text.len() {
        emit(&text[start..], base + start, cap, out);
    }
}

/// A piece as one chunk if it fits the cap, else cut by `split_lines`.
fn emit(slice: &str, offset: usize, cap: usize, out: &mut Vec<Chunk>) {
    if slice.len() > cap {
        split_lines(slice, offset, cap, out);
    } else {
        push(slice, offset, out);
    }
}

/// Cut a paragraph too long for the endpoint into pieces of at most `cap`
/// bytes: whole lines packed up to the cap, and a line longer than the cap
/// cut at it, on a character boundary. Notes holding a raw diff have no blank
/// lines at all, and sent whole they were refused on every sweep.
///
/// The pieces tile the slice exactly and keep their text untrimmed, so
/// nothing is lost: their concatenation is the slice, leading spaces of a
/// diff's context lines included. Only a piece of nothing but whitespace is
/// left out, as `push` leaves one out.
fn split_lines(text: &str, base: usize, cap: usize, out: &mut Vec<Chunk>) {
    let mut start = 0usize;
    let mut cursor = 0usize;
    while cursor < text.len() {
        let line_end = text[cursor..]
            .find('\n')
            .map(|n| cursor + n + 1)
            .unwrap_or(text.len());
        if line_end - start <= cap {
            cursor = line_end;
            continue;
        }
        let end = if cursor > start {
            // The lines so far are a piece; this line starts the next one.
            cursor
        } else {
            // One line alone is over the cap: cut it there.
            let mut end = start + cap;
            while !text.is_char_boundary(end) {
                end -= 1;
            }
            end
        };
        push_exact(&text[start..end], base + start, out);
        start = end;
        cursor = end;
    }
    if start < text.len() {
        push_exact(&text[start..], base + start, out);
    }
}

fn push_exact(slice: &str, offset: usize, out: &mut Vec<Chunk>) {
    if slice.trim().is_empty() {
        return;
    }
    out.push(Chunk {
        text: slice.to_string(),
        offset,
        len: slice.len(),
    });
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

/// A document body as the pieces to embed, none over `cap` bytes (see `cap`).
/// An empty body yields nothing, which is why a document with no text never
/// enters the index.
pub fn document(body: &str, cap: usize) -> Vec<Chunk> {
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
    let cap = cap.max(MIN_CAP);
    let target = MAX_CHARS.min(cap);
    let mut out = Vec::new();
    for (offset, end) in sections {
        let text = &body[offset..end];
        if text.len() > target {
            split_long(text, offset, target, cap, &mut out);
        } else {
            push(text, offset, &mut out);
        }
    }
    out
}

/// A memory is atomic by design — one fact, one lesson — so it is one chunk,
/// unless it is too long for the endpoint to embed at all: then it is cut by
/// line like an oversized paragraph, since one chunk the server refuses is
/// no chunk.
pub fn memory(body: &str, cap: usize) -> Vec<Chunk> {
    if body.trim().is_empty() {
        return Vec::new();
    }
    let cap = cap.max(MIN_CAP);
    if body.len() > cap {
        let mut out = Vec::new();
        split_lines(body, 0, cap, &mut out);
        return out;
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

    /// The cap a node with the default `[embed]` config chunks to.
    const CAP: usize = 1792;

    #[test]
    fn the_default_cap_comes_from_the_default_context() {
        let ctx = crate::config::Embed::default().context_tokens;
        assert_eq!(cap(ctx), CAP);
        assert!(cap(ctx) >= MAX_CHARS, "default config must chunk as before");
        assert_eq!(cap(8192), 7168);
        assert_eq!(cap(0), MIN_CAP);
    }

    /// Pieces cut inside a paragraph must tile it: in order, no gap, no
    /// overlap, and each one's text exactly its slice of the body.
    fn tiles(body: &str, chunks: &[Chunk]) -> String {
        let mut at = chunks[0].offset;
        let mut joined = String::new();
        for c in chunks {
            assert_eq!(c.offset, at, "gap or overlap before {}", c.offset);
            assert_eq!(&body[c.offset..c.offset + c.len], c.text);
            at += c.len;
            joined.push_str(&c.text);
        }
        joined
    }

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
        let chunks = document(body, CAP);
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
        assert_eq!(document(body, CAP).len(), 1);
    }

    #[test]
    fn a_lone_heading_folds_into_what_follows_it() {
        let body = "# A\n\n## B\n\nThe only real content in this document, which is long enough to be a section on its own.\n";
        let chunks = document(body, CAP);
        assert_eq!(chunks.len(), 1);
        assert!(chunks[0].text.contains("only real content"));
    }

    #[test]
    fn a_section_past_the_cap_splits_on_paragraphs() {
        let para = "This is a paragraph of a reasonable length that says something.\n\n";
        let body = format!("# Long\n\n{}", para.repeat(40));
        let chunks = document(&body, CAP);
        assert!(
            chunks.len() > 1,
            "a {}-char section stayed whole",
            body.len()
        );
        assert!(chunks.iter().all(|c| c.text.len() <= MAX_CHARS * 2));
        offsets_are_real(&body, &chunks);
    }

    /// Cutting mid-sentence would embed half an idea, so a single paragraph
    /// over the packing target is left whole while it fits the cap.
    #[test]
    fn a_paragraph_over_the_target_but_under_the_cap_is_left_whole() {
        let body = format!("# X\n\n{}", "word ".repeat(1000));
        let chunks = document(&body, cap(8192));
        assert_eq!(chunks.len(), 1);
        offsets_are_real(&body, &chunks);
    }

    /// A note holding a raw diff has no blank line to split at. Sent whole it
    /// is refused by the endpoint on every sweep, so it is cut by line.
    #[test]
    fn a_diff_with_no_blank_lines_splits_by_line_under_the_cap() {
        let mut body = String::from("diff --git a/x.rs b/x.rs\n--- a/x.rs\n+++ b/x.rs\n");
        for i in 0..2000 {
            body.push_str(match i % 3 {
                0 => " context line that stays the same — ünïcödé\n",
                1 => "-let old = compute(input, 42);\n",
                _ => "+let new = compute(input, 43); // 変更\n",
            });
        }
        assert!(!body.contains("\n\n"));
        let chunks = document(&body, CAP);
        assert!(chunks.len() > 10, "{} chunks", chunks.len());
        assert!(chunks.iter().all(|c| c.text.len() <= CAP));
        // Line packing: every piece but the last ends at a line end.
        assert!(chunks[..chunks.len() - 1]
            .iter()
            .all(|c| c.text.ends_with('\n')));
        assert_eq!(chunks[0].offset, 0);
        assert_eq!(tiles(&body, &chunks), body);
    }

    #[test]
    fn a_line_longer_than_the_cap_is_cut_at_it() {
        // Multi-byte characters throughout, so a cut at the cap lands inside
        // one unless it is moved to a character boundary.
        let body: String = "aé漢😀".repeat(10_000);
        assert!(body.len() >= 100_000);
        let chunks = document(&body, CAP);
        assert!(chunks.len() >= body.len() / CAP);
        assert!(chunks.iter().all(|c| c.text.len() <= CAP));
        // A cut moves back at most the width of one character.
        assert!(chunks[..chunks.len() - 1]
            .iter()
            .all(|c| c.text.len() > CAP - 4));
        assert_eq!(tiles(&body, &chunks), body);

        let plain = "x".repeat(100_000);
        let chunks = document(&plain, CAP);
        assert!(chunks[..chunks.len() - 1].iter().all(|c| c.len == CAP));
        assert_eq!(tiles(&plain, &chunks), plain);
    }

    /// A long line among short ones is cut, and the short lines around it
    /// still pack into pieces of their own.
    #[test]
    fn a_long_line_inside_a_paragraph_is_cut_and_its_neighbours_kept() {
        let body = format!(
            "# Log\n\nfirst short line\n{}\nlast short line\n",
            "y".repeat(5000)
        );
        let chunks = document(&body, CAP);
        assert!(chunks.iter().all(|c| c.text.len() <= CAP));
        assert!(chunks[0].text.contains("first short line"));
        assert!(chunks.last().unwrap().text.contains("last short line"));
        assert_eq!(tiles(&body, &chunks), body);
    }

    /// The bound is what keeps normal documents' vectors: with the default
    /// cap they chunk exactly as with no cap at all, which is how they
    /// chunked before there was one.
    #[test]
    fn normal_paragraphs_chunk_exactly_as_before() {
        let para = "This is a paragraph of a reasonable length that says something.\n\n";
        let bodies = [
            format!("# Long\n\n{}", para.repeat(40)),
            format!(
                "# Mid\n\n{}\n\n## Next\n\n{}",
                "word ".repeat(300),
                para.repeat(5)
            ),
            "# Short\n\nOne paragraph that is long enough to be a section alone.\n".to_string(),
        ];
        for body in &bodies {
            assert_eq!(document(body, CAP), document(body, usize::MAX), "{body}");
        }
    }

    #[test]
    fn a_memory_too_long_to_embed_is_cut_like_a_paragraph() {
        let body = "a line of a memory\n".repeat(500);
        let chunks = memory(&body, CAP);
        assert!(chunks.len() > 1);
        assert!(chunks.iter().all(|c| c.text.len() <= CAP));
        assert_eq!(tiles(&body, &chunks), body);
        assert_eq!(memory("short", CAP).len(), 1);
    }

    #[test]
    fn a_memory_is_one_chunk_and_an_empty_body_is_none() {
        assert_eq!(memory("the test command is just test", CAP).len(), 1);
        assert!(memory("   \n ", CAP).is_empty());
        assert!(document("", CAP).is_empty());
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
        let chunks = document(&body, CAP);
        assert!(chunks.len() > 5, "{} chunks", chunks.len());
        offsets_are_real(&body, &chunks);
        assert!(chunks.iter().all(|c| !c.text.trim().is_empty()));
    }
}
