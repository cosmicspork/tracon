//! Where a review thread is anchored, and whether a later revision has left
//! it outdated.
//!
//! A thread is bound to the revision it was written on, and a file or line
//! thread to a path and a side (`old`, the base; `new`, the change) of that
//! revision's diff. The node reads the anchor from the diff itself, never
//! from the client: a line thread keeps the text of its lines as that diff
//! showed them, a file thread a hash of that file's diff. On a later revision
//! the thread is not moved. It reads **outdated** there when the same lines
//! of the same path and side no longer read the same, or are no longer in
//! the diff, and stays anchored to the revision it was written on. A line
//! shifted by a change above it reads outdated too: that is the price of
//! never pointing a comment at code it was not written about.

use std::collections::BTreeMap;

use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::store::review_thread::{
    ReviewThreadComment, ReviewThreadRow, ADDRESSED, RESOLVED, UNRESOLVED,
};
use crate::store::ReviewRevisionRow;

pub const GENERAL: &str = "general";
pub const FILE: &str = "file";
pub const LINE: &str = "line";

pub const OLD: &str = "old";
pub const NEW: &str = "new";

/// The derived state of a thread whose lines changed on the latest revision.
pub const OUTDATED: &str = "outdated";

/// The most lines one thread may cover.
pub const MAX_RANGE: i64 = 500;
/// The longest comment kept, in bytes.
pub const MAX_COMMENT_BYTES: usize = 64 * 1024;

/// What a new thread is anchored to, as asked.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Anchor {
    pub kind: String,
    pub path: Option<String>,
    pub side: Option<String>,
    pub start_line: Option<i64>,
    pub end_line: Option<i64>,
}

/// One file's part of a unified diff, by the path the review shows it under:
/// the `+++ b/` side, or for a deletion the `diff --git` header's.
fn sections(diff: &str) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    let mut in_hunk = false;
    for line in diff.split_inclusive('\n') {
        let bare = line.trim_end_matches('\n');
        if let Some(header) = bare.strip_prefix("diff --git ") {
            let path = header.rsplit(" b/").next().unwrap_or(header).to_string();
            out.push((path, String::new()));
            in_hunk = false;
        }
        let Some((path, text)) = out.last_mut() else {
            continue;
        };
        text.push_str(line);
        if bare.starts_with("@@") {
            in_hunk = true;
        } else if !in_hunk {
            if let Some(p) = bare.strip_prefix("+++ b/") {
                *path = p.to_string();
            }
        }
    }
    out
}

/// `path`'s part of `diff`, if the diff touches it.
pub fn file_section(diff: &str, path: &str) -> Option<String> {
    sections(diff)
        .into_iter()
        .find(|(p, _)| p == path)
        .map(|(_, text)| text)
}

/// The lines of one file's diff on `side`, by line number, as the diff shows
/// them: context lines on both sides, removals on the old, additions on the
/// new. Lines the diff does not show are absent.
pub fn shown_lines(section: &str, side: &str) -> BTreeMap<i64, String> {
    let mut lines = BTreeMap::new();
    let (mut old, mut new) = (0i64, 0i64);
    let mut in_hunk = false;
    for line in section.lines() {
        if let Some(header) = line.strip_prefix("@@ ") {
            // "@@ -a,b +c,d @@"
            let mut parts = header.split_whitespace();
            let start = |part: Option<&str>, sign: char| {
                part.and_then(|p| p.strip_prefix(sign))
                    .and_then(|p| p.split(',').next())
                    .and_then(|n| n.parse::<i64>().ok())
            };
            match (start(parts.next(), '-'), start(parts.next(), '+')) {
                (Some(o), Some(n)) => {
                    (old, new) = (o, n);
                    in_hunk = true;
                }
                _ => in_hunk = false,
            }
            continue;
        }
        if !in_hunk {
            continue;
        }
        if let Some(text) = line.strip_prefix('+') {
            if side == NEW {
                lines.insert(new, text.to_string());
            }
            new += 1;
        } else if let Some(text) = line.strip_prefix('-') {
            if side == OLD {
                lines.insert(old, text.to_string());
            }
            old += 1;
        } else if line.starts_with('\\') {
            // "\ No newline at end of file"
        } else {
            let text = line.strip_prefix(' ').unwrap_or(line);
            lines.insert(if side == OLD { old } else { new }, text.to_string());
            old += 1;
            new += 1;
        }
    }
    lines
}

fn hash(text: &str) -> String {
    hex::encode(Sha256::digest(text.as_bytes()))
}

/// Check `anchor` against the revision's `diff` and say what it is anchored
/// to there: `None` for a general thread, a hash of the file's diff for a
/// file thread, the lines' text for a line thread. The error says what is
/// wrong with the anchor asked for.
pub fn anchor_in(diff: &str, anchor: &Anchor) -> Result<Option<String>, String> {
    match anchor.kind.as_str() {
        GENERAL => {
            if anchor.path.is_some()
                || anchor.side.is_some()
                || anchor.start_line.is_some()
                || anchor.end_line.is_some()
            {
                return Err("a general thread names no path, side or lines".into());
            }
            Ok(None)
        }
        FILE | LINE => {
            let path = anchor
                .path
                .as_deref()
                .filter(|p| !p.is_empty())
                .ok_or("a file or line thread names the path it is on")?;
            let side = anchor.side.as_deref().unwrap_or(NEW);
            if side != OLD && side != NEW {
                return Err("side is old or new".into());
            }
            let section = file_section(diff, path)
                .ok_or_else(|| format!("{path} is not in this revision's diff"))?;
            if anchor.kind == FILE {
                if anchor.start_line.is_some() || anchor.end_line.is_some() {
                    return Err("a file thread names no lines; a line thread does".into());
                }
                return Ok(Some(hash(&section)));
            }
            let start = anchor
                .start_line
                .ok_or("a line thread names the line it starts on")?;
            let end = anchor.end_line.unwrap_or(start);
            if start < 1 || end < start {
                return Err(
                    "a thread's lines run from start_line to an end_line at or after it, from 1"
                        .into(),
                );
            }
            if end - start + 1 > MAX_RANGE {
                return Err(format!("a thread covers at most {MAX_RANGE} lines"));
            }
            let shown = shown_lines(&section, side);
            let mut text = Vec::new();
            for n in start..=end {
                let line = shown.get(&n).ok_or_else(|| {
                    format!("line {n} of {path} on the {side} side is not in this revision's diff")
                })?;
                text.push(line.as_str());
            }
            Ok(Some(text.join("\n")))
        }
        other => Err(format!(
            "{other} is not a thread kind: general, file or line"
        )),
    }
}

/// Whether `thread`, written on another revision, reads outdated on the
/// revision whose diff is `diff`: its lines or its file's diff no longer read
/// as they did, or are not there.
pub fn outdated_in(diff: &str, thread: &ReviewThreadRow) -> bool {
    let anchor = Anchor {
        kind: thread.kind.clone(),
        path: thread.path.clone(),
        side: thread.side.clone(),
        start_line: thread.start_line,
        end_line: thread.end_line,
    };
    match thread.kind.as_str() {
        GENERAL => false,
        _ => anchor_in(diff, &anchor).ok().flatten() != thread.anchor,
    }
}

/// A thread's state as read on the latest revision: resolved, addressed,
/// outdated, or unresolved, in that order of precedence.
pub fn state_of(thread: &ReviewThreadRow, outdated: bool) -> &'static str {
    match thread.state.as_str() {
        RESOLVED => RESOLVED,
        ADDRESSED => ADDRESSED,
        _ if outdated => OUTDATED,
        _ => UNRESOLVED,
    }
}

/// A review's threads as the review page and the agent read them, on the
/// latest of `revisions` (oldest first). `open_only` leaves out the resolved
/// ones, for the agent.
pub fn view(
    threads: &[(ReviewThreadRow, Vec<ReviewThreadComment>)],
    revisions: &[ReviewRevisionRow],
    open_only: bool,
) -> Vec<Value> {
    let number = |id: &str| {
        revisions
            .iter()
            .position(|r| r.id == id)
            .map(|at| at as i64 + 1)
    };
    let latest = revisions.last();
    threads
        .iter()
        .filter(|(t, _)| !(open_only && t.state == RESOLVED))
        .map(|(t, comments)| {
            let outdated = latest
                .is_some_and(|latest| latest.id != t.revision_id && outdated_in(&latest.diff, t));
            let mut v = json!({
                "id": t.id,
                "kind": t.kind,
                "revision_id": t.revision_id,
                "revision": number(&t.revision_id),
                "state": state_of(t, outdated),
                "outdated": outdated,
                "comments": comments.iter().map(|c| json!({
                    "author": c.author,
                    "body": c.body,
                    "revision": c.revision_id.as_deref().and_then(number),
                    "at_ms": c.created_ms,
                })).collect::<Vec<_>>(),
                "created_ms": t.created_ms,
                "updated_ms": t.updated_ms,
            });
            if t.kind != GENERAL {
                v["path"] = json!(t.path);
                v["side"] = json!(t.side);
            }
            if t.kind == LINE {
                v["start_line"] = json!(t.start_line);
                v["end_line"] = json!(t.end_line);
                // What it was written on, whatever the lines read now.
                v["lines"] = json!(t.anchor);
            }
            if let Some(addressed) = &t.addressed_revision_id {
                v["addressed_revision"] = json!(number(addressed));
            }
            v
        })
        .collect()
}

/// A review's threads as [`view`] reads them, from the store.
pub fn of_review(
    store: &crate::store::Store,
    review_id: &str,
    open_only: bool,
) -> Result<Vec<Value>, crate::store::StoreError> {
    let threads = store.review_threads(review_id)?;
    if threads.is_empty() {
        return Ok(Vec::new());
    }
    let revisions = store.review_revisions(review_id)?;
    Ok(view(&threads, &revisions, open_only))
}

#[cfg(test)]
mod tests {
    use super::*;

    const REV1: &str = "\
diff --git a/src/a.rs b/src/a.rs
index 1..2 100644
--- a/src/a.rs
+++ b/src/a.rs
@@ -1,4 +1,5 @@
 fn one() {}
-fn two() {}
+fn two() -> u8 { 2 }
+fn three() {}
 fn four() {}
 fn five() {}
diff --git a/README.md b/README.md
--- a/README.md
+++ b/README.md
@@ -1 +1 @@
-old
+new
";

    /// The same change, with the line a thread is on rewritten.
    const REV2: &str = "\
diff --git a/src/a.rs b/src/a.rs
index 1..3 100644
--- a/src/a.rs
+++ b/src/a.rs
@@ -1,4 +1,5 @@
 fn one() {}
-fn two() {}
+fn two() -> u16 { 2 }
+fn three() {}
 fn four() {}
 fn five() {}
diff --git a/README.md b/README.md
--- a/README.md
+++ b/README.md
@@ -1 +1 @@
-old
+new
";

    fn line(path: &str, side: &str, start: i64, end: i64) -> Anchor {
        Anchor {
            kind: LINE.into(),
            path: Some(path.into()),
            side: Some(side.into()),
            start_line: Some(start),
            end_line: Some(end),
        }
    }

    fn thread(anchor: &Anchor, on: &str) -> ReviewThreadRow {
        ReviewThreadRow {
            id: "t".into(),
            review_id: "r".into(),
            revision_id: "rev1".into(),
            kind: anchor.kind.clone(),
            path: anchor.path.clone(),
            side: anchor.side.clone(),
            start_line: anchor.start_line,
            end_line: anchor.end_line,
            anchor: anchor_in(on, anchor).unwrap(),
            state: UNRESOLVED.into(),
            addressed_revision_id: None,
            created_ms: 0,
            updated_ms: 0,
        }
    }

    #[test]
    fn lines_are_numbered_on_the_side_they_are_read_on() {
        let section = file_section(REV1, "src/a.rs").unwrap();
        let new = shown_lines(&section, NEW);
        assert_eq!(
            new.get(&2).map(String::as_str),
            Some("fn two() -> u8 { 2 }")
        );
        assert_eq!(new.get(&3).map(String::as_str), Some("fn three() {}"));
        assert_eq!(new.get(&5).map(String::as_str), Some("fn five() {}"));
        let old = shown_lines(&section, OLD);
        assert_eq!(old.get(&2).map(String::as_str), Some("fn two() {}"));
        assert_eq!(old.get(&4).map(String::as_str), Some("fn five() {}"));
        assert_eq!(
            anchor_in(REV1, &line("src/a.rs", NEW, 2, 3))
                .unwrap()
                .as_deref(),
            Some("fn two() -> u8 { 2 }\nfn three() {}")
        );
    }

    #[test]
    fn an_anchor_the_diff_does_not_show_is_refused() {
        for (anchor, says) in [
            (line("src/b.rs", NEW, 1, 1), "not in this revision's diff"),
            (line("src/a.rs", NEW, 5, 9), "line 6 of src/a.rs"),
            (line("src/a.rs", NEW, 3, 2), "end_line"),
            (line("src/a.rs", "middle", 1, 1), "old or new"),
            (
                Anchor {
                    kind: GENERAL.into(),
                    path: Some("src/a.rs".into()),
                    ..Anchor::default()
                },
                "names no path",
            ),
            (
                Anchor {
                    kind: "hunk".into(),
                    ..Anchor::default()
                },
                "not a thread kind",
            ),
        ] {
            let error = anchor_in(REV1, &anchor).unwrap_err();
            assert!(error.contains(says), "{anchor:?}: {error}");
        }
    }

    #[test]
    fn a_thread_whose_lines_changed_reads_outdated_and_is_not_moved() {
        let on_two = thread(&line("src/a.rs", NEW, 2, 2), REV1);
        let on_five = thread(&line("src/a.rs", NEW, 5, 5), REV1);
        let on_old = thread(&line("src/a.rs", OLD, 2, 2), REV1);
        let on_file = thread(
            &Anchor {
                kind: FILE.into(),
                path: Some("src/a.rs".into()),
                side: Some(NEW.into()),
                ..Anchor::default()
            },
            REV1,
        );
        let on_readme = thread(
            &Anchor {
                kind: FILE.into(),
                path: Some("README.md".into()),
                side: Some(NEW.into()),
                ..Anchor::default()
            },
            REV1,
        );
        let general = thread(
            &Anchor {
                kind: GENERAL.into(),
                ..Anchor::default()
            },
            REV1,
        );
        assert!(outdated_in(REV2, &on_two));
        assert!(outdated_in(REV2, &on_file));
        assert!(!outdated_in(REV2, &on_five));
        assert!(!outdated_in(REV2, &on_old));
        assert!(!outdated_in(REV2, &on_readme));
        assert!(!outdated_in(REV2, &general));
        // A file the new revision no longer changes leaves its threads outdated.
        assert!(outdated_in("", &on_readme));
        // Still anchored where it was written.
        assert_eq!(on_two.anchor.as_deref(), Some("fn two() -> u8 { 2 }"));
    }

    #[test]
    fn the_view_reads_threads_on_the_latest_revision() {
        let revision = |id: &str, diff: &str| ReviewRevisionRow {
            id: id.into(),
            review_id: "r".into(),
            candidate_id: "c".into(),
            title: String::new(),
            body: String::new(),
            diff: diff.into(),
            files: "[]".into(),
            head_sha: String::new(),
            context_json: "[]".into(),
            requirements_work_item_id: None,
            requirements_title: None,
            requirements_body: None,
            requirements_hash: None,
            created_ms: 0,
            intent_json: None,
        };
        let mut addressed = thread(&line("src/a.rs", NEW, 2, 2), REV1);
        addressed.id = "addressed".into();
        addressed.state = ADDRESSED.into();
        addressed.addressed_revision_id = Some("rev2".into());
        let mut resolved = thread(&line("src/a.rs", NEW, 5, 5), REV1);
        resolved.id = "resolved".into();
        resolved.state = RESOLVED.into();
        let outdated = thread(&line("src/a.rs", NEW, 2, 2), REV1);
        let threads = vec![(outdated, vec![]), (addressed, vec![]), (resolved, vec![])];
        let one = [revision("rev1", REV1)];
        let two = [revision("rev1", REV1), revision("rev2", REV2)];
        let states = |view: Vec<Value>| {
            view.iter()
                .map(|t| t["state"].as_str().unwrap().to_string())
                .collect::<Vec<_>>()
        };
        assert_eq!(
            states(view(&threads, &one, false)),
            [UNRESOLVED, ADDRESSED, RESOLVED]
        );
        assert_eq!(
            states(view(&threads, &two, false)),
            [OUTDATED, ADDRESSED, RESOLVED]
        );
        let agent = view(&threads, &two, true);
        assert_eq!(states(agent.clone()), [OUTDATED, ADDRESSED]);
        assert_eq!(agent[0]["revision"], 1);
        assert_eq!(agent[0]["outdated"], true);
        assert_eq!(agent[0]["lines"], "fn two() -> u8 { 2 }");
        assert_eq!(agent[1]["addressed_revision"], 2);
    }
}
