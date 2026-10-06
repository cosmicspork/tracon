//! `show_work`: what an agent built, shown to the operator beside its diff.
//!
//! The gap between "work completed" and "ready to merge" is the operator's to
//! close, and a diff alone often does not close it. The agent builds whatever
//! helps — a written account, an HTML page with its images, plain files —
//! inside its boundary, and this tool carries it out to its session and to the
//! review its candidate is in. The files are read from the workspace snapshot
//! the node exports itself and stored in the HTML bundle store, so a page is
//! only ever rendered on the isolated preview origin, under its content
//! security policy, with no network.
//!
//! Shown work is the agent's account, not verification. The node vouches only
//! for what it already knows: the commit the work was shown at, that it is
//! stale once the candidate moves, and the required checks it ran on that
//! commit. None of it leaves tracon: publication sends the forge prose only.
//!
//! Like the operator interventions it is dispatched before the policy is
//! consulted. It writes nothing anyone else reads and reaches no forge, so a
//! signed bundle that predates it cannot turn showing work into a question.

use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use crate::{
    corpus::html::{normalize_path, HtmlFile, MAX_BUNDLE_BYTES, MAX_FILES, MAX_FILE_BYTES},
    mcp::{CallContext, SessionAccess},
    review::publish::Target,
    session::state::event_kind as ek,
    store::{
        now_ms,
        shown::{ShownFile, ShownWorkRow, FILES, HTML, MARKDOWN},
    },
};

pub const SHOW: &str = "show_work";

/// The listing the node writes for plain files. Not a path a workspace file
/// can collide with silently: one that does is refused by name.
pub const LISTING: &str = "tracon-shown.html";

const MAX_TITLE_CHARS: usize = 200;
const MAX_MARKDOWN_BYTES: usize = 256 * 1024;

pub fn definitions() -> Vec<Value> {
    vec![json!({
        "name": SHOW,
        "description": "Show the operator what you built, beside the diff they review: a changed \
                        screen, a before and after, a short account of what you tried. It is \
                        your account to help them decide, not verification, and it stays in \
                        tracon — the pull request gets prose only. Pass `markdown` for a written \
                        account, `paths` for files in your workspace (relative to the repository \
                        root; a directory brings its files), and `entry` to have one of those \
                        files rendered as an HTML page with whatever it loads beside it. Pages \
                        render on an isolated origin with no network, so inline or bundle every \
                        asset. Without `entry`, the files are listed for the operator to open. \
                        The node records the commit your workspace is at: commit first, and \
                        show again after a resubmission, since work shown at an older commit is \
                        marked stale. It appears on this session and on its reviews; pass \
                        `review_id` to attach it to one review only. A harness you run yourself \
                        must pass `review_id`, and paths are read from that review's worktree.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "title": { "type": "string", "description": "What this shows, as the operator will see it labelled." },
                "markdown": { "type": "string", "description": "A written account, in Markdown." },
                "paths": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Files or directories in your workspace, relative to the repository root.",
                },
                "entry": { "type": "string", "description": "With `paths`: the .html file to render, one of the files shown." },
                "review_id": { "type": "string", "description": "The review this belongs to. Defaults to every review of this session." },
            },
            "required": ["title"],
        },
    })]
}

pub async fn call(
    access: &SessionAccess,
    ctx: &CallContext,
    args: &Value,
) -> Result<Value, String> {
    let store = &access.store;
    let manager = &access.manager;
    let title = args
        .get("title")
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or("");
    if title.is_empty() || title.chars().count() > MAX_TITLE_CHARS {
        return Err(format!(
            "title is required, and at most {MAX_TITLE_CHARS} characters"
        ));
    }
    let markdown = match args.get("markdown") {
        None | Some(Value::Null) => "",
        Some(Value::String(markdown)) => markdown.as_str(),
        Some(_) => return Err("markdown is a string".into()),
    };
    if markdown.len() > MAX_MARKDOWN_BYTES {
        return Err(format!(
            "markdown is {} bytes; the limit is {MAX_MARKDOWN_BYTES}. Put a long account in a page \
             and show it with `paths` and `entry`.",
            markdown.len()
        ));
    }
    let paths: Vec<String> = match args.get("paths") {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(items)) => items
            .iter()
            .map(|item| {
                item.as_str()
                    .map(str::to_string)
                    .ok_or_else(|| "paths are strings".to_string())
            })
            .collect::<Result<_, _>>()?,
        Some(_) => return Err("paths is a list of paths".into()),
    };
    let entry = match args.get("entry") {
        None | Some(Value::Null) => None,
        Some(Value::String(entry)) => {
            Some(clean(entry).map_err(|_| format!("entry {entry:?} is not a relative path"))?)
        }
        Some(_) => return Err("entry is a path".into()),
    };
    if entry.is_some() && paths.is_empty() {
        return Err("entry names one of `paths`; pass the files it is part of".into());
    }
    if markdown.trim().is_empty() && paths.is_empty() {
        return Err("nothing to show: pass `markdown`, `paths`, or both".into());
    }

    let review = match args.get("review_id").and_then(Value::as_str) {
        Some(id) => {
            let review = store
                .get_review(id)
                .map_err(|e| e.to_string())?
                .ok_or("no review with that id")?;
            if !super::review::caller_owns(
                store,
                ctx,
                &review.channel,
                review.session_id.as_deref(),
            ) {
                return Err("that review belongs to another session".into());
            }
            if review.kind == crate::store::reports::KIND {
                return Err(
                    "that id is a standalone narrative report; put what it shows in the report"
                        .into(),
                );
            }
            if review.node_id != manager.node_id() {
                return Err("that review is held by another node".into());
            }
            Some(review)
        }
        None => None,
    };

    // Where the files are read from, and so the commit the work is shown at:
    // the snapshot the node exports itself, never a path the agent names.
    let root: PathBuf = match (ctx.session_id(), &review) {
        (Some(session_id), _) => manager
            .snapshot_workspace(session_id)
            .await
            .map_err(|e| e.to_string())?,
        (None, Some(review)) => serde_json::from_str::<Target>(&review.target)
            .ok()
            .and_then(|target| target.worktree)
            .map(PathBuf::from)
            .ok_or("that review has no worktree to read from")?,
        (None, None) => {
            return Err(
                "a harness you run yourself shows work on a review: pass `review_id`".into(),
            )
        }
    };
    let head_sha = crate::review::resolve(&root.to_string_lossy(), "HEAD")
        .await
        .map_err(|e| format!("the workspace has no commit to show work at: {e}"))?;

    let id = uuid::Uuid::now_v7().to_string();
    let collected = if paths.is_empty() {
        None
    } else {
        let root = root.clone();
        Some(
            tokio::task::spawn_blocking(move || collect(&root, &paths))
                .await
                .map_err(|e| e.to_string())??,
        )
    };
    let (format, document, files) = match collected {
        None => (MARKDOWN, None, Vec::new()),
        Some(Collected { files, skipped }) => {
            let listed: Vec<ShownFile> = files
                .iter()
                .map(|file| ShownFile {
                    path: file.path.clone(),
                    size_bytes: file.bytes.len() as u64,
                })
                .collect();
            let (format, entry_path, bundle) = match &entry {
                Some(entry) => {
                    if !files.iter().any(|file| &file.path == entry) {
                        return Err(format!("entry {entry} is not one of the files shown"));
                    }
                    (HTML, entry.clone(), files)
                }
                None => {
                    if files.iter().any(|file| file.path == LISTING) {
                        return Err(format!(
                            "{LISTING} is the listing the node writes for plain files; name it \
                             as `entry` to show it as a page, or rename it"
                        ));
                    }
                    let listing = listing(title, &listed);
                    let mut files = files;
                    files.push(HtmlFile {
                        path: LISTING.into(),
                        bytes: listing.into_bytes(),
                    });
                    (FILES, LISTING.to_string(), files)
                }
            };
            let slug = format!("{}-{id}", super::docs::SHOWN);
            let (doc, changes) = store
                .write_html_document_change(
                    manager.node_id(),
                    &ctx.channel,
                    &slug,
                    &slug,
                    &entry_path,
                    bundle,
                    None,
                    true,
                )
                .map_err(|e| e.to_string())?;
            for change in changes {
                manager.bus().publish(crate::stream::Frame::Changes {
                    channel: ctx.channel.clone(),
                    changes: vec![change],
                });
            }
            (format, Some((doc, skipped)), listed)
        }
    };

    let row = ShownWorkRow {
        id: id.clone(),
        channel: ctx.channel.clone(),
        session_id: ctx.session_id().map(str::to_string),
        lane: ctx.lane().map(str::to_string),
        review_id: review.as_ref().map(|review| review.id.clone()),
        head_sha: head_sha.clone(),
        title: title.to_string(),
        format: format.to_string(),
        markdown: markdown.to_string(),
        document_id: document.as_ref().map(|(doc, _)| doc.id.clone()),
        document_slug: document.as_ref().map(|(doc, _)| doc.slug.clone()),
        document_hash: document.as_ref().map(|(doc, _)| doc.hash.clone()),
        files,
        created_ms: now_ms(),
    };
    store.insert_shown_work(&row).map_err(|e| e.to_string())?;
    manager.record_for(
        ctx,
        ek::WORK_SHOWN,
        Some(&id),
        json!({
            "id": id,
            "title": row.title,
            "format": row.format,
            "head_sha": head_sha,
            "review_id": row.review_id,
            "channel": row.channel,
            "slug": row.document_slug,
            "files": row.files.len(),
        }),
    );
    let skipped = document.map(|(_, skipped)| skipped).unwrap_or_default();
    let mut out = json!({
        "shown_work_id": id,
        "format": row.format,
        "head_sha": head_sha,
        "review_id": row.review_id,
        "files": row.files.len(),
        "note": "Shown to the operator in tracon beside the diff, marked with the commit above; \
                 it goes stale once the candidate moves. Nothing here is sent to the forge.",
    });
    if !skipped.is_empty() {
        out["skipped"] = json!(skipped);
    }
    Ok(out)
}

/// What a set of paths brought, and what was left out on the way: a symbolic
/// link inside a directory, or anything under `.git`.
#[derive(Debug)]
struct Collected {
    files: Vec<HtmlFile>,
    skipped: Vec<String>,
}

/// A path as the agent wrote it, relative to the repository root.
fn clean(path: &str) -> Result<String, ()> {
    let trimmed = path.trim();
    let trimmed = trimmed.strip_prefix("./").unwrap_or(trimmed);
    let trimmed = trimmed.trim_end_matches('/');
    normalize_path(trimmed).map_err(|_| ())
}

fn is_git(segment: &str) -> bool {
    segment == ".git"
}

/// Read the named files from the workspace snapshot. A path is refused when it
/// leaves the root, names Git's own directory, or passes through a symbolic
/// link: the snapshot is the node's copy of the workspace, and nothing is read
/// from anywhere a link could point it at.
fn collect(root: &Path, paths: &[String]) -> Result<Collected, String> {
    let mut files: Vec<HtmlFile> = Vec::new();
    let mut skipped = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut total = 0usize;
    for asked in paths {
        let relative = clean(asked)
            .map_err(|_| format!("{asked:?} is not a path relative to the repository root"))?;
        if relative.split('/').any(is_git) {
            return Err(format!("{relative} is Git's own directory, not the work"));
        }
        let mut at = root.to_path_buf();
        let mut metadata = None;
        for segment in relative.split('/') {
            at.push(segment);
            let found = std::fs::symlink_metadata(&at)
                .map_err(|_| format!("{relative} is not in the workspace"))?;
            if found.file_type().is_symlink() {
                return Err(format!(
                    "{relative} is, or passes through, a symbolic link; show the file itself"
                ));
            }
            metadata = Some(found);
        }
        let metadata = metadata.expect("a normalized path has a segment");
        let mut add = |path: String, at: &Path, len: u64| -> Result<(), String> {
            if !seen.insert(path.clone()) {
                return Ok(());
            }
            if files.len() >= MAX_FILES {
                return Err(format!(
                    "more than {MAX_FILES} files; show fewer, or a directory that holds only the work"
                ));
            }
            if len > MAX_FILE_BYTES as u64 {
                return Err(format!("{path} is over the {MAX_FILE_BYTES}-byte limit"));
            }
            total = total.saturating_add(len as usize);
            if total > MAX_BUNDLE_BYTES {
                return Err(format!(
                    "what was shown is over the {MAX_BUNDLE_BYTES}-byte limit"
                ));
            }
            let bytes = std::fs::read(at).map_err(|e| format!("{path}: {e}"))?;
            files.push(HtmlFile { path, bytes });
            Ok(())
        };
        if metadata.is_file() {
            add(relative, &at, metadata.len())?;
        } else if metadata.is_dir() {
            let mut stack = vec![(at, relative)];
            while let Some((dir, prefix)) = stack.pop() {
                let entries: Vec<_> = std::fs::read_dir(&dir)
                    .map_err(|e| format!("{prefix}: {e}"))?
                    .collect::<Result<_, _>>()
                    .map_err(|e| format!("{prefix}: {e}"))?;
                for entry in entries {
                    let Some(name) = entry.file_name().to_str().map(str::to_string) else {
                        skipped.push(format!("{prefix}/{}", entry.file_name().to_string_lossy()));
                        continue;
                    };
                    let path = format!("{prefix}/{name}");
                    let kind = entry.file_type().map_err(|e| format!("{path}: {e}"))?;
                    if is_git(&name) || kind.is_symlink() || normalize_path(&path).is_err() {
                        skipped.push(path);
                    } else if kind.is_dir() {
                        stack.push((entry.path(), path));
                    } else if kind.is_file() {
                        let len = entry.metadata().map_err(|e| format!("{path}: {e}"))?.len();
                        add(path, &entry.path(), len)?;
                    } else {
                        skipped.push(path);
                    }
                }
            }
        } else {
            return Err(format!("{relative} is neither a file nor a directory"));
        }
    }
    if files.is_empty() {
        return Err("the paths named no files".into());
    }
    files.sort_by(|a, b| a.path.cmp(&b.path));
    skipped.sort();
    Ok(Collected { files, skipped })
}

fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}

/// A path as a relative URL: each segment percent-encoded, so a name with a
/// `#`, a `?` or a space still opens the file it names.
fn href(path: &str) -> String {
    path.split('/')
        .map(|segment| {
            let mut out = String::new();
            for byte in segment.bytes() {
                if byte.is_ascii_alphanumeric() || b"-._~".contains(&byte) {
                    out.push(byte as char);
                } else {
                    out.push_str(&format!("%{byte:02X}"));
                }
            }
            out
        })
        .collect::<Vec<_>>()
        .join("/")
}

fn size(bytes: u64) -> String {
    match bytes {
        b if b >= 1024 * 1024 => format!("{:.1} MiB", b as f64 / (1024.0 * 1024.0)),
        b if b >= 1024 => format!("{:.1} KiB", b as f64 / 1024.0),
        b => format!("{b} B"),
    }
}

/// The page that lists plain files. Static, and served under the same policy
/// as any other bundle.
fn listing(title: &str, files: &[ShownFile]) -> String {
    let mut items = String::new();
    for file in files {
        items.push_str(&format!(
            "<li><a href=\"{}\">{}</a> <small>{}</small></li>\n",
            escape(&href(&file.path)),
            escape(&file.path),
            size(file.size_bytes)
        ));
    }
    format!(
        "<!doctype html>\n<html><head><meta charset=\"utf-8\"><title>{title}</title>\
         <style>body{{font:14px system-ui,sans-serif;margin:1.5rem;color:#222}}\
         li{{margin:.3rem 0}}small{{color:#777}}</style></head>\n\
         <body><h1>{title}</h1>\n<ul>\n{items}</ul></body></html>\n",
        title = escape(title),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree() -> tempfile_like::Dir {
        tempfile_like::Dir::new()
    }

    /// No temp-dir crate in this package: a directory under the system temp
    /// directory, removed on drop.
    mod tempfile_like {
        pub struct Dir(pub std::path::PathBuf);
        impl Dir {
            pub fn new() -> Self {
                let path =
                    std::env::temp_dir().join(format!("tracon-show-{}", uuid::Uuid::now_v7()));
                std::fs::create_dir_all(&path).unwrap();
                Self(path)
            }
        }
        impl Drop for Dir {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
    }

    #[test]
    fn a_directory_brings_its_files_in_order_and_leaves_git_out() {
        let dir = tree();
        let root = &dir.0;
        std::fs::create_dir_all(root.join("out/img")).unwrap();
        std::fs::create_dir_all(root.join("out/.git")).unwrap();
        std::fs::write(root.join("out/index.html"), "<p>hi</p>").unwrap();
        std::fs::write(root.join("out/img/a.png"), [1u8, 2, 3]).unwrap();
        std::fs::write(root.join("out/.git/config"), "x").unwrap();
        let got = collect(root, &["./out/".into(), "out/index.html".into()]).unwrap();
        let paths: Vec<_> = got.files.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(paths, ["out/img/a.png", "out/index.html"]);
        assert_eq!(got.skipped, ["out/.git"]);
    }

    #[test]
    fn a_path_that_leaves_the_root_or_names_git_is_refused() {
        let dir = tree();
        let root = &dir.0;
        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::fs::write(root.join(".git/config"), "x").unwrap();
        for bad in [
            "../etc/passwd",
            "/etc/passwd",
            ".git/config",
            "a/../../b",
            "",
        ] {
            assert!(collect(root, &[bad.into()]).is_err(), "{bad}");
        }
        assert!(collect(root, &["missing.txt".into()]).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn a_symbolic_link_is_never_followed() {
        let dir = tree();
        let root = &dir.0;
        let outside = tree();
        std::fs::write(outside.0.join("secret"), "s").unwrap();
        std::fs::create_dir_all(root.join("out")).unwrap();
        std::fs::write(root.join("out/ok.txt"), "ok").unwrap();
        std::os::unix::fs::symlink(outside.0.join("secret"), root.join("out/link")).unwrap();
        std::os::unix::fs::symlink(&outside.0, root.join("dirlink")).unwrap();
        // Named directly, or as a directory on the way, a link is refused.
        assert!(collect(root, &["out/link".into()]).is_err());
        assert!(collect(root, &["dirlink/secret".into()]).is_err());
        // Met inside a directory, it is left out and said so.
        let got = collect(root, &["out".into()]).unwrap();
        assert_eq!(got.files.len(), 1);
        assert_eq!(got.skipped, ["out/link"]);
    }

    #[test]
    fn the_listing_escapes_names_and_links_each_file() {
        let page = listing(
            "<b>before & after</b>",
            &[ShownFile {
                path: "shots/a b#1.png".into(),
                size_bytes: 2048,
            }],
        );
        assert!(page.contains("&lt;b&gt;before &amp; after&lt;/b&gt;"));
        assert!(page.contains("href=\"shots/a%20b%231.png\""));
        assert!(page.contains("2.0 KiB"));
    }
}
