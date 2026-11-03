//! Which files of a checkout an export keeps.
//!
//! This mirrors the node's own checkout export (`workspace::copy_checkout`,
//! and `STAGE_WORKSPACE` in the Podman backend, which is
//! `git ls-files -co --exclude-standard` plus the repository): with a `.git`
//! directory, every file the index tracks, even one an ignore rule matches,
//! the untracked files the ignore rules leave in, and `.git` itself; without
//! one, everything. The rules and the index are read as files; no Git runs,
//! so a repository's configuration cannot execute anything here. Keep the two
//! in step: a tree that exports differently from the sandbox than from a
//! volume is a review that sees different bytes.

use std::collections::HashSet;
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use super::{open_no_follow, FilesError};

/// Large enough for the index of a tree far past the file limit.
const MAX_INDEX_BYTES: u64 = 64 * 1024 * 1024;

/// The paths under `root` (relative to it) an export keeps, or `None` when
/// `root` is not a repository and everything is kept. Ancestors of every kept
/// path are included; `.git` and what is under it are not listed, since the
/// caller keeps the repository whole.
pub fn kept_paths(root: &Path) -> Result<Option<HashSet<PathBuf>>, FilesError> {
    let repository = root.join(".git");
    if !std::fs::symlink_metadata(&repository).is_ok_and(|m| m.is_dir()) {
        return Ok(None);
    }
    let real_root = std::fs::canonicalize(root)?;
    if !ignore_rules_inside(&real_root, root) {
        return Err(FilesError::Unsafe(root.into()));
    }
    let outside = Arc::new(Mutex::new(None::<PathBuf>));
    let seen = outside.clone();
    let mut keep = HashSet::new();
    let walk = ignore::WalkBuilder::new(root)
        .standard_filters(false)
        .git_ignore(true)
        .git_exclude(true)
        .require_git(false)
        .follow_links(false)
        .filter_entry(move |entry| {
            if entry.file_name().eq_ignore_ascii_case(".git") {
                return false;
            }
            let dir = entry.file_type().is_some_and(|kind| kind.is_dir());
            if dir && !ignore_rules_inside(&real_root, entry.path()) {
                *seen.lock().unwrap() = Some(entry.path().to_path_buf());
                return false;
            }
            true
        })
        .build();
    for entry in walk {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => match error.io_error() {
                Some(io) if io.kind() == std::io::ErrorKind::NotFound => continue,
                _ => return Err(FilesError::Unsafe(root.into())),
            },
        };
        if let Ok(relative) = entry.path().strip_prefix(root) {
            if !relative.as_os_str().is_empty() {
                keep.insert(relative.to_path_buf());
            }
        }
    }
    if let Some(dir) = outside.lock().unwrap().take() {
        return Err(FilesError::Unsafe(dir));
    }
    for tracked in tracked_paths(&repository)? {
        let mut path = tracked.as_path();
        while !path.as_os_str().is_empty() && keep.insert(path.to_path_buf()) {
            path = path.parent().unwrap_or(Path::new(""));
        }
    }
    Ok(Some(keep))
}

/// Whether the ignore rules a walk reads in `dir` (its `.gitignore`, and
/// `.git/info/exclude`) are, where a link leads to them, files inside
/// `real_root`, the tree's canonical path. The walk opens them by path and
/// so follows links, and a tree may hold links that leave it: a prepared
/// tree's, or on the node an absolute one, which names the node's own
/// filesystem there.
pub fn ignore_rules_inside(real_root: &Path, dir: &Path) -> bool {
    [&[".gitignore"][..], &[".git", "info", "exclude"]]
        .iter()
        .all(|names| {
            let mut path = dir.to_path_buf();
            let mut linked = false;
            for name in *names {
                path.push(name);
                match std::fs::symlink_metadata(&path) {
                    Ok(meta) => linked |= meta.file_type().is_symlink(),
                    Err(_) => break,
                }
            }
            if !linked {
                return true;
            }
            // Resolved without opening anything, so a link to a device or a
            // FIFO is never read.
            match std::fs::canonicalize(&path) {
                Ok(real) => {
                    real.starts_with(real_root)
                        && std::fs::metadata(&real).is_ok_and(|meta| meta.is_file())
                }
                // Nothing there for the walk to read either.
                Err(_) => true,
            }
        })
}

fn read_git_file(path: &Path) -> Result<Option<Vec<u8>>, FilesError> {
    let file = match open_no_follow(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(FilesError::Unsafe(path.into())),
    };
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.len() > MAX_INDEX_BYTES {
        return Err(FilesError::Unsafe(path.into()));
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.take(MAX_INDEX_BYTES).read_to_end(&mut bytes)?;
    Ok(Some(bytes))
}

/// The paths a repository's index tracks, parsed from `index` (and the shared
/// index a split index names). A missing index tracks nothing; one that
/// cannot be parsed refuses the export, as `git ls-files` would fail on it.
fn tracked_paths(git_dir: &Path) -> Result<Vec<PathBuf>, FilesError> {
    let refused = |name: &str| FilesError::Index(format!("{name} is not an index this can read"));
    let Some(index) = read_git_file(&git_dir.join("index"))? else {
        return Ok(Vec::new());
    };
    let config = read_git_file(&git_dir.join("config")).ok().flatten();
    let sha256 = config.is_some_and(|config| {
        let config: Vec<u8> = config
            .iter()
            .filter(|byte| !byte.is_ascii_whitespace())
            .map(u8::to_ascii_lowercase)
            .collect();
        config
            .windows(b"objectformat=sha256".len())
            .any(|window| window == b"objectformat=sha256")
    });
    let hash_len = if sha256 { 32 } else { 20 };
    let mut paths = Vec::new();
    let shared = parse_index(&index, hash_len, &mut paths).ok_or_else(|| refused("index"))?;
    if let Some(shared) = shared {
        let name = format!("sharedindex.{shared}");
        let bytes = read_git_file(&git_dir.join(&name))?.ok_or_else(|| refused(&name))?;
        parse_index(&bytes, hash_len, &mut paths).ok_or_else(|| refused(&name))?;
    }
    Ok(paths)
}

/// Git's index format, versions 2 to 4, for the entry names alone. Returns
/// the hex name of the shared index a split index points at, if any.
fn parse_index(bytes: &[u8], hash_len: usize, out: &mut Vec<PathBuf>) -> Option<Option<String>> {
    let word = |at: usize| -> Option<u32> {
        Some(u32::from_be_bytes(bytes.get(at..at + 4)?.try_into().ok()?))
    };
    if bytes.get(..4)? != b"DIRC" {
        return None;
    }
    let version = word(4)?;
    if !(2..=4).contains(&version) {
        return None;
    }
    let count = word(8)? as usize;
    let mut at = 12;
    let mut previous = Vec::new();
    for _ in 0..count {
        let start = at;
        // ctime, mtime, dev, ino, mode, uid, gid and size, then the object id.
        let flags_at = start + 40 + hash_len;
        let flags = u16::from_be_bytes(bytes.get(flags_at..flags_at + 2)?.try_into().ok()?);
        let mut name_at = flags_at + 2;
        if flags & 0x4000 != 0 {
            if version < 3 {
                return None;
            }
            name_at += 2;
        }
        let name = if version == 4 {
            // The name drops a varint count of bytes from the end of the
            // previous one and appends a NUL-terminated suffix; no padding.
            let mut strip = 0usize;
            loop {
                let byte = *bytes.get(name_at)?;
                name_at += 1;
                strip = strip
                    .checked_mul(128)?
                    .checked_add(usize::from(byte & 0x7f))?;
                if byte & 0x80 == 0 {
                    break;
                }
                strip = strip.checked_add(1)?;
            }
            let end = name_at + bytes.get(name_at..)?.iter().position(|b| *b == 0)?;
            let mut name: Vec<u8> = previous.get(..previous.len().checked_sub(strip)?)?.to_vec();
            name.extend_from_slice(&bytes[name_at..end]);
            at = end + 1;
            name
        } else {
            let end = name_at + bytes.get(name_at..)?.iter().position(|b| *b == 0)?;
            // NUL padding to a multiple of eight, at least one byte.
            at = start + ((end - start + 8) & !7);
            bytes[name_at..end].to_vec()
        };
        // A split index stores an entry that replaces a shared one without
        // its name; the shared index names it.
        if !name.is_empty() {
            out.push(index_path(&name));
        }
        previous = name;
    }
    let mut shared = None;
    let body = bytes.len().checked_sub(hash_len)?;
    while at + 8 <= body {
        let size = word(at + 4)? as usize;
        let data = bytes.get(at + 8..(at + 8).checked_add(size)?)?;
        if &bytes[at..at + 4] == b"link" {
            let oid = data.get(..hash_len)?;
            if oid.iter().any(|byte| *byte != 0) {
                shared = Some(oid.iter().map(|byte| format!("{byte:02x}")).collect());
            }
        }
        at += 8 + size;
    }
    Some(shared)
}

fn index_path(name: &[u8]) -> PathBuf {
    use std::os::unix::ffi::OsStrExt as _;
    // A sparse index names a directory it does not expand with a trailing `/`.
    let name = name.strip_suffix(b"/").unwrap_or(name);
    PathBuf::from(std::ffi::OsStr::from_bytes(name))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_plain_directory_keeps_everything() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.log"), "x").unwrap();
        std::fs::write(dir.path().join(".gitignore"), "*.log\n").unwrap();
        assert_eq!(kept_paths(dir.path()).unwrap(), None);
    }

    #[test]
    fn a_garbled_index_refuses() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join(".git")).unwrap();
        std::fs::write(dir.path().join(".git/index"), b"DIRC\0\0\0\x09").unwrap();
        assert!(matches!(kept_paths(dir.path()), Err(FilesError::Index(_))));
    }
}
