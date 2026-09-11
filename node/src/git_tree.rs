//! Git's own tree-object hash, recomputed directly from a flat file list.
//!
//! A continuity transfer's `verify()` recomputes this from the package's
//! files and requires it to equal the candidate's own recorded `tree_sha`
//! (`git rev-parse <commit>^{tree}` at capture time). That closes the gap
//! where a signer authorized on a channel could label a package with one
//! candidate's identity while shipping a different, unrelated file tree: the
//! candidate/evidence identity checks alone only bind opaque JSON fields, not
//! the actual bytes being transferred.
//!
//! Candidate evidence only records the hash string; it exposes no helper to
//! recompute it from a plain file list, so the algorithm is implemented here
//! directly from Git's documented tree-object format.

use std::collections::BTreeMap;

use base64::Engine;
use sha1::{Digest, Sha1};

use crate::transfers::TransferFile;

fn blob_sha1(content: &[u8]) -> [u8; 20] {
    let mut hasher = Sha1::new();
    hasher.update(b"blob ");
    hasher.update(content.len().to_string().as_bytes());
    hasher.update([0]);
    hasher.update(content);
    hasher.finalize().into()
}

#[derive(Default)]
struct Tree {
    files: BTreeMap<String, (u32, [u8; 20])>,
    dirs: BTreeMap<String, Tree>,
}

impl Tree {
    fn insert(&mut self, path: &str, mode: u32, blob: [u8; 20]) {
        match path.split_once('/') {
            Some((dir, rest)) if !dir.is_empty() && !rest.is_empty() => {
                self.dirs
                    .entry(dir.to_string())
                    .or_default()
                    .insert(rest, mode, blob);
            }
            _ => {
                self.files.insert(path.to_string(), (mode, blob));
            }
        }
    }

    fn sha1(&self) -> [u8; 20] {
        // Git sorts tree entries as if a subtree's name carried a trailing
        // `/`, so a file `foo.txt` sorts before a directory entry `foo`.
        let mut entries: Vec<(String, u32, [u8; 20])> = self
            .files
            .iter()
            .map(|(name, (mode, sha))| (name.clone(), *mode, *sha))
            .collect();
        for (name, dir) in &self.dirs {
            entries.push((name.clone(), 0o040_000, dir.sha1()));
        }
        entries.sort_by_key(|entry| sort_key(&entry.0, entry.1));
        let mut body = Vec::new();
        for (name, mode, sha) in &entries {
            body.extend_from_slice(format!("{mode:o} ").as_bytes());
            body.extend_from_slice(name.as_bytes());
            body.push(0);
            body.extend_from_slice(sha);
        }
        let mut hasher = Sha1::new();
        hasher.update(b"tree ");
        hasher.update(body.len().to_string().as_bytes());
        hasher.update([0]);
        hasher.update(&body);
        hasher.finalize().into()
    }
}

fn sort_key(name: &str, mode: u32) -> Vec<u8> {
    let mut key = name.as_bytes().to_vec();
    if mode & 0o170_000 == 0o040_000 {
        key.push(b'/');
    }
    key
}

/// The hex SHA-1 of the root git tree object these files would produce,
/// matching `git rev-parse <commit>^{tree}` for the commit that captured
/// them. Modes other than a regular or executable blob are meaningless here;
/// `validate_files` already rejects everything else before this ever runs.
/// A malformed `content_b64` decodes as empty rather than panicking: the
/// resulting hash simply will not match, which `verify` treats as invalid.
pub fn tree_sha1(files: &[TransferFile]) -> String {
    let mut root = Tree::default();
    for file in files {
        let content = base64::engine::general_purpose::STANDARD
            .decode(file.content_b64.as_bytes())
            .unwrap_or_default();
        root.insert(&file.path, file.mode, blob_sha1(&content));
    }
    hex::encode(root.sha1())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(path: &str, content: &[u8]) -> TransferFile {
        TransferFile {
            path: path.into(),
            mode: 0o100644,
            content_b64: base64::engine::general_purpose::STANDARD.encode(content),
        }
    }
    #[test]
    fn matches_known_git_tree_hash() {
        // `printf 'hello\n' | git hash-object -t blob --stdin` =
        // ce013625030ba8dba906f756967f9e9ca394464a; `git rev-parse
        // HEAD^{tree}` for a repo containing only that file as `hello.txt`
        // is aaa96ced2d9a1c8e72c56b253a0e2fe78393feb7, reproducible with:
        //   git init -q d && cd d && printf 'hello\n' > hello.txt \
        //     && git add hello.txt && git commit -q -m x \
        //     && git rev-parse HEAD^{tree}
        let files = vec![file("hello.txt", b"hello\n")];
        assert_eq!(
            tree_sha1(&files),
            "aaa96ced2d9a1c8e72c56b253a0e2fe78393feb7"
        );
    }

    #[test]
    fn nested_paths_build_subtrees() {
        let a = vec![file("dir/a.txt", b"a")];
        let b = vec![file("dir/a.txt", b"a")];
        assert_eq!(tree_sha1(&a), tree_sha1(&b));
        let different = vec![file("dir/a.txt", b"b")];
        assert_ne!(tree_sha1(&a), tree_sha1(&different));
    }

    #[test]
    fn file_before_same_named_directory() {
        // `foo.txt` (0x2E) must sort before a `foo/` subtree (0x2F).
        let with_file_first = vec![file("foo.txt", b"x"), file("foo/bar.txt", b"y")];
        let with_file_last = vec![file("foo/bar.txt", b"y"), file("foo.txt", b"x")];
        assert_eq!(tree_sha1(&with_file_first), tree_sha1(&with_file_last));
    }
}
