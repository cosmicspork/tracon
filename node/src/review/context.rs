//! The whole of one reviewed file on each side of a revision, for the review
//! screen to show the lines around a hunk that the diff left out.
//!
//! Context is evidence too: an unchanged line shown beside a change must be
//! the line the change was made against, not whatever the worktree holds now.
//! So nothing here reads a file from disk. The base side is the blob the
//! revision's own diff names on its `index` line, or a file at the commit of
//! an earlier revision; the candidate side is the file at the revision's
//! commit, or, once the worktree is gone, the copy the evidence store kept of
//! that commit's tree. Git objects are addressed by hash, so what comes back
//! is fixed by the revision whatever has happened to the branch since.

use serde::Serialize;

use super::blob;

/// A file larger than this is not sent whole; the diff's own lines stand.
pub const MAX_CONTEXT_BYTES: usize = 2 * 1024 * 1024;

/// One side of a file as the revision pinned it, or why it cannot be shown.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Side {
    pub text: Option<String>,
    pub why: Option<String>,
}

impl Side {
    pub fn text(text: String) -> Self {
        Self {
            text: Some(text),
            why: None,
        }
    }

    pub fn missing(why: impl Into<String>) -> Self {
        Self {
            text: None,
            why: Some(why.into()),
        }
    }
}

/// The blob hashes a unified diff's `index` line gives one file, before and
/// after, as git wrote them (possibly abbreviated). `None` when the diff has
/// no such file or names no blobs for it (a rename with no content change).
pub fn diff_blobs(diff: &str, path: &str) -> Option<(String, String)> {
    let mut names = Vec::<String>::new();
    let mut index = None::<(String, String)>;
    let mut found = None;
    let mut settle = |names: &mut Vec<String>, index: &mut Option<(String, String)>| {
        if found.is_none() && names.iter().any(|n| n == path) {
            found = index.take();
        }
        names.clear();
        *index = None;
    };
    let mut in_hunk = false;
    for line in diff.lines() {
        if let Some(header) = line.strip_prefix("diff --git ") {
            settle(&mut names, &mut index);
            in_hunk = false;
            if let Some(b) = header.rsplit(" b/").next() {
                names.push(b.to_string());
            }
            continue;
        }
        if in_hunk {
            continue;
        }
        if line.starts_with("@@") {
            in_hunk = true;
        } else if let Some(p) = line.strip_prefix("+++ b/") {
            names.push(p.to_string());
        } else if let Some(p) = line.strip_prefix("rename to ") {
            names.push(p.to_string());
        } else if let Some(rest) = line.strip_prefix("index ") {
            let range = rest.split(' ').next().unwrap_or("");
            if let Some((a, b)) = range.split_once("..") {
                index = Some((a.to_string(), b.to_string()));
            }
        }
    }
    settle(&mut names, &mut index);
    found
}

/// Whether a blob hash is git's way of saying the file is absent on that side.
fn absent(hash: &str) -> bool {
    !hash.is_empty() && hash.bytes().all(|b| b == b'0')
}

/// Bytes as text, or why they are not shown as text.
fn as_text(bytes: Vec<u8>) -> Side {
    if bytes.len() > MAX_CONTEXT_BYTES {
        return Side::missing(format!(
            "the file is {} KiB, over the {} KiB shown whole",
            bytes.len() / 1024,
            MAX_CONTEXT_BYTES / 1024
        ));
    }
    if bytes.contains(&0) {
        return Side::missing("the file is binary");
    }
    match String::from_utf8(bytes) {
        Ok(text) => Side::text(text),
        Err(_) => Side::missing("the file is not UTF-8 text"),
    }
}

/// Read one object out of the repository by hash, as text.
async fn object(worktree: &str, object: &str, gone: &str) -> Side {
    match blob(worktree, object).await {
        Ok(bytes) => as_text(bytes),
        Err(_) => Side::missing(gone.to_string()),
    }
}

/// The base side of `path`: at `from` (an earlier revision's commit) when
/// given, else the blob the revision's diff names for it.
pub async fn base_side(worktree: Option<&str>, diff: &str, path: &str, from: Option<&str>) -> Side {
    let Some(worktree) = worktree else {
        return Side::missing("the repository this review was captured from is gone");
    };
    if let Some(from) = from {
        return object(
            worktree,
            &format!("{from}:{path}"),
            &format!("the file is not in {from:.8}, the earlier revision"),
        )
        .await;
    }
    match diff_blobs(diff, path) {
        Some((a, _)) if absent(&a) => Side::missing("the file is new in this change"),
        Some((a, _)) => {
            object(
                worktree,
                &a,
                &format!("the repository no longer holds {a}, the file's base"),
            )
            .await
        }
        None => Side::missing("the diff names no base for this file"),
    }
}

/// The candidate side of `path`: the file at the revision's commit, read from
/// the repository, or from the copy of that commit's tree the evidence store
/// kept (`kept`) when the repository cannot say.
pub async fn head_side(
    worktree: Option<&str>,
    head_sha: &str,
    path: &str,
    kept: impl FnOnce() -> Option<Vec<u8>>,
) -> Side {
    let read = match worktree {
        Some(worktree) => blob(worktree, &format!("{head_sha}:{path}")).await.ok(),
        None => None,
    };
    match read.or_else(kept) {
        Some(bytes) => as_text(bytes),
        None => Side::missing(format!(
            "neither the repository nor the evidence store holds the file at {head_sha:.8}"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DIFF: &str = "diff --git a/src/a.rs b/src/a.rs\n\
index 83db48f..bf269f4 100644\n\
--- a/src/a.rs\n\
+++ b/src/a.rs\n\
@@ -1,2 +1,2 @@\n\
-index 1..2 is not a header in a hunk\n\
+x\n\
diff --git a/old.rs b/new.rs\n\
similarity index 90%\n\
rename from old.rs\n\
rename to new.rs\n\
index 1111111..2222222 100644\n\
--- a/old.rs\n\
+++ b/new.rs\n\
@@ -1 +1 @@\n\
-a\n\
+b\n\
diff --git a/added.rs b/added.rs\n\
new file mode 100644\n\
index 0000000..3333333\n\
--- /dev/null\n\
+++ b/added.rs\n\
@@ -0,0 +1 @@\n\
+n\n\
diff --git a/moved.rs b/moved/here.rs\n\
similarity index 100%\n\
rename from moved.rs\n\
rename to moved/here.rs\n";

    #[test]
    fn the_blobs_are_read_from_each_files_own_index_line() {
        assert_eq!(
            diff_blobs(DIFF, "src/a.rs"),
            Some(("83db48f".into(), "bf269f4".into()))
        );
        assert_eq!(
            diff_blobs(DIFF, "new.rs"),
            Some(("1111111".into(), "2222222".into()))
        );
        let (a, _) = diff_blobs(DIFF, "added.rs").unwrap();
        assert!(absent(&a));
        // A pure rename names no blobs; a path the diff lacks has none.
        assert_eq!(diff_blobs(DIFF, "moved/here.rs"), None);
        assert_eq!(diff_blobs(DIFF, "nowhere.rs"), None);
    }

    #[test]
    fn binary_and_oversized_files_say_why_instead_of_sending_text() {
        assert_eq!(
            as_text(b"a\0b".to_vec()).why.as_deref(),
            Some("the file is binary")
        );
        assert!(as_text(vec![b'a'; MAX_CONTEXT_BYTES + 1])
            .why
            .unwrap()
            .contains("KiB"));
        assert_eq!(as_text(b"ok\n".to_vec()), Side::text("ok\n".into()));
    }
}
