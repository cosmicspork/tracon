//! The artefact stash: bytes a session keeps per repository because they are
//! expensive to rebuild, such as "before" screenshots of the default branch.
//!
//! An entry is found by a name the agent picks and a digest of the inputs it
//! names (files and directories of its workspace, or Git objects such as
//! `origin/main:spa`), which the node computes itself from its own snapshot of
//! the workspace. A changed input changes the digest, so it misses: a stale
//! entry is never handed back, it only ages out.
//!
//! **Sessions only.** The stash lives under the node's state directory
//! ([`DIR`]) and is never mounted into a run. The `stash_put` and `stash_get`
//! tools are the only readers, and they answer a session the node runs: a
//! check, a preparation or a probe has no tools to call, so nothing an agent
//! put here can reach a check.
//!
//! **Bytes, not knowledge.** What an agent learned belongs in a memory, where
//! the operator can read and amend it; the stash holds opaque files.
//!
//! **Bounded.** Each repository of each channel has [`LIMIT_BYTES`]; a put
//! that takes it past the limit evicts the least recently used entries (a
//! `stash_get` hit counts as a use). Usage is shown in the data inventory.
//!
//! Layout: `stash/<scope>/<entry>/{meta.json,data/<path>}`, where the scope is
//! a digest of the channel and repository and the entry one of the name and
//! the input digest. `data` holds the stashed path at its place relative to
//! the repository root, so restoring it is unpacking `data` over the
//! workspace.

use std::io::Read as _;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use crate::config::Config;

/// The state directory the stash lives in.
pub const DIR: &str = "stash";
/// What one repository's entries may take, together.
pub const LIMIT_BYTES: u64 = 1 << 30;
/// How many files one entry may hold.
pub const MAX_FILES: usize = 20_000;
/// How many files the inputs of one key may name, together.
pub const MAX_INPUT_FILES: usize = 200_000;

const META: &str = "meta.json";
const DATA: &str = "data";
const SCOPE: &str = "scope.json";

#[derive(Debug, thiserror::Error)]
pub enum StashError {
    #[error("{0}")]
    Refused(String),
    #[error("stash storage: {0}")]
    Io(#[from] std::io::Error),
    #[error("stash entry: {0}")]
    Json(#[from] serde_json::Error),
}

/// Whose entries: one repository on one channel.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Scope {
    pub channel: String,
    pub repo: String,
}

impl Scope {
    fn id(&self) -> String {
        short(&[self.channel.as_bytes(), self.repo.as_bytes()])
    }
}

/// What an entry was stashed as.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Meta {
    pub key: String,
    pub digest: String,
    pub inputs: Vec<String>,
    /// The stashed path, relative to the repository root.
    pub path: String,
    pub bytes: u64,
    pub files: u64,
    pub created_ms: i64,
    pub used_ms: i64,
    #[serde(default)]
    pub session_id: Option<String>,
}

/// An input as the node read it: a workspace path, or a Git object the
/// agent named as `<revision>:<path>` and the node resolved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Input {
    Path(String),
    Object { named: String, object: String },
}

impl Input {
    fn named(&self) -> &str {
        match self {
            Self::Path(path) => path,
            Self::Object { named, .. } => named,
        }
    }
}

/// What a put kept and what it pushed out.
#[derive(Debug, Clone, Serialize)]
pub struct Put {
    pub meta: Meta,
    pub skipped: Vec<String>,
    pub evicted: Vec<String>,
    pub used_bytes: u64,
}

/// A hit: the entry, and the tree to unpack over the workspace.
#[derive(Debug, Clone)]
pub struct Hit {
    pub meta: Meta,
    pub data: PathBuf,
}

/// A miss, and how many entries the key has at other inputs: those are
/// stale for what the workspace holds now.
#[derive(Debug, Clone)]
pub struct Miss {
    pub other_inputs: usize,
}

/// One repository's usage, for the data inventory.
#[derive(Debug, Clone, Serialize)]
pub struct Usage {
    pub entries: u64,
    pub bytes: u64,
}

pub struct Stash {
    root: PathBuf,
    limit: u64,
}

impl Stash {
    pub fn node() -> Self {
        Self::at(Config::state_dir().join(DIR))
    }

    pub fn at(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            limit: LIMIT_BYTES,
        }
    }

    /// The same stash with another per-repository limit, for tests.
    #[cfg(test)]
    fn with_limit(mut self, limit: u64) -> Self {
        self.limit = limit;
        self
    }

    fn scope_dir(&self, scope: &Scope) -> PathBuf {
        self.root.join(scope.id())
    }

    /// Keep `path` of the tree at `source` under `key` at `digest`, replacing
    /// the entry with that name and digest if there is one, then evict the
    /// least recently used others past the limit.
    #[allow(clippy::too_many_arguments)]
    pub fn put(
        &self,
        scope: &Scope,
        key: &str,
        digest: &str,
        inputs: &[Input],
        source: &Path,
        path: &str,
        session_id: Option<&str>,
        now_ms: i64,
    ) -> Result<Put, StashError> {
        let dir = self.scope_dir(scope);
        std::fs::create_dir_all(&dir)?;
        let scope_file = dir.join(SCOPE);
        if !scope_file.exists() {
            write_atomic(&scope_file, &serde_json::to_vec_pretty(scope)?)?;
        }
        let staging = dir.join(format!(".staging-{}", uuid::Uuid::now_v7()));
        let result = (|| {
            let data = staging.join(DATA);
            std::fs::create_dir_all(&data)?;
            let copied = copy_path(source, path, &data, self.limit)?;
            let meta = Meta {
                key: key.to_string(),
                digest: digest.to_string(),
                inputs: inputs.iter().map(|i| i.named().to_string()).collect(),
                path: path.to_string(),
                bytes: copied.bytes,
                files: copied.files,
                created_ms: now_ms,
                used_ms: now_ms,
                session_id: session_id.map(str::to_string),
            };
            std::fs::write(staging.join(META), serde_json::to_vec_pretty(&meta)?)?;
            let entry = dir.join(entry_id(key, digest));
            if entry.exists() {
                std::fs::remove_dir_all(&entry)?;
            }
            std::fs::rename(&staging, &entry)?;
            Ok::<_, StashError>((meta, copied.skipped))
        })();
        if result.is_err() {
            let _ = std::fs::remove_dir_all(&staging);
        }
        let (meta, skipped) = result?;
        let (evicted, used_bytes) = self.evict(scope, Some(&entry_id(key, digest)))?;
        Ok(Put {
            meta,
            skipped,
            evicted,
            used_bytes,
        })
    }

    /// The entry with this name at this digest, marked used now.
    pub fn get(
        &self,
        scope: &Scope,
        key: &str,
        digest: &str,
        now_ms: i64,
    ) -> Result<Result<Hit, Miss>, StashError> {
        let dir = self.scope_dir(scope);
        let entry = dir.join(entry_id(key, digest));
        match read_meta(&entry) {
            Some(mut meta) if meta.key == key && meta.digest == digest => {
                meta.used_ms = now_ms.max(meta.used_ms);
                write_atomic(&entry.join(META), &serde_json::to_vec_pretty(&meta)?)?;
                Ok(Ok(Hit {
                    meta,
                    data: entry.join(DATA),
                }))
            }
            _ => {
                let other_inputs = self
                    .entries(scope)?
                    .into_iter()
                    .filter(|(_, meta)| meta.key == key)
                    .count();
                Ok(Err(Miss { other_inputs }))
            }
        }
    }

    /// Every entry of a scope, with its directory.
    fn entries(&self, scope: &Scope) -> Result<Vec<(PathBuf, Meta)>, StashError> {
        let dir = self.scope_dir(scope);
        let Ok(read) = std::fs::read_dir(&dir) else {
            return Ok(Vec::new());
        };
        let mut out = Vec::new();
        for entry in read.flatten() {
            let name = entry.file_name();
            if name.to_string_lossy().starts_with('.') || !entry.path().is_dir() {
                continue;
            }
            if let Some(meta) = read_meta(&entry.path()) {
                out.push((entry.path(), meta));
            } else {
                // A directory with no readable account is nothing a get can
                // hit; it only takes room.
                let _ = std::fs::remove_dir_all(entry.path());
            }
        }
        Ok(out)
    }

    /// Remove the least recently used entries until the scope is within the
    /// limit, never `keep`. Answers the keys removed and what is left.
    fn evict(&self, scope: &Scope, keep: Option<&str>) -> Result<(Vec<String>, u64), StashError> {
        let mut entries = self.entries(scope)?;
        let mut total: u64 = entries.iter().map(|(_, meta)| meta.bytes).sum();
        entries.sort_by_key(|(_, meta)| (meta.used_ms, meta.created_ms));
        let mut evicted = Vec::new();
        for (dir, meta) in entries {
            if total <= self.limit {
                break;
            }
            if keep.is_some_and(|keep| dir.file_name().is_some_and(|name| name == keep)) {
                continue;
            }
            std::fs::remove_dir_all(&dir)?;
            total = total.saturating_sub(meta.bytes);
            evicted.push(meta.key);
        }
        Ok((evicted, total))
    }

    /// What a scope's entries take.
    pub fn usage(&self, scope: &Scope) -> Result<Usage, StashError> {
        let entries = self.entries(scope)?;
        Ok(Usage {
            entries: entries.len() as u64,
            bytes: entries.iter().map(|(_, meta)| meta.bytes).sum(),
        })
    }
}

/// Whether a name is one an agent may stash under.
pub fn key_refusal(key: &str) -> Option<String> {
    if key.trim().is_empty() {
        return Some("key is required: the name this entry is kept under".into());
    }
    if key.chars().count() > 128 {
        return Some("key is at most 128 characters".into());
    }
    if key.chars().any(char::is_control) {
        return Some("key holds no control characters".into());
    }
    None
}

/// The digest of a key's inputs, read from the tree at `root`: each input's
/// name and, for a path, every file, directory and link under it (Git's own
/// directory left out); for a Git object, its id, which is already a digest
/// of what it holds. Order and repetition in the list do not matter.
pub fn digest(root: &Path, inputs: &[Input]) -> Result<String, StashError> {
    let mut sorted: Vec<&Input> = inputs.iter().collect();
    sorted.sort_by(|a, b| a.named().cmp(b.named()));
    sorted.dedup_by(|a, b| a.named() == b.named());
    let mut hasher = Sha256::new();
    let mut files = 0usize;
    for input in sorted {
        hasher.update(b"input\0");
        hasher.update(input.named().as_bytes());
        hasher.update(b"\0");
        match input {
            Input::Object { object, .. } => {
                hasher.update(b"object\0");
                hasher.update(object.as_bytes());
                hasher.update(b"\0");
            }
            Input::Path(path) => {
                walk(root, path, &mut |relative, node| {
                    files += 1;
                    if files > MAX_INPUT_FILES {
                        return Err(StashError::Refused(format!(
                            "the inputs hold more than {MAX_INPUT_FILES} files; name the \
                             directories the artefact is built from, not the whole tree"
                        )));
                    }
                    hasher.update(relative.as_bytes());
                    hasher.update(b"\0");
                    match node {
                        Node::Dir => hasher.update(b"d\0"),
                        Node::Link(target) => {
                            hasher.update(b"l\0");
                            hasher.update(target.as_os_str().as_encoded_bytes());
                            hasher.update(b"\0");
                        }
                        Node::File { at, len, exec } => {
                            hasher.update(if exec { b"x\0" } else { b"f\0" });
                            hasher.update(len.to_le_bytes());
                            let mut file = std::fs::File::open(at)?;
                            let mut buf = vec![0u8; 64 * 1024];
                            loop {
                                let n = file.read(&mut buf)?;
                                if n == 0 {
                                    break;
                                }
                                hasher.update(&buf[..n]);
                            }
                        }
                    }
                    Ok(())
                })?;
            }
        }
    }
    Ok(hex::encode(hasher.finalize()))
}

/// What one tree walk meets.
enum Node<'a> {
    Dir,
    Link(PathBuf),
    File { at: &'a Path, len: u64, exec: bool },
}

/// Visit `relative` under `root` and everything below it, in name order. Git's
/// own directory is passed over; `relative` itself must not be, or pass
/// through, a link.
fn walk(
    root: &Path,
    relative: &str,
    visit: &mut dyn FnMut(&str, Node<'_>) -> Result<(), StashError>,
) -> Result<(), StashError> {
    use std::os::unix::fs::PermissionsExt as _;
    let mut at = root.to_path_buf();
    for segment in relative.split('/') {
        at.push(segment);
        let meta = std::fs::symlink_metadata(&at)
            .map_err(|_| StashError::Refused(format!("{relative} is not in the workspace")))?;
        if meta.file_type().is_symlink() {
            return Err(StashError::Refused(format!(
                "{relative} is, or passes through, a symbolic link; name the files themselves"
            )));
        }
    }
    let mut stack = vec![(at, relative.to_string())];
    while let Some((path, shown)) = stack.pop() {
        let meta = std::fs::symlink_metadata(&path)?;
        let kind = meta.file_type();
        if kind.is_symlink() {
            visit(&shown, Node::Link(std::fs::read_link(&path)?))?;
        } else if kind.is_file() {
            let exec = meta.permissions().mode() & 0o111 != 0;
            visit(
                &shown,
                Node::File {
                    at: &path,
                    len: meta.len(),
                    exec,
                },
            )?;
        } else if kind.is_dir() {
            visit(&shown, Node::Dir)?;
            let mut names: Vec<_> = std::fs::read_dir(&path)?
                .collect::<Result<Vec<_>, _>>()?
                .into_iter()
                .filter_map(|e| e.file_name().to_str().map(str::to_string))
                .filter(|name| name != ".git")
                .collect();
            // Popped in reverse, so pushed in reverse to visit in order.
            names.sort();
            for name in names.into_iter().rev() {
                stack.push((path.join(&name), format!("{shown}/{name}")));
            }
        }
    }
    Ok(())
}

struct Copied {
    bytes: u64,
    files: u64,
    skipped: Vec<String>,
}

/// Copy `relative` of `source` to the same place under `dest`: regular files
/// and directories only, within the limits. Links are left out and named.
fn copy_path(source: &Path, relative: &str, dest: &Path, limit: u64) -> Result<Copied, StashError> {
    use std::os::unix::fs::PermissionsExt as _;
    let mut copied = Copied {
        bytes: 0,
        files: 0,
        skipped: Vec::new(),
    };
    walk(source, relative, &mut |shown, node| {
        let target = dest.join(shown);
        match node {
            Node::Dir => std::fs::create_dir_all(&target)?,
            Node::Link(_) => copied.skipped.push(shown.to_string()),
            Node::File { at, len, exec } => {
                copied.files += 1;
                copied.bytes = copied.bytes.saturating_add(len);
                if copied.files > MAX_FILES as u64 {
                    return Err(StashError::Refused(format!(
                        "{relative} holds more than {MAX_FILES} files; stash fewer"
                    )));
                }
                if copied.bytes > limit {
                    return Err(StashError::Refused(format!(
                        "{relative} is over the stash's {limit}-byte limit for a repository"
                    )));
                }
                if let Some(parent) = target.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                std::fs::copy(at, &target)?;
                let mode = if exec { 0o755 } else { 0o644 };
                std::fs::set_permissions(&target, std::fs::Permissions::from_mode(mode))?;
            }
        }
        Ok(())
    })?;
    if copied.files == 0 {
        return Err(StashError::Refused(format!(
            "{relative} holds no files to stash"
        )));
    }
    Ok(copied)
}

fn entry_id(key: &str, digest: &str) -> String {
    short(&[key.as_bytes(), digest.as_bytes()])
}

fn short(parts: &[&[u8]]) -> String {
    let mut hasher = Sha256::new();
    for part in parts {
        hasher.update((part.len() as u64).to_le_bytes());
        hasher.update(part);
    }
    hex::encode(&hasher.finalize()[..16])
}

fn read_meta(entry: &Path) -> Option<Meta> {
    let bytes = std::fs::read(entry.join(META)).ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), StashError> {
    let tmp = path.with_extension(format!("tmp-{}", uuid::Uuid::now_v7()));
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scope() -> Scope {
        Scope {
            channel: "work".into(),
            repo: "/repos/orbit".into(),
        }
    }

    fn tree() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("spa/src")).unwrap();
        std::fs::write(dir.path().join("spa/src/app.ts"), "one").unwrap();
        std::fs::create_dir_all(dir.path().join("shots")).unwrap();
        std::fs::write(dir.path().join("shots/home.png"), [7u8; 32]).unwrap();
        std::fs::create_dir_all(dir.path().join(".git")).unwrap();
        std::fs::write(dir.path().join(".git/HEAD"), "ref").unwrap();
        dir
    }

    fn path(p: &str) -> Input {
        Input::Path(p.into())
    }

    #[test]
    fn a_changed_input_misses_rather_than_returning_stale_bytes() {
        let ws = tree();
        let store = tempfile::tempdir().unwrap();
        let stash = Stash::at(store.path());
        let inputs = [path("spa/src")];
        let before = digest(ws.path(), &inputs).unwrap();
        stash
            .put(
                &scope(),
                "before",
                &before,
                &inputs,
                ws.path(),
                "shots",
                None,
                1,
            )
            .unwrap();
        let hit = stash.get(&scope(), "before", &before, 2).unwrap().unwrap();
        assert_eq!(hit.meta.path, "shots");
        assert_eq!(
            std::fs::read(hit.data.join("shots/home.png")).unwrap(),
            [7u8; 32]
        );
        assert_eq!(hit.meta.used_ms, 2);

        std::fs::write(ws.path().join("spa/src/app.ts"), "two").unwrap();
        let after = digest(ws.path(), &inputs).unwrap();
        assert_ne!(before, after);
        let miss = stash
            .get(&scope(), "before", &after, 3)
            .unwrap()
            .unwrap_err();
        assert_eq!(miss.other_inputs, 1, "the old entry is counted as stale");
        // Another channel on the same repository sees nothing.
        let other = Scope {
            channel: "personal".into(),
            ..scope()
        };
        assert!(stash.get(&other, "before", &before, 4).unwrap().is_err());
    }

    #[test]
    fn the_digest_ignores_order_and_git_and_sees_names_modes_and_objects() {
        let ws = tree();
        let a = digest(ws.path(), &[path("spa"), path("shots")]).unwrap();
        let b = digest(ws.path(), &[path("shots"), path("spa"), path("spa")]).unwrap();
        assert_eq!(a, b);
        std::fs::write(ws.path().join(".git/HEAD"), "other").unwrap();
        assert_eq!(digest(ws.path(), &[path("spa"), path("shots")]).unwrap(), a);
        std::fs::rename(
            ws.path().join("spa/src/app.ts"),
            ws.path().join("spa/src/main.ts"),
        )
        .unwrap();
        assert_ne!(digest(ws.path(), &[path("spa"), path("shots")]).unwrap(), a);

        let object = |id: &str| Input::Object {
            named: "origin/main:spa".into(),
            object: id.into(),
        };
        assert_ne!(
            digest(ws.path(), &[object("aaaa")]).unwrap(),
            digest(ws.path(), &[object("bbbb")]).unwrap()
        );
        assert!(matches!(
            digest(ws.path(), &[path("nope")]),
            Err(StashError::Refused(_))
        ));
    }

    #[test]
    fn a_link_is_neither_followed_nor_stashed() {
        let ws = tree();
        std::os::unix::fs::symlink("/etc", ws.path().join("shots/etc")).unwrap();
        std::os::unix::fs::symlink("shots", ws.path().join("via")).unwrap();
        let store = tempfile::tempdir().unwrap();
        let stash = Stash::at(store.path());
        let put = stash
            .put(&scope(), "k", "d", &[], ws.path(), "shots", None, 1)
            .unwrap();
        assert_eq!(put.skipped, ["shots/etc"]);
        assert_eq!(put.meta.files, 1);
        assert!(matches!(
            stash.put(&scope(), "k", "d", &[], ws.path(), "via", None, 1),
            Err(StashError::Refused(_))
        ));
    }

    #[test]
    fn past_the_limit_the_least_recently_used_go_first() {
        let ws = tempfile::tempdir().unwrap();
        let store = tempfile::tempdir().unwrap();
        // Room for three of a kilobyte, and not for a fourth.
        let limit = 3 * 1024 + 200;
        let stash = Stash::at(store.path()).with_limit(limit);
        let third = 1024;
        std::fs::write(ws.path().join("big"), vec![1u8; third]).unwrap();
        for (i, key) in ["a", "b", "c"].iter().enumerate() {
            stash
                .put(&scope(), key, "d", &[], ws.path(), "big", None, i as i64)
                .unwrap();
        }
        // `a` was used last, so `b` is the oldest now.
        assert!(stash.get(&scope(), "a", "d", 10).unwrap().is_ok());
        std::fs::write(ws.path().join("big"), vec![1u8; third + 100]).unwrap();
        let put = stash
            .put(&scope(), "d", "d", &[], ws.path(), "big", None, 11)
            .unwrap();
        assert_eq!(put.evicted, ["b"]);
        assert!(put.used_bytes <= limit);
        assert!(stash.get(&scope(), "b", "d", 12).unwrap().is_err());
        for key in ["a", "c", "d"] {
            assert!(stash.get(&scope(), key, "d", 12).unwrap().is_ok(), "{key}");
        }
        let usage = stash.usage(&scope()).unwrap();
        assert_eq!(usage.entries, 3);
        assert_eq!(usage.bytes, put.used_bytes);
    }

    #[test]
    fn an_entry_over_the_whole_limit_is_refused_and_leaves_nothing() {
        let ws = tempfile::tempdir().unwrap();
        let store = tempfile::tempdir().unwrap();
        let stash = Stash::at(store.path()).with_limit(1024);
        std::fs::write(ws.path().join("huge"), [0u8; 1025]).unwrap();
        assert!(matches!(
            stash.put(&scope(), "k", "d", &[], ws.path(), "huge", None, 1),
            Err(StashError::Refused(_))
        ));
        assert_eq!(stash.usage(&scope()).unwrap().entries, 0);
        let leftovers: Vec<_> = std::fs::read_dir(stash.scope_dir(&scope()))
            .unwrap()
            .flatten()
            .filter(|e| e.file_name() != SCOPE)
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
    }

    #[test]
    fn keys_are_bounded_names() {
        assert!(key_refusal("before-screenshots").is_none());
        assert!(key_refusal(" ").is_some());
        assert!(key_refusal("a\nb").is_some());
        assert!(key_refusal(&"x".repeat(129)).is_some());
    }
}
