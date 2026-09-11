//! Writing a channel's documents out as files: `<slug>.md` for a live one,
//! `archive/<slug>.md` for an archived one — the shape `import` reads. The
//! directory is mirrored, not appended to: a document deleted or moved to the
//! archive loses its old file. A file without a document kind's prefix is
//! never touched, so a README or anything else kept alongside survives.

use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use crate::config::Config;
use crate::mcp::docs::{kind_of, valid_slug};
use crate::store::{Store, StoreError};

#[derive(Debug, Clone, PartialEq)]
pub struct ExportDoc {
    pub slug: String,
    pub body: String,
    pub archived: bool,
}

#[derive(Debug, Default, PartialEq)]
pub struct Report {
    pub written: usize,
    pub unchanged: usize,
    pub removed: usize,
    pub skipped_html: usize,
}

/// Make `dir` hold exactly these documents.
pub fn sync_dir(dir: &Path, docs: &[ExportDoc]) -> std::io::Result<Report> {
    let archive = dir.join("archive");
    std::fs::create_dir_all(dir)?;
    let mut report = Report::default();
    let mut live = HashSet::new();
    let mut archived = HashSet::new();
    for d in docs {
        let into = if d.archived {
            std::fs::create_dir_all(&archive)?;
            archived.insert(d.slug.as_str());
            &archive
        } else {
            live.insert(d.slug.as_str());
            dir
        };
        let path = into.join(format!("{}.md", d.slug));
        if std::fs::read_to_string(&path).ok().as_deref() == Some(d.body.as_str()) {
            report.unchanged += 1;
            continue;
        }
        std::fs::write(&path, &d.body)?;
        report.written += 1;
    }
    report.removed += prune(dir, &live)?;
    if archive.is_dir() {
        report.removed += prune(&archive, &archived)?;
    }
    Ok(report)
}

/// Remove the `<kind>-….md` files in `dir` that are no longer a document there.
fn prune(dir: &Path, keep: &HashSet<&str>) -> std::io::Result<usize> {
    let mut removed = 0;
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(slug) = name.strip_suffix(".md") else {
            continue;
        };
        if !entry.path().is_file()
            || !valid_slug(slug)
            || kind_of(slug) == "other"
            || keep.contains(slug)
        {
            continue;
        }
        std::fs::remove_file(entry.path())?;
        removed += 1;
    }
    Ok(removed)
}

/// Every Markdown document on a channel, with an explicit HTML skip count.
pub fn from_store(store: &Store, channel: &str) -> Result<(Vec<ExportDoc>, usize), StoreError> {
    let mut out = Vec::new();
    let mut skipped_html = 0;
    for listed in store.doc_list(Some(channel))? {
        if listed.format == "html" {
            skipped_html += 1;
            continue;
        }
        if let Some(d) = store.doc_get(channel, &listed.slug)? {
            out.push(ExportDoc {
                slug: d.slug,
                body: d.body,
                archived: d.archived != 0,
            });
        }
    }
    Ok((out, skipped_html))
}

/// `[docs] export_dir` on its timer: at startup, then every
/// `export_every_secs`. Off while `export_dir` is unset.
pub async fn run(store: Arc<Store>, cfg: Arc<Config>) {
    let Some(dir) = cfg
        .docs
        .export_dir
        .as_deref()
        .map(crate::config::expand_home)
    else {
        return;
    };
    let channel = if cfg.docs.export_channel.is_empty() {
        cfg.session.default_channel.clone()
    } else {
        cfg.docs.export_channel.clone()
    };
    if channel.is_empty() {
        tracing::warn!(
            "[docs] export_dir is set, but neither export_channel nor [session] default_channel names a channel; nothing is exported"
        );
        return;
    }
    let mut tick = tokio::time::interval(Duration::from_secs(cfg.docs.export_every_secs.max(60)));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tick.tick().await;
        let (store, into, from) = (store.clone(), dir.clone(), channel.clone());
        let done = tokio::task::spawn_blocking(move || {
            let (docs, skipped_html) = from_store(&store, &from).map_err(|e| e.to_string())?;
            let mut report = sync_dir(&into, &docs).map_err(|e| e.to_string())?;
            report.skipped_html = skipped_html;
            Ok::<_, String>(report)
        })
        .await;
        match done {
            Ok(Ok(r)) if r.written + r.removed + r.skipped_html > 0 => tracing::info!(
                written = r.written,
                removed = r.removed,
                skipped_html = r.skipped_html,
                dir = %dir.display(),
                "Markdown documents exported"
            ),
            Ok(Ok(_)) => {}
            Ok(Err(e)) => {
                tracing::warn!(error = %e, dir = %dir.display(), "document export failed")
            }
            Err(e) => tracing::warn!(error = %e, "document export stopped"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc(slug: &str, body: &str, archived: bool) -> ExportDoc {
        ExportDoc {
            slug: slug.into(),
            body: body.into(),
            archived,
        }
    }

    #[test]
    fn a_directory_mirrors_the_documents_and_leaves_other_files_alone() {
        let dir = std::env::temp_dir().join(format!("tracon-export-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("archive")).unwrap();
        for (path, body) in [
            ("README.md", "keep"),
            ("notes.txt", "keep"),
            ("note-gone.md", "old"),
            ("plan-moved.md", "old"),
            ("guide-same.md", "same"),
            ("archive/ref-gone.md", "old"),
        ] {
            std::fs::write(dir.join(path), body).unwrap();
        }
        let docs = [
            doc("guide-same", "same", false),
            doc("note-new", "new", false),
            doc("plan-moved", "moved", true),
        ];
        let r = sync_dir(&dir, &docs).unwrap();
        assert_eq!(
            r,
            Report {
                written: 2,
                unchanged: 1,
                removed: 3,
                skipped_html: 0,
            }
        );
        for kept in [
            "README.md",
            "notes.txt",
            "guide-same.md",
            "note-new.md",
            "archive/plan-moved.md",
        ] {
            assert!(dir.join(kept).exists(), "{kept}");
        }
        for gone in ["note-gone.md", "plan-moved.md", "archive/ref-gone.md"] {
            assert!(!dir.join(gone).exists(), "{gone}");
        }
        assert_eq!(
            std::fs::read_to_string(dir.join("archive/plan-moved.md")).unwrap(),
            "moved"
        );
        // Run again: nothing left to do.
        assert_eq!(
            sync_dir(&dir, &docs).unwrap(),
            Report {
                written: 0,
                unchanged: 3,
                removed: 0,
                skipped_html: 0,
            }
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn store_export_reports_and_skips_html_documents() {
        let store = Store::open_in_memory().unwrap();
        store
            .write_change(
                "node",
                "personal",
                "document",
                tracon_sync::ChangeOp::Upsert,
                "markdown",
                serde_json::json!({
                    "channel": "personal",
                    "slug": "guide-markdown",
                    "kind": "guide",
                    "title": "Markdown",
                    "body": "# Markdown",
                    "hash": "hash",
                    "created_ms": 1,
                    "updated_ms": 1,
                }),
            )
            .unwrap();
        store
            .write_html_document_change(
                "node",
                "personal",
                "ref-html",
                "page.html",
                "page.html",
                vec![crate::corpus::html::HtmlFile {
                    path: "page.html".into(),
                    bytes: b"<title>HTML</title>".to_vec(),
                }],
                None,
                true,
            )
            .unwrap();
        let (docs, skipped_html) = from_store(&store, "personal").unwrap();
        assert_eq!(docs.len(), 1);
        assert_eq!(docs[0].slug, "guide-markdown");
        assert_eq!(skipped_html, 1);
    }
}
