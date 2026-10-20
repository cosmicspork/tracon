//! Hashlines: each line read is shown with its number and a short hash of its
//! content, `12:a3f9|text`, and edits name lines by that anchor.
//!
//! The hash is what makes an edit fail closed: an anchor whose line changed
//! since the model read it is refused before anything is written, rather than
//! landing on whatever now sits at that number. Four hex characters are
//! enough because the number does the addressing; the hash only has to notice
//! a change.

use sha2::{Digest, Sha256};

pub fn hash(line: &str) -> String {
    let digest = Sha256::digest(line.as_bytes());
    hex::encode(&digest[..2])
}

pub fn anchor(number: usize, line: &str) -> String {
    format!("{number}:{}", hash(line))
}

/// One line as the model sees it.
pub fn format(number: usize, line: &str) -> String {
    format!("{}|{line}", anchor(number, line))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Anchor {
    /// 1-based.
    pub line: usize,
    pub hash: String,
}

/// Parses `12:a3f9`. Lenient about what weak models copy around it: the
/// `|text` tail from a read, the `path:` head from a grep, whitespace, a
/// leading `L`. Leniency is safe because the hash is still checked.
pub fn parse(text: &str) -> Result<Anchor, String> {
    let expected = "an anchor is LINE:HASH as shown before the `|`, e.g. `12:a3f9`, with no path";
    let core = text.split('|').next().unwrap_or("").trim();
    let (rest, hash) = core
        .rsplit_once(':')
        .ok_or_else(|| format!("`{text}` is not an anchor; {expected}"))?;
    let number = rest.rsplit(':').next().unwrap_or(rest).trim();
    let number = number.strip_prefix('L').unwrap_or(number);
    let line = number
        .parse::<usize>()
        .ok()
        .filter(|n| *n > 0)
        .ok_or_else(|| format!("`{text}` is not an anchor; {expected}"))?;
    let hash = hash.trim().to_ascii_lowercase();
    if hash.len() != 4 || !hash.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(format!(
            "`{text}`: the hash must be the 4 hex characters shown; {expected}"
        ));
    }
    Ok(Anchor { line, hash })
}

/// A file split into lines, remembering how to put it back together.
#[derive(Debug, Clone)]
pub struct Text {
    pub lines: Vec<String>,
    pub crlf: bool,
    pub trailing_newline: bool,
}

impl Text {
    pub fn parse(content: &str) -> Self {
        let crlf = content.contains("\r\n");
        let trailing_newline = content.ends_with('\n');
        let lines = content.lines().map(str::to_string).collect();
        Self {
            lines,
            crlf,
            trailing_newline,
        }
    }

    pub fn render(&self) -> String {
        let eol = if self.crlf { "\r\n" } else { "\n" };
        let mut out = self.lines.join(eol);
        if self.trailing_newline && !self.lines.is_empty() {
            out.push_str(eol);
        }
        out
    }

    /// `None` when the anchor still names the line it was read from;
    /// otherwise why not, with what is there now so the model can see it.
    pub fn check(&self, anchor: &Anchor) -> Option<String> {
        match self.lines.get(anchor.line - 1) {
            None => Some(format!(
                "line {} does not exist (the file has {} lines)",
                anchor.line,
                self.lines.len()
            )),
            Some(line) if hash(line) != anchor.hash => {
                // Small models pair one line's number with a neighbour's hash.
                // Name both lines, but never act on a guess between them.
                let owners: Vec<usize> = (0..self.lines.len())
                    .filter(|i| hash(&self.lines[*i]) == anchor.hash)
                    .collect();
                Some(match owners[..] {
                    [only] => format!(
                        "`{}:{}` mixes two lines: line {} is `{}` but hash {} is `{}`; \
                         use the anchor of the line you mean",
                        anchor.line,
                        anchor.hash,
                        anchor.line,
                        format(anchor.line, line),
                        anchor.hash,
                        format(only + 1, &self.lines[only])
                    ),
                    _ => format!(
                        "line {} changed since it was read; it is now `{}`",
                        anchor.line,
                        format(anchor.line, line)
                    ),
                })
            }
            Some(_) => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_parse_round_trip() {
        let shown = format(12, "fn main() {");
        let a = parse(&shown).unwrap();
        assert_eq!(a.line, 12);
        assert_eq!(a.hash, hash("fn main() {"));
        assert_eq!(parse(&format!(" L{} ", anchor(3, "x"))).unwrap().line, 3);
        // Copied whole from a grep result.
        let from_grep = format!("src/a.py:{}", format(7, "x = 1"));
        assert_eq!(
            parse(&from_grep).unwrap(),
            parse(&anchor(7, "x = 1")).unwrap()
        );
    }

    #[test]
    fn parse_refuses_garbage() {
        assert!(parse("12").is_err());
        assert!(parse("0:abcd").is_err());
        assert!(parse("3:abc").is_err());
        assert!(parse("3:zzzz").is_err());
    }

    #[test]
    fn text_round_trips_endings() {
        for s in ["a\nb\n", "a\nb", "a\r\nb\r\n", ""] {
            assert_eq!(Text::parse(s).render(), s);
        }
    }

    #[test]
    fn check_notices_a_changed_line() {
        let t = Text::parse("one\ntwo\n");
        assert!(t.check(&parse(&anchor(2, "two")).unwrap()).is_none());
        assert!(t.check(&parse(&anchor(2, "TWO")).unwrap()).is_some());
        assert!(t.check(&parse(&anchor(9, "two")).unwrap()).is_some());
    }

    #[test]
    fn check_names_the_line_a_misplaced_hash_belongs_to() {
        let t = Text::parse("one\ntwo\nthree\n");
        let crossed = Anchor {
            line: 1,
            hash: hash("two"),
        };
        let why = t.check(&crossed).unwrap();
        assert!(why.contains("mixes two lines"), "{why}");
        assert!(why.contains(&format(2, "two")), "{why}");
    }
}
