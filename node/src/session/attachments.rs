//! Files the operator sends with a prompt: a screenshot, a log.
//!
//! Each attachment is checked on the node (its count, its size, what its
//! bytes are), refused with a reason when it does not pass, and otherwise
//! staged into the session's workspace under one fixed path,
//! [`DIR`]`/<id>/<name>`, before the prompt is sent, so the turn it starts
//! (or, for a prompt held while a turn runs, the turn that delivers it) can
//! open it. The prompt's text then names each staged file with its path,
//! type and size, and the session's log records it (`attachments_staged`:
//! name, path, type, size and SHA-256).
//!
//! **Where the files are written.** Every runtime keeps a session's
//! workspace where the node writes it the same way: while the session's
//! sandbox runs, into that sandbox's live tree over the files channel
//! (`import_dir`, as `fetch_branch` sends its bundle); otherwise into the
//! node's own copy of the workspace, which the sandbox is sent when it
//! starts. Both are the files channel's unpack (`tracon_sandbox::files`):
//! regular files and directories only, no absolute or climbing path, and
//! nothing written through a link the agent left in the tree, so nothing is
//! ever written outside [`DIR`].
//!
//! **Kept out of the work.** [`DIR`] holds a `.gitignore` of `*`, so an
//! attachment is never part of a change the session submits, and never comes
//! back to the node with the checkout.

use std::path::{Path, PathBuf};
use std::time::Duration;

use base64::Engine;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tracon_sandbox::files::Limits;
use tracon_sandbox::protocol::Exclusions;

/// Where, in the session's workspace, attachments are staged.
pub const DIR: &str = ".tracon/attachments";
/// The most files one prompt carries.
pub const MAX_COUNT: usize = 8;
/// The largest one file may be.
pub const MAX_FILE_BYTES: usize = 10 * 1024 * 1024;
/// The most one prompt's files may add up to.
pub const MAX_TOTAL_BYTES: usize = 20 * 1024 * 1024;
/// The longest a staged file's name is, in bytes.
pub const MAX_NAME_BYTES: usize = 128;
/// The largest prompt body the API takes: every file at its limit, as
/// base64, and room for the text.
pub const MAX_BODY_BYTES: usize = MAX_TOTAL_BYTES / 3 * 4 + 2 * 1024 * 1024;
/// How long writing a prompt's files into a running sandbox may take.
const SEND_WITHIN: Duration = Duration::from_secs(120);

/// What the node stages, by what the bytes are, never by what the client
/// says they are.
pub const ACCEPTED: &str =
    "PNG, JPEG, GIF or WebP images, PDF, or UTF-8 text (a log, JSON, CSV, Markdown)";

/// One file as a client sends it with a prompt.
#[derive(Debug, Clone, Deserialize)]
pub struct Upload {
    pub name: String,
    /// The client's idea of its type. Not trusted: the node reads the bytes.
    #[serde(default, rename = "type")]
    pub media_type: Option<String>,
    /// Its bytes, base64.
    pub data: String,
}

/// One file accepted for staging.
#[derive(Debug, Clone)]
pub struct Accepted {
    pub name: String,
    pub media_type: &'static str,
    pub bytes: Vec<u8>,
}

/// What the session records of a staged file, and the API answers with.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Staged {
    pub name: String,
    /// Relative to the workspace's root.
    pub path: String,
    #[serde(rename = "type")]
    pub media_type: String,
    pub size: u64,
    pub sha256: String,
}

/// Check every upload, in order, against the limits above. The first one
/// that fails refuses them all, with why.
pub fn accept(uploads: &[Upload]) -> Result<Vec<Accepted>, String> {
    if uploads.len() > MAX_COUNT {
        return Err(format!(
            "{} files are attached; a prompt carries at most {MAX_COUNT}",
            uploads.len()
        ));
    }
    let mut accepted: Vec<Accepted> = Vec::with_capacity(uploads.len());
    let mut total = 0usize;
    for upload in uploads {
        let shown = upload.name.trim();
        let shown = if shown.is_empty() {
            "an attachment"
        } else {
            shown
        };
        let bytes = decode(&upload.data)
            .map_err(|_| format!("{shown} could not be read: its data is not base64"))?;
        if bytes.len() > MAX_FILE_BYTES {
            return Err(format!(
                "{shown} is {}; one attachment may be at most {}",
                size_text(bytes.len() as u64),
                size_text(MAX_FILE_BYTES as u64)
            ));
        }
        total += bytes.len();
        if total > MAX_TOTAL_BYTES {
            return Err(format!(
                "the attachments add up to more than {}, the most one prompt carries",
                size_text(MAX_TOTAL_BYTES as u64)
            ));
        }
        let name = unique(clean_name(&upload.name), &accepted);
        let media_type = sniff(&bytes, &name)
            .ok_or_else(|| format!("{shown} is not a type the node stages; it takes {ACCEPTED}"))?;
        accepted.push(Accepted {
            name,
            media_type,
            bytes,
        });
    }
    Ok(accepted)
}

fn decode(data: &str) -> Result<Vec<u8>, base64::DecodeError> {
    // A data URL's header is the client's, and says nothing the bytes do not.
    let data = match data.split_once(";base64,") {
        Some((head, rest)) if head.starts_with("data:") => rest,
        _ => data,
    };
    let data: String = data.chars().filter(|c| !c.is_ascii_whitespace()).collect();
    base64::engine::general_purpose::STANDARD.decode(data)
}

/// What the bytes are, when they are something the node stages.
pub fn sniff(bytes: &[u8], name: &str) -> Option<&'static str> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        return Some("image/png");
    }
    if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        return Some("image/jpeg");
    }
    if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        return Some("image/gif");
    }
    if bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP" {
        return Some("image/webp");
    }
    if bytes.starts_with(b"%PDF-") {
        return Some("application/pdf");
    }
    if bytes.contains(&0) || std::str::from_utf8(bytes).is_err() {
        return None;
    }
    let ext = name
        .rsplit_once('.')
        .map(|(_, ext)| ext.to_ascii_lowercase())
        .unwrap_or_default();
    Some(match ext.as_str() {
        "json" => "application/json",
        "jsonl" | "ndjson" => "application/x-ndjson",
        "md" | "markdown" => "text/markdown",
        "csv" => "text/csv",
        "html" | "htm" => "text/html",
        "xml" => "application/xml",
        "yaml" | "yml" => "application/yaml",
        "diff" | "patch" => "text/x-diff",
        _ => "text/plain",
    })
}

/// A name that is one plain file in one directory: its last component, with
/// no separator, control character or leading dot, at most
/// [`MAX_NAME_BYTES`].
pub fn clean_name(name: &str) -> String {
    let last = name.rsplit(['/', '\\']).next().unwrap_or_default();
    let cleaned: String = last
        .chars()
        .map(|c| {
            if c.is_control() || ":*?\"<>|".contains(c) {
                '_'
            } else {
                c
            }
        })
        .collect();
    let cleaned = cleaned.trim().trim_start_matches('.').trim();
    let cleaned = if cleaned.is_empty() {
        "attachment"
    } else {
        cleaned
    };
    if cleaned.len() <= MAX_NAME_BYTES {
        return cleaned.to_string();
    }
    // Cut the stem, keeping a short extension, at a character boundary.
    let (stem, ext) = match cleaned.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() && ext.len() <= 16 => (stem, format!(".{ext}")),
        _ => (cleaned, String::new()),
    };
    let mut keep = MAX_NAME_BYTES - ext.len();
    while !stem.is_char_boundary(keep) {
        keep -= 1;
    }
    format!("{}{ext}", &stem[..keep])
}

/// `name`, or `name-2`, `name-3`… before its extension, so two files sent
/// with one name do not overwrite each other.
fn unique(name: String, taken: &[Accepted]) -> String {
    let used = |candidate: &str| taken.iter().any(|a| a.name == candidate);
    if !used(&name) {
        return name;
    }
    let (stem, ext) = match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() => (stem.to_string(), format!(".{ext}")),
        _ => (name.clone(), String::new()),
    };
    (2..)
        .map(|n| format!("{stem}-{n}{ext}"))
        .find(|candidate| !used(candidate))
        .expect("an unused name")
}

/// A size as the turn's text and the refusals say it.
pub fn size_text(bytes: u64) -> String {
    const KIB: f64 = 1024.0;
    match bytes {
        0..1024 => format!("{bytes} bytes"),
        1024..1_048_576 => format!("{:.1} KiB", bytes as f64 / KIB),
        _ => format!("{:.1} MiB", bytes as f64 / KIB / KIB),
    }
}

/// The directory, in the workspace, of the prompt `id`'s files.
pub fn prompt_dir(id: &str) -> String {
    format!("{DIR}/{id}")
}

/// What is recorded of each file, staged for the prompt `id`.
pub fn staged(id: &str, files: &[Accepted]) -> Vec<Staged> {
    files
        .iter()
        .map(|file| Staged {
            name: file.name.clone(),
            path: format!("{}/{}", prompt_dir(id), file.name),
            media_type: file.media_type.to_string(),
            size: file.bytes.len() as u64,
            sha256: hex::encode(Sha256::digest(&file.bytes)),
        })
        .collect()
}

/// What the turn is told of its attachments, after the operator's own text.
pub fn note(staged: &[Staged]) -> String {
    let (count, them) = match staged.len() {
        1 => ("1 file".to_string(), "It is"),
        n => (format!("{n} files"), "They are"),
    };
    let mut note = format!(
        "[tracon] The operator attached {count} to this message. {them} in your workspace \
         (paths relative to its root), ignored by Git, so never part of your change; open \
         them to read them:"
    );
    for file in staged {
        note.push_str(&format!(
            "\n- `{}` ({}, {})",
            file.path,
            file.media_type,
            size_text(file.size)
        ));
    }
    note
}

/// The prompt's text with its attachments named after it.
pub fn with_note(text: &str, staged: &[Staged]) -> String {
    let text = text.trim_end();
    if text.is_empty() {
        note(staged)
    } else {
        format!("{text}\n\n{}", note(staged))
    }
}

/// Lay the prompt `id`'s files out under `root` as they go into the
/// workspace: [`DIR`]`/.gitignore` and [`DIR`]`/<id>/<name>`.
pub fn lay_out(root: &Path, id: &str, files: &[Accepted]) -> std::io::Result<()> {
    let base = root.join(DIR);
    let dir = base.join(id);
    std::fs::create_dir_all(&dir)?;
    std::fs::write(base.join(".gitignore"), "*\n")?;
    for file in files {
        std::fs::write(dir.join(&file.name), &file.bytes)?;
    }
    Ok(())
}

/// What unpacking a prompt's files may write: its files, the `.gitignore`,
/// and the directories down to them.
fn limits() -> Limits {
    Limits {
        max_bytes: MAX_TOTAL_BYTES as u64 + 64,
        max_files: MAX_COUNT + 1,
        max_dirs: 8,
        ..Limits::default()
    }
}

/// Where the session's workspace is, to write into.
pub enum Place {
    /// In the running sandbox that holds it, at this directory.
    Live {
        conn: tracon_sandbox::node::SandboxConn,
        dir: String,
    },
    /// The node's own copy, which the sandbox is sent when it starts.
    Kept { volume: String, tree: PathBuf },
}

/// Where the workspace `volume` is to be written, or why it cannot be.
pub fn place(backend: &dyn crate::boundary::Backend, volume: &str) -> Result<Place, String> {
    if let Some((conn, dir)) = crate::runner::sandbox_files::live_target(volume) {
        return Ok(Place::Live { conn, dir });
    }
    if crate::runner::sandbox_files::is_live(volume) {
        return Err(
            "the session's sandbox is not connected to the node right now; send the \
             attachments again in a moment"
                .into(),
        );
    }
    backend
        .files_for(volume)
        .and_then(|store| store.tree(volume))
        .map(|tree| Place::Kept {
            volume: volume.to_string(),
            tree,
        })
        .ok_or_else(|| {
            format!(
                "the {} runtime keeps this session's workspace where the node cannot write \
                 attachments into it",
                backend.kind()
            )
        })
}

/// Write the tree laid out at `stage` over the workspace at `place`, by the
/// files channel's rules: nothing through a link, nothing outside the tree.
pub async fn write(place: Place, stage: &Path) -> Result<(), String> {
    match place {
        Place::Live { conn, dir } => {
            match tokio::time::timeout(
                SEND_WITHIN,
                conn.import_dir_within(stage, &dir, Exclusions::None, Some(limits())),
            )
            .await
            {
                Ok(Ok(_)) => Ok(()),
                Ok(Err(error)) => Err(format!(
                    "the attachments could not be written into the session's sandbox: {error}"
                )),
                Err(_) => Err(format!(
                    "writing the attachments into the session's sandbox did not finish within {}s",
                    SEND_WITHIN.as_secs()
                )),
            }
        }
        Place::Kept { volume, tree } => {
            let lock = crate::workspace::volume_lock(&volume);
            let _held = lock.lock().await;
            let stage = stage.to_path_buf();
            tokio::task::spawn_blocking(move || unpack_over(&stage, &tree))
                .await
                .map_err(|e| e.to_string())?
                .map_err(|error| {
                    format!("the attachments could not be written into the workspace: {error}")
                })
        }
    }
}

/// Pack `stage` and unpack it over `tree`, as a sandbox unpacks an import.
pub fn unpack_over(stage: &Path, tree: &Path) -> Result<(), tracon_sandbox::files::FilesError> {
    let mut archive = Vec::new();
    tracon_sandbox::files::pack(stage, Exclusions::None, &mut archive)?;
    tracon_sandbox::files::unpack(archive.as_slice(), tree, Some(tree), &limits())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn upload(name: &str, bytes: &[u8]) -> Upload {
        Upload {
            name: name.into(),
            media_type: None,
            data: base64::engine::general_purpose::STANDARD.encode(bytes),
        }
    }

    const PNG: &[u8] = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR";

    #[test]
    fn what_the_bytes_are_decides_the_type() {
        let files = accept(&[
            upload("shot.png", PNG),
            upload("build.log", b"error: it broke\n"),
            upload("data.json", b"{}"),
            upload("doc.pdf", b"%PDF-1.7\n"),
        ])
        .unwrap();
        let types: Vec<_> = files.iter().map(|f| f.media_type).collect();
        assert_eq!(
            types,
            [
                "image/png",
                "text/plain",
                "application/json",
                "application/pdf"
            ]
        );
        // A name that says image is not believed over its bytes.
        assert_eq!(sniff(b"plain words", "fake.png"), Some("text/plain"));
    }

    #[test]
    fn a_file_of_another_type_is_refused_with_why() {
        let error = accept(&[upload("tool.exe", b"MZ\x90\0\x03\0\0\0\xff\xff")]).unwrap_err();
        assert!(
            error.contains("tool.exe is not a type the node stages"),
            "{error}"
        );
        assert!(error.contains("PNG"), "{error}");
    }

    #[test]
    fn the_limits_are_enforced_with_why() {
        let many: Vec<_> = (0..=MAX_COUNT)
            .map(|n| upload(&format!("{n}.txt"), b"x"))
            .collect();
        let error = accept(&many).unwrap_err();
        assert!(error.contains(&format!("at most {MAX_COUNT}")), "{error}");

        let big = vec![b'a'; MAX_FILE_BYTES + 1];
        let error = accept(&[upload("huge.log", &big)]).unwrap_err();
        assert!(error.starts_with("huge.log is 10.0 MiB"), "{error}");
        assert!(error.contains("at most 10.0 MiB"), "{error}");

        let nine = vec![b'a'; 9 * 1024 * 1024];
        let error = accept(&[
            upload("a.log", &nine),
            upload("b.log", &nine),
            upload("c.log", &nine),
        ])
        .unwrap_err();
        assert!(error.contains("add up to more than 20.0 MiB"), "{error}");

        let error = accept(&[Upload {
            name: "x.txt".into(),
            media_type: None,
            data: "not base64!".into(),
        }])
        .unwrap_err();
        assert!(error.contains("not base64"), "{error}");
    }

    #[test]
    fn a_name_is_one_plain_file() {
        assert_eq!(clean_name("../../etc/passwd"), "passwd");
        assert_eq!(clean_name("C:\\Users\\me\\shot.png"), "shot.png");
        assert_eq!(clean_name(".."), "attachment");
        assert_eq!(clean_name(".bashrc"), "bashrc");
        assert_eq!(clean_name("a\nb?.txt"), "a_b_.txt");
        assert_eq!(clean_name(""), "attachment");
        let long = format!("{}.log", "é".repeat(200));
        let cleaned = clean_name(&long);
        assert!(cleaned.len() <= MAX_NAME_BYTES, "{}", cleaned.len());
        assert!(cleaned.ends_with(".log"), "{cleaned}");
        // Two files of one name both arrive.
        let files = accept(&[upload("a.txt", b"1"), upload("a.txt", b"2")]).unwrap();
        assert_eq!(files[0].name, "a.txt");
        assert_eq!(files[1].name, "a-2.txt");
        // A data URL is read as its bytes.
        let url = Upload {
            data: format!(
                "data:image/png;base64,{}",
                base64::engine::general_purpose::STANDARD.encode(PNG)
            ),
            ..upload("p.png", b"")
        };
        assert_eq!(accept(&[url]).unwrap()[0].media_type, "image/png");
    }

    #[test]
    fn the_turn_names_each_file_with_its_path_type_and_size() {
        let files = accept(&[upload("shot.png", PNG), upload("run.log", &[b'x'; 2048])]).unwrap();
        let staged = staged("p1", &files);
        assert_eq!(staged[0].path, ".tracon/attachments/p1/shot.png");
        assert_eq!(staged[1].size, 2048);
        assert_eq!(staged[1].sha256, hex::encode(Sha256::digest([b'x'; 2048])));
        let text = with_note("look at these", &staged);
        assert!(text.starts_with("look at these\n\n[tracon] The operator attached 2 files"));
        assert!(
            text.contains("- `.tracon/attachments/p1/shot.png` (image/png, 16 bytes)"),
            "{text}"
        );
        assert!(
            text.contains("- `.tracon/attachments/p1/run.log` (text/plain, 2.0 KiB)"),
            "{text}"
        );
        // Files alone still make a prompt.
        assert!(with_note("  ", &staged).starts_with("[tracon]"));
    }

    #[test]
    fn files_land_under_the_fixed_path_and_git_ignores_them() {
        let root = tempfile::tempdir().unwrap();
        let tree = root.path().join("tree");
        std::fs::create_dir_all(tree.join(".tracon/attachments/old")).unwrap();
        std::fs::write(tree.join(".tracon/attachments/old/kept.txt"), "kept").unwrap();
        std::fs::write(tree.join("README"), "mine").unwrap();
        let stage = root.path().join("stage");
        let files = accept(&[upload("shot.png", PNG)]).unwrap();
        lay_out(&stage, "p1", &files).unwrap();
        unpack_over(&stage, &tree).unwrap();
        assert_eq!(
            std::fs::read(tree.join(".tracon/attachments/p1/shot.png")).unwrap(),
            PNG
        );
        assert_eq!(
            std::fs::read_to_string(tree.join(".tracon/attachments/.gitignore")).unwrap(),
            "*\n"
        );
        // What was there stays: an earlier prompt's files and the work.
        assert!(tree.join(".tracon/attachments/old/kept.txt").exists());
        assert_eq!(
            std::fs::read_to_string(tree.join("README")).unwrap(),
            "mine"
        );
        // Git leaves them out of the change.
        let git = |args: &[&str]| {
            std::process::Command::new("git")
                .args(args)
                .current_dir(&tree)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .output()
                .unwrap()
        };
        assert!(git(&["init", "-q"]).status.success());
        let status = git(&["status", "--porcelain", "--untracked-files=all"]);
        let status = String::from_utf8_lossy(&status.stdout);
        assert!(!status.contains(".tracon"), "{status}");
        assert!(status.contains("README"), "{status}");
    }

    #[test]
    fn a_link_the_agent_left_is_never_written_through() {
        let root = tempfile::tempdir().unwrap();
        let outside = root.path().join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        for planted in [".tracon", ".tracon/attachments"] {
            let tree = root.path().join(format!("tree-{}", planted.len()));
            let link = tree.join(planted);
            std::fs::create_dir_all(link.parent().unwrap()).unwrap();
            std::os::unix::fs::symlink(&outside, &link).unwrap();
            let stage = root.path().join(format!("stage-{}", planted.len()));
            lay_out(&stage, "p1", &accept(&[upload("a.txt", b"x")]).unwrap()).unwrap();
            assert!(unpack_over(&stage, &tree).is_err(), "{planted}");
            assert_eq!(std::fs::read_dir(&outside).unwrap().count(), 0, "{planted}");
        }
    }
}
