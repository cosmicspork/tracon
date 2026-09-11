use std::collections::HashSet;

use regex::RegexBuilder;
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::mcp::docs::title_of;

pub const MAX_BUNDLE_BYTES: usize = 20 * 1024 * 1024;
pub const MAX_FILE_BYTES: usize = 16 * 1024 * 1024;
pub const MAX_FILES: usize = 256;
pub const CHUNK_BYTES: usize = 256 * 1024;

const HASH_DOMAIN: &[u8] = b"tracon/html-bundle/v1";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HtmlFile {
    pub path: String,
    pub bytes: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidatedHtmlBundle {
    pub entry_path: String,
    pub entry_html: String,
    pub title: String,
    pub hash: String,
    pub files: Vec<HtmlFile>,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum HtmlBundleError {
    #[error("HTML import must contain at least one file")]
    Empty,
    #[error("HTML import contains too many files (maximum {MAX_FILES})")]
    TooManyFiles,
    #[error("bundle path is invalid: {0}")]
    InvalidPath(String),
    #[error("bundle path is duplicated: {0}")]
    DuplicatePath(String),
    #[error("file exceeds the {MAX_FILE_BYTES}-byte limit: {0}")]
    FileTooLarge(String),
    #[error("bundle exceeds the {MAX_BUNDLE_BYTES}-byte limit")]
    BundleTooLarge,
    #[error("entry HTML is missing from the bundle: {0}")]
    MissingEntry(String),
    #[error("entry file must have an .html or .htm extension: {0}")]
    EntryNotHtml(String),
    #[error("entry HTML is not valid UTF-8")]
    EntryNotUtf8,
}

pub fn validate_bundle(
    slug: &str,
    entry_path: &str,
    files: Vec<HtmlFile>,
) -> Result<ValidatedHtmlBundle, HtmlBundleError> {
    if files.is_empty() {
        return Err(HtmlBundleError::Empty);
    }
    if files.len() > MAX_FILES {
        return Err(HtmlBundleError::TooManyFiles);
    }

    let entry_path = normalize_path(entry_path)?;
    let entry_lower = entry_path.to_ascii_lowercase();
    if !entry_lower.ends_with(".html") && !entry_lower.ends_with(".htm") {
        return Err(HtmlBundleError::EntryNotHtml(entry_path));
    }

    let mut seen = HashSet::with_capacity(files.len());
    let mut total = 0usize;
    let mut normalized = Vec::with_capacity(files.len());
    for file in files {
        let path = normalize_path(&file.path)?;
        if !seen.insert(path.clone()) {
            return Err(HtmlBundleError::DuplicatePath(path));
        }
        if file.bytes.len() > MAX_FILE_BYTES {
            return Err(HtmlBundleError::FileTooLarge(path));
        }
        total = total
            .checked_add(file.bytes.len())
            .ok_or(HtmlBundleError::BundleTooLarge)?;
        if total > MAX_BUNDLE_BYTES {
            return Err(HtmlBundleError::BundleTooLarge);
        }
        normalized.push(HtmlFile {
            path,
            bytes: file.bytes,
        });
    }
    normalized.sort_by(|a, b| a.path.cmp(&b.path));

    let entry = normalized
        .iter()
        .find(|file| file.path == entry_path)
        .ok_or_else(|| HtmlBundleError::MissingEntry(entry_path.clone()))?;
    let entry_html = std::str::from_utf8(&entry.bytes)
        .map_err(|_| HtmlBundleError::EntryNotUtf8)?
        .to_string();
    let title = html_title(&entry_html).unwrap_or_else(|| title_of(slug, ""));
    let hash = generation_hash(&entry_path, &normalized);

    Ok(ValidatedHtmlBundle {
        entry_path,
        entry_html,
        title,
        hash,
        files: normalized,
    })
}

pub fn normalize_path(path: &str) -> Result<String, HtmlBundleError> {
    if path.is_empty()
        || path.starts_with('/')
        || path.starts_with('\\')
        || path.contains('\\')
        || path.contains('\0')
    {
        return Err(HtmlBundleError::InvalidPath(path.to_string()));
    }
    let segments: Vec<&str> = path.split('/').collect();
    if segments
        .iter()
        .any(|segment| segment.is_empty() || *segment == "." || *segment == "..")
    {
        return Err(HtmlBundleError::InvalidPath(path.to_string()));
    }
    Ok(segments.join("/"))
}

pub fn media_type(path: &str, is_entry: bool) -> String {
    if is_entry {
        return "text/html; charset=utf-8".to_string();
    }
    mime_guess::from_path(path)
        .first_raw()
        .unwrap_or("application/octet-stream")
        .to_string()
}

pub fn content_hash(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

fn generation_hash(entry_path: &str, files: &[HtmlFile]) -> String {
    let mut digest = Sha256::new();
    digest.update(HASH_DOMAIN);
    update_len_prefixed(&mut digest, entry_path.as_bytes());
    for file in files {
        update_len_prefixed(&mut digest, file.path.as_bytes());
        update_len_prefixed(&mut digest, &file.bytes);
    }
    hex::encode(digest.finalize())
}

fn update_len_prefixed(digest: &mut Sha256, bytes: &[u8]) {
    digest.update((bytes.len() as u64).to_be_bytes());
    digest.update(bytes);
}

fn html_title(html: &str) -> Option<String> {
    let pattern = RegexBuilder::new(r"<title(?:\s[^>]*)?>(.*?)</title\s*>")
        .case_insensitive(true)
        .dot_matches_new_line(true)
        .build()
        .expect("static title regex");
    let raw = pattern.captures(html)?.get(1)?.as_str();
    let collapsed = decode_entities(raw)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    (!collapsed.is_empty()).then_some(collapsed)
}

fn decode_entities(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(ix) = rest.find('&') {
        out.push_str(&rest[..ix]);
        rest = &rest[ix..];
        let Some(end) = rest.find(';') else {
            out.push_str(rest);
            return out;
        };
        let entity = &rest[1..end];
        let decoded = match entity {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" | "#39" => Some('\''),
            "nbsp" => Some(' '),
            value if value.starts_with("#x") || value.starts_with("#X") => {
                u32::from_str_radix(&value[2..], 16)
                    .ok()
                    .and_then(char::from_u32)
            }
            value if value.starts_with('#') => value[1..].parse().ok().and_then(char::from_u32),
            _ => None,
        };
        if let Some(ch) = decoded {
            out.push(ch);
        } else {
            out.push_str(&rest[..=end]);
        }
        rest = &rest[end + 1..];
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(path: &str, bytes: &[u8]) -> HtmlFile {
        HtmlFile {
            path: path.to_string(),
            bytes: bytes.to_vec(),
        }
    }

    #[test]
    fn validates_sorts_hashes_and_extracts_title() {
        let bundle = validate_bundle(
            "ref-fallback",
            "index.html",
            vec![
                file("style.css", b"body{}"),
                file("index.html", b"<TITLE> A &amp;  B </title>"),
            ],
        )
        .unwrap();
        assert_eq!(bundle.title, "A & B");
        assert_eq!(bundle.files[0].path, "index.html");
        assert_eq!(bundle.hash.len(), 64);

        let reordered = validate_bundle(
            "ref-fallback",
            "index.html",
            vec![
                file("index.html", b"<TITLE> A &amp;  B </title>"),
                file("style.css", b"body{}"),
            ],
        )
        .unwrap();
        assert_eq!(bundle.hash, reordered.hash);
    }

    #[test]
    fn rejects_unsafe_and_duplicate_paths() {
        for path in [
            "/index.html",
            "../index.html",
            "a/./index.html",
            "a\\index.html",
        ] {
            assert!(matches!(
                validate_bundle("ref-x", path, vec![file(path, b"x")]),
                Err(HtmlBundleError::InvalidPath(_))
            ));
        }
        assert!(matches!(
            validate_bundle(
                "ref-x",
                "index.html",
                vec![file("index.html", b"x"), file("index.html", b"y")]
            ),
            Err(HtmlBundleError::DuplicatePath(_))
        ));
    }

    #[test]
    fn requires_html_utf8_entry() {
        assert!(matches!(
            validate_bundle("ref-x", "readme.txt", vec![file("readme.txt", b"x")]),
            Err(HtmlBundleError::EntryNotHtml(_))
        ));
        assert!(matches!(
            validate_bundle("ref-x", "index.html", vec![file("index.html", &[0xff])]),
            Err(HtmlBundleError::EntryNotUtf8)
        ));
    }
}
