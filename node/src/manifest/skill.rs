//! Importing a skill package into storage the node owns.
//!
//! A skill is a directory with a `SKILL.md` at its root and whatever assets
//! and scripts it ships beside it. Upstream discovers such directories from
//! six places, follows symlinks in all of them, applies no depth or size
//! limit, and fetches `skills.urls` over the network before any interaction
//! (`config-state.md` §3.1–§3.6). The node does none of that. It reads one
//! package that an operator named, validates every path in it, copies the
//! bytes into its own storage, and renders exactly that at launch.
//!
//! What the validation is actually for: a package path that is absolute, or
//! that climbs out with `..`, or that is a symlink to somewhere else on the
//! machine, would be staged into the session's read-only mount and handed to
//! a harness. The node stages files, not references — so a symlink is refused
//! rather than followed, and a path that is not plainly relative is refused
//! rather than normalised.

use std::path::{Component, Path, PathBuf};

use sha2::{Digest, Sha256};

use super::{ManifestError, ManifestFile, SkillEntry};

/// The warning every import records. Skill *content* is executable: a
/// `SKILL.md` body is registered as a slash command and command templates are
/// shell-interpolated, so a body carrying `` !`cmd` `` runs a shell on
/// `/<name>` without reaching the `skill` permission check
/// (`config-state.md` §3.7). Scripts beside it are listed by the skill tool
/// and run by the harness's own `bash`, under the gate like any other command.
pub const CODE_WARNING: &str = "Skill content is code: its scripts run inside the runner under \
                                the gate, and its body is also registered as a slash command \
                                whose template OpenCode shell-interpolates. Import only packages \
                                you would run yourself.";

/// Bounds. A skill package is documentation and a script or two; anything
/// past these is a mistake worth reporting rather than a package worth
/// mounting. Upstream applies no limit of any kind.
const MAX_FILES: usize = 64;
const MAX_FILE_BYTES: usize = 256 * 1024;
const MAX_TOTAL_BYTES: usize = 1024 * 1024;
const MAX_DEPTH: usize = 8;

/// Where an import read the package from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkillSource {
    /// A directory on this machine, as it is right now.
    Dir(PathBuf),
    /// A directory as one revision of a Git repository recorded it. The
    /// revision is resolved to a commit id so the recorded source names the
    /// bytes rather than a branch that moves.
    Git {
        repo: PathBuf,
        commit: String,
        prefix: String,
    },
}

impl SkillSource {
    /// How the manifest records it, for an operator reading the pane.
    pub fn label(&self) -> String {
        match self {
            Self::Dir(path) => format!("dir:{}", path.display()),
            Self::Git {
                repo,
                commit,
                prefix,
            } => format!("git:{commit} {}/{prefix}", repo.display()),
        }
    }
}

/// Is this string a URL skill source? Refused wherever it appears: upstream's
/// `skills.urls` fetch has no off-switch, retries automatically, and on the
/// pinned version writes fetched files through an unhardened `path.join`.
pub fn is_url(source: &str) -> bool {
    let lower = source.trim().to_ascii_lowercase();
    ["http://", "https://", "ftp://", "//"]
        .iter()
        .any(|scheme| lower.starts_with(scheme))
}

/// `<dir>` or `<dir>#<rev>`. A URL is refused here rather than at manifest
/// build, so the operator is told at the moment they asked for it.
pub fn parse_source(argument: &str) -> Result<SkillSource, ManifestError> {
    if is_url(argument) {
        return Err(ManifestError::UrlSource(argument.to_string()));
    }
    match argument.split_once('#') {
        Some((path, rev)) if !rev.trim().is_empty() => git_source(Path::new(path), rev.trim()),
        _ => {
            let path = PathBuf::from(argument);
            if !path.is_dir() {
                return Err(ManifestError::Skill(format!(
                    "`{}` is not a directory. Name a skill package directory (one holding \
                     SKILL.md), or `<directory>#<git-rev>` to import it as a revision \
                     recorded it.",
                    path.display()
                )));
            }
            Ok(SkillSource::Dir(
                path.canonicalize().unwrap_or(path.clone()),
            ))
        }
    }
}

fn git(repo: &Path, args: &[&str]) -> Result<Vec<u8>, ManifestError> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()?;
    if !out.status.success() {
        return Err(ManifestError::Skill(format!(
            "git {} failed in {}: {}",
            args.join(" "),
            repo.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    Ok(out.stdout)
}

fn git_text(repo: &Path, args: &[&str]) -> Result<String, ManifestError> {
    Ok(String::from_utf8_lossy(&git(repo, args)?)
        .trim()
        .to_string())
}

fn git_source(path: &Path, rev: &str) -> Result<SkillSource, ManifestError> {
    if !path.is_dir() {
        return Err(ManifestError::Skill(format!(
            "`{}` is not a directory on this machine; a git import still names the \
             package's location in the working tree.",
            path.display()
        )));
    }
    let repo = PathBuf::from(git_text(path, &["rev-parse", "--show-toplevel"])?);
    let prefix = git_text(path, &["rev-parse", "--show-prefix"])?
        .trim_end_matches('/')
        .to_string();
    // `<rev>^{commit}`: what is recorded is the commit, not the name that
    // pointed at it, so re-reading the source later reads the same bytes.
    let commit = git_text(
        path,
        &["rev-parse", "--verify", &format!("{rev}^{{commit}}")],
    )?;
    Ok(SkillSource::Git {
        repo,
        commit,
        prefix,
    })
}

/// Read, validate and hash a package. Nothing is written anywhere: the caller
/// decides whether the result becomes part of a manifest.
pub fn read_package(source: &SkillSource) -> Result<SkillEntry, ManifestError> {
    let files = match source {
        SkillSource::Dir(dir) => read_dir(dir)?,
        SkillSource::Git {
            repo,
            commit,
            prefix,
        } => read_git(repo, commit, prefix)?,
    };
    if files.is_empty() {
        return Err(ManifestError::Skill(
            "the package holds no files".to_string(),
        ));
    }
    let body = files
        .iter()
        .find(|file| file.path == "SKILL.md")
        .ok_or_else(|| {
            ManifestError::Skill(
                "the package has no SKILL.md at its root; that file is what makes a \
                 directory a skill"
                    .to_string(),
            )
        })?;
    let (name, description) = frontmatter(&body.text)?;
    validate_name(&name)?;
    if description.contains('<') || description.contains('>') {
        // `name` and `description` are interpolated into the system prompt
        // without HTML escaping (`config-state.md` §3.7); `location` is the
        // only field that is escaped.
        return Err(ManifestError::Skill(format!(
            "the description of `{name}` contains markup. OpenCode writes a skill's name \
             and description into the system prompt without escaping them, so angle \
             brackets there are an injection channel."
        )));
    }

    let mut warnings = vec![CODE_WARNING.to_string()];
    for file in &files {
        if file.text.contains("!`") {
            warnings.push(format!(
                "`{}` contains shell interpolation (!`…`). OpenCode registers a skill as a \
                 slash command and interpolates its template, so this executes on \
                 /{name} on a path that never reaches the skill permission check.",
                file.path
            ));
        }
    }
    if files.len() > 1 {
        warnings.push(format!(
            "{} file(s) beside SKILL.md are mounted into the session and readable by it.",
            files.len() - 1
        ));
    }

    Ok(SkillEntry {
        name,
        description,
        source: source.label(),
        digest: digest_of(&files),
        warnings,
        files,
    })
}

/// sha256 over the package's sorted `(path, bytes)` pairs. Content-addressed,
/// so the same package imported twice from two places digests the same and a
/// single changed byte anywhere moves it.
pub fn digest_of(files: &[ManifestFile]) -> String {
    let mut sorted: Vec<&ManifestFile> = files.iter().collect();
    sorted.sort_by(|a, b| a.path.cmp(&b.path));
    let mut hasher = Sha256::new();
    for file in sorted {
        hasher.update(file.path.as_bytes());
        hasher.update([0u8]);
        hasher.update((file.text.len() as u64).to_be_bytes());
        hasher.update(file.text.as_bytes());
    }
    hex::encode(hasher.finalize())
}

/// A path a package may contain. Plainly relative, no climbing, bounded.
fn validate_rel(path: &str) -> Result<(), ManifestError> {
    if path.is_empty() {
        return Err(ManifestError::Skill("a file has an empty path".into()));
    }
    let as_path = Path::new(path);
    if as_path.is_absolute() || path.starts_with('/') || path.starts_with('\\') {
        return Err(ManifestError::Skill(format!(
            "`{path}` is an absolute path; a skill package holds only paths relative to \
             its own root."
        )));
    }
    let mut depth = 0;
    for component in as_path.components() {
        match component {
            Component::Normal(_) => depth += 1,
            Component::CurDir => {
                return Err(ManifestError::Skill(format!("`{path}` is not normalised")))
            }
            Component::ParentDir => {
                return Err(ManifestError::Skill(format!(
                    "`{path}` climbs out of the package with `..`; the node stages files, \
                     so a path that leaves the package is refused rather than resolved."
                )))
            }
            _ => {
                return Err(ManifestError::Skill(format!(
                    "`{path}` is not a plain relative path"
                )))
            }
        }
    }
    if depth > MAX_DEPTH {
        return Err(ManifestError::Skill(format!(
            "`{path}` is nested deeper than {MAX_DEPTH} directories"
        )));
    }
    Ok(())
}

fn text_of(path: &str, bytes: Vec<u8>) -> Result<String, ManifestError> {
    if bytes.len() > MAX_FILE_BYTES {
        return Err(ManifestError::Skill(format!(
            "`{path}` is {} bytes; a skill package file may be at most {MAX_FILE_BYTES}",
            bytes.len()
        )));
    }
    String::from_utf8(bytes).map_err(|_| {
        ManifestError::Skill(format!(
            "`{path}` is not valid UTF-8. A skill package is text — SKILL.md, scripts and \
             text assets — because the node stages its contents rather than a reference \
             to them."
        ))
    })
}

fn read_dir(root: &Path) -> Result<Vec<ManifestFile>, ManifestError> {
    let mut files = Vec::new();
    let mut total = 0usize;
    let mut stack = vec![(root.to_path_buf(), String::new())];
    while let Some((dir, rel)) = stack.pop() {
        let mut entries: Vec<_> = std::fs::read_dir(&dir)?.collect::<Result<Vec<_>, _>>()?;
        entries.sort_by_key(std::fs::DirEntry::file_name);
        for entry in entries {
            let name = entry.file_name().to_string_lossy().into_owned();
            let child = if rel.is_empty() {
                name.clone()
            } else {
                format!("{rel}/{name}")
            };
            validate_rel(&child)?;
            // `symlink_metadata`, never `metadata`: a symlink is refused
            // rather than followed, which is the whole of the escape check.
            // Upstream's own scans hardcode `symlink: true` and follow them.
            let meta = std::fs::symlink_metadata(entry.path())?;
            if meta.file_type().is_symlink() {
                return Err(ManifestError::Skill(format!(
                    "`{child}` is a symlink. The node stages a package's bytes into a \
                     read-only mount, so a link out of the package is refused rather \
                     than followed."
                )));
            }
            if meta.is_dir() {
                stack.push((entry.path(), child));
                continue;
            }
            if !meta.is_file() {
                return Err(ManifestError::Skill(format!(
                    "`{child}` is neither a file nor a directory"
                )));
            }
            total += meta.len() as usize;
            if total > MAX_TOTAL_BYTES {
                return Err(ManifestError::Skill(format!(
                    "the package is larger than {MAX_TOTAL_BYTES} bytes"
                )));
            }
            let text = text_of(&child, std::fs::read(entry.path())?)?;
            files.push(ManifestFile { path: child, text });
            if files.len() > MAX_FILES {
                return Err(ManifestError::Skill(format!(
                    "the package holds more than {MAX_FILES} files"
                )));
            }
        }
    }
    files.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(files)
}

fn read_git(repo: &Path, commit: &str, prefix: &str) -> Result<Vec<ManifestFile>, ManifestError> {
    let tree = if prefix.is_empty() {
        commit.to_string()
    } else {
        format!("{commit}:{prefix}")
    };
    let listing = git(repo, &["ls-tree", "-r", "-z", "--full-tree", &tree])?;
    let mut files = Vec::new();
    let mut total = 0usize;
    for record in listing.split(|b| *b == 0) {
        if record.is_empty() {
            continue;
        }
        let record = String::from_utf8_lossy(record);
        let (meta, path) = record.split_once('\t').ok_or_else(|| {
            ManifestError::Skill(format!(
                "git listed a tree entry this node cannot read: {record}"
            ))
        })?;
        let mode = meta.split_whitespace().next().unwrap_or("");
        validate_rel(path)?;
        // 120000 is a symlink, 160000 a submodule gitlink. Neither is bytes
        // this node can stage.
        if mode == "120000" {
            return Err(ManifestError::Skill(format!(
                "`{path}` is a symlink at that revision; the node stages bytes, not links."
            )));
        }
        if mode == "160000" {
            return Err(ManifestError::Skill(format!(
                "`{path}` is a submodule at that revision, whose contents are not in this \
                 repository."
            )));
        }
        // `<commit>:<path-from-the-repo-root>`. The listing's paths are
        // relative to the package, so the prefix goes back on here: there is
        // no `<commit>:<dir>:<file>` spelling.
        let object = if prefix.is_empty() {
            format!("{commit}:{path}")
        } else {
            format!("{commit}:{prefix}/{path}")
        };
        let bytes = git(repo, &["show", &object])?;
        total += bytes.len();
        if total > MAX_TOTAL_BYTES {
            return Err(ManifestError::Skill(format!(
                "the package is larger than {MAX_TOTAL_BYTES} bytes"
            )));
        }
        files.push(ManifestFile {
            path: path.to_string(),
            text: text_of(path, bytes)?,
        });
        if files.len() > MAX_FILES {
            return Err(ManifestError::Skill(format!(
                "the package holds more than {MAX_FILES} files"
            )));
        }
    }
    files.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(files)
}

/// `name` and `description` out of the YAML frontmatter. Upstream accepts
/// only those two fields and validates nothing else (`config-state.md` §3.1);
/// this reads the same two and refuses a file without them rather than
/// inferring a name from the directory, which is a v2 behaviour the pinned v1
/// does not have.
fn frontmatter(text: &str) -> Result<(String, String), ManifestError> {
    let body = text
        .strip_prefix("---\n")
        .or_else(|| text.strip_prefix("---\r\n"))
        .ok_or_else(|| {
            ManifestError::Skill(
                "SKILL.md does not start with a `---` frontmatter block".to_string(),
            )
        })?;
    let end = body
        .find("\n---")
        .ok_or_else(|| ManifestError::Skill("SKILL.md's frontmatter is not closed".to_string()))?;
    let mut name = String::new();
    let mut description = String::new();
    for line in body[..end].lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let value = value
            .trim()
            .trim_matches('"')
            .trim_matches('\'')
            .to_string();
        match key.trim() {
            "name" => name = value,
            "description" => description = value,
            _ => {}
        }
    }
    if name.is_empty() {
        return Err(ManifestError::Skill(
            "SKILL.md's frontmatter has no `name`".to_string(),
        ));
    }
    Ok((name, description))
}

/// The name rule upstream's documentation states and its code does not
/// enforce. The node enforces it, because the name is three things at once
/// here: the directory the package is staged into, the argument to the `skill`
/// tool, and a slash command.
fn validate_name(name: &str) -> Result<(), ManifestError> {
    let ok = !name.is_empty()
        && name.len() <= 64
        && name.split('-').all(|part| {
            !part.is_empty()
                && part
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        });
    if !ok {
        return Err(ManifestError::Skill(format!(
            "`{name}` is not a usable skill name. Use lowercase letters, digits and single \
             hyphens (at most 64 characters): the name is the directory the package is \
             staged into, the argument to the skill tool, and a slash command."
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn package(dir: &Path, body: &str) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join("SKILL.md"), body).unwrap();
    }

    const GOOD: &str =
        "---\nname: release-notes\ndescription: Draft release notes\n---\n\nSteps.\n";

    fn scratch(name: &str) -> PathBuf {
        let dir = crate::config::Config::state_dir()
            .join("skill-test")
            .join(name);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A git import reads the bytes one revision recorded, not the ones in the
    /// working tree — and records the commit rather than the name that pointed
    /// at it, so re-reading the source later reads the same package.
    #[test]
    fn a_git_revision_is_read_as_that_revision_recorded_it() {
        let repo = scratch("git");
        let run = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .arg("-C")
                .arg(&repo)
                .args(args)
                .output()
                .expect("git runs");
            assert!(
                out.status.success(),
                "git {args:?}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        };
        run(&["init", "-q", "-b", "main"]);
        run(&["config", "user.email", "test@localhost"]);
        run(&["config", "user.name", "test"]);
        let dir = repo.join("skills/release-notes");
        package(&dir, GOOD);
        run(&["add", "-A"]);
        run(&["commit", "-q", "-m", "first"]);

        // The working tree moves on; the revision does not.
        package(&dir, &GOOD.replace("Steps.", "Rewritten."));

        let source = parse_source(&format!("{}#HEAD", dir.display())).unwrap();
        let SkillSource::Git { ref commit, .. } = source else {
            panic!("a `#rev` suffix is a git source")
        };
        assert_eq!(commit.len(), 40, "the commit id is resolved, not the name");
        let entry = read_package(&source).unwrap();
        assert!(entry.files[0].text.contains("Steps."), "{:?}", entry.files);
        assert!(entry.source.starts_with("git:"));

        // And the working tree's version digests differently, which is the
        // whole reason the source is recorded.
        let live = read_package(&SkillSource::Dir(dir)).unwrap();
        assert_ne!(entry.digest, live.digest);
    }

    #[test]
    fn a_package_is_read_named_and_digested() {
        let dir = scratch("read").join("release-notes");
        package(&dir, GOOD);
        std::fs::write(dir.join("run.sh"), "#!/bin/sh\necho hi\n").unwrap();
        let entry = read_package(&SkillSource::Dir(dir.clone())).unwrap();
        assert_eq!(entry.name, "release-notes");
        assert_eq!(entry.description, "Draft release notes");
        assert_eq!(
            entry
                .files
                .iter()
                .map(|f| f.path.as_str())
                .collect::<Vec<_>>(),
            ["SKILL.md", "run.sh"]
        );
        assert!(entry.source.starts_with("dir:"));
        assert!(entry.warnings[0].contains("Skill content is code"));
        // Re-reading the same bytes digests the same.
        let again = read_package(&SkillSource::Dir(dir)).unwrap();
        assert_eq!(entry.digest, again.digest);
    }

    #[test]
    fn a_changed_byte_changes_the_package_digest() {
        let dir = scratch("changed").join("release-notes");
        package(&dir, GOOD);
        let before = read_package(&SkillSource::Dir(dir.clone())).unwrap().digest;
        package(&dir, &GOOD.replace("Steps.", "Steps!"));
        let after = read_package(&SkillSource::Dir(dir)).unwrap().digest;
        assert_ne!(before, after);
    }

    /// The escape check. A symlink is the only way a package can name bytes
    /// outside itself once absolute and `..` paths are gone, and upstream
    /// follows them everywhere.
    #[test]
    fn a_symlink_out_of_the_package_is_refused() {
        let root = scratch("symlink");
        let secret = root.join("secret");
        std::fs::write(&secret, "not yours").unwrap();
        let dir = root.join("release-notes");
        package(&dir, GOOD);
        #[cfg(unix)]
        std::os::unix::fs::symlink(&secret, dir.join("leak.md")).unwrap();
        let error = read_package(&SkillSource::Dir(dir)).expect_err("a symlink must be refused");
        assert!(error.to_string().contains("symlink"), "{error}");
    }

    #[test]
    fn a_package_without_skill_md_is_refused() {
        let dir = scratch("no-skill-md").join("thing");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("README.md"), "hi").unwrap();
        let error = read_package(&SkillSource::Dir(dir)).expect_err("SKILL.md is required");
        assert!(error.to_string().contains("SKILL.md"), "{error}");
    }

    #[test]
    fn a_body_that_shell_interpolates_is_reported_by_name() {
        let dir = scratch("shell").join("release-notes");
        package(&dir, &format!("{GOOD}\nRun !`rm -rf /` first.\n"));
        let entry = read_package(&SkillSource::Dir(dir)).unwrap();
        assert!(
            entry
                .warnings
                .iter()
                .any(|w| w.contains("shell interpolation")),
            "{:?}",
            entry.warnings
        );
    }

    #[test]
    fn a_url_source_never_becomes_a_source_at_all() {
        for url in ["https://skills.example/x", "HTTP://skills.example/x"] {
            assert!(matches!(
                parse_source(url),
                Err(ManifestError::UrlSource(_))
            ));
        }
    }

    #[test]
    fn a_path_that_climbs_out_is_refused() {
        for path in ["../outside", "a/../../b", "/etc/passwd"] {
            assert!(validate_rel(path).is_err(), "{path}");
        }
        assert!(validate_rel("a/b/c.md").is_ok());
    }

    #[test]
    fn names_follow_the_rule_the_docs_state_and_upstream_does_not_enforce() {
        for good in ["release-notes", "a", "x1-y2"] {
            assert!(validate_name(good).is_ok(), "{good}");
        }
        for bad in ["", "Release", "a--b", "-a", "a-", "a_b", "a/b"] {
            assert!(validate_name(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn a_description_with_markup_is_refused() {
        let dir = scratch("markup").join("release-notes");
        package(
            &dir,
            "---\nname: release-notes\ndescription: <b>do it</b>\n---\n\nBody.\n",
        );
        let error = read_package(&SkillSource::Dir(dir)).expect_err("markup must be refused");
        assert!(error.to_string().contains("without escaping"), "{error}");
    }

    #[test]
    fn a_binary_asset_is_refused_with_a_reason() {
        let dir = scratch("binary").join("release-notes");
        package(&dir, GOOD);
        std::fs::write(dir.join("logo.bin"), [0xff, 0xfe, 0x00, 0x01]).unwrap();
        let error = read_package(&SkillSource::Dir(dir)).expect_err("binary must be refused");
        assert!(error.to_string().contains("UTF-8"), "{error}");
    }
}
