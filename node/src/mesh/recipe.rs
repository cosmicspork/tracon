//! Repository recipes offered between nodes.
//!
//! A `[[repo]]` entry is the operator's, and most of it is portable: how the
//! repository's image is built, what prepares it, which checks are required,
//! and what preparation may reach. Some of it is not, and never leaves the
//! node: the image a node pinned (its digest names what *this* node built or
//! chose), the credentials it lends to preparation, the path of its own
//! checkout, and how its commits reach the forge. A recipe is the portable
//! part, named by the repository's forge identity (`github.com/owner/name`)
//! rather than any path, and a receiving node stages it for its operator,
//! who sees what would change and accepts or declines it. An accepted recipe
//! is mapped onto that node's own clone, and the node builds and pins its own
//! image from it.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::config::Repo;

/// How long an offer waits for its operator before it lapses.
pub const OFFER_TTL_MS: i64 = 14 * 24 * 3600 * 1000;

/// How many undecided offers one sender may have staged here at once. A
/// newer offer for the same repository replaces its older one, so this
/// bounds a peer offering many repositories, not one offered often.
pub const MAX_PENDING_PER_SENDER: i64 = 32;

const MAX_COMMANDS: usize = 64;
const MAX_COMMAND_BYTES: usize = 4096;
const MAX_NOTE_CHARS: usize = 1000;

/// The fields of a `[[repo]]` entry that are this node's alone and are never
/// offered: kept as they are when an offer is accepted onto the entry.
pub const LOCAL_ONLY: &[&str] = &["path", "image", "prepare_credentials", "commits", "style"];

/// What one node offers another for a repository. Unknown fields are refused
/// on receipt, so a sender cannot slip an image, a digest or a credential
/// name past a receiver that would not have shown it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RepoRecipe {
    /// The repository's forge identity, `host/owner/name`.
    pub repo: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dockerfile: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checks: Option<Vec<String>>,
    #[serde(default)]
    pub prepare: Vec<String>,
    #[serde(default)]
    pub egress: Vec<String>,
    #[serde(default)]
    pub session_egress: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_secs: Option<u64>,
}

/// An entry with an offer applied, and what the operator should know about
/// how it was applied.
#[derive(Debug, Clone)]
pub struct Applied {
    pub entry: Repo,
    pub notes: Vec<String>,
}

impl RepoRecipe {
    /// The portable part of `entry`, offered as `repo`.
    pub fn from_entry(entry: &Repo, repo: &str) -> Self {
        Self {
            repo: repo.to_string(),
            dockerfile: entry.dockerfile.clone(),
            context: entry.context.clone(),
            checks: entry.checks.clone(),
            prepare: entry.prepare.clone(),
            egress: entry.egress.clone(),
            session_egress: entry.session_egress,
            timeout_secs: entry.timeout_secs,
        }
    }

    /// A recipe as a peer sent it: only the fields a recipe has, and only
    /// values that pass [`validate`](Self::validate).
    pub fn parse(value: &Value) -> Result<Self, String> {
        if let Some(object) = value.as_object() {
            for field in object.keys() {
                if LOCAL_ONLY.contains(&field.as_str()) || field.contains("digest") {
                    return Err(format!(
                        "the recipe carries `{field}`, which is never shared between nodes"
                    ));
                }
            }
        }
        let recipe: Self = serde_json::from_value(value.clone())
            .map_err(|error| format!("the recipe is not one this node reads: {error}"))?;
        recipe.validate()?;
        Ok(recipe)
    }

    /// Refuse a recipe a node could not honour, or one that would carry
    /// something of the sender's machine to the receiver's.
    pub fn validate(&self) -> Result<(), String> {
        valid_identity(&self.repo)?;
        for (field, commands) in [
            ("checks", self.checks.as_deref().unwrap_or_default()),
            ("prepare", &self.prepare[..]),
        ] {
            if commands.len() > MAX_COMMANDS {
                return Err(format!("{field} has more than {MAX_COMMANDS} commands"));
            }
            for command in commands {
                if command.trim().is_empty() {
                    return Err(format!("{field} has an empty command"));
                }
                if command.len() > MAX_COMMAND_BYTES || command.contains('\0') {
                    return Err(format!("{field} has a command that is too long"));
                }
                if let Some(path) = local_path(command) {
                    return Err(format!(
                        "{field} names {path}, a path on one node's machine; a recipe names \
                         paths inside the repository or the image, never a home directory"
                    ));
                }
            }
        }
        if self.egress.len() > MAX_COMMANDS {
            return Err(format!("egress has more than {MAX_COMMANDS} entries"));
        }
        // Everything else a `[[repo]]` entry is held to: a Dockerfile and
        // context inside the repository, egress that is a preset or a host.
        crate::config::validate_repos(&[self.entry_at(PathBuf::from(&self.repo))])
            .map_err(|error| error.replacen(&format!("repo {}: ", self.repo), "", 1))
    }

    fn entry_at(&self, path: PathBuf) -> Repo {
        Repo {
            path,
            dockerfile: self.dockerfile.clone(),
            context: self.context.clone(),
            checks: self.checks.clone(),
            prepare: self.prepare.clone(),
            egress: self.egress.clone(),
            session_egress: self.session_egress,
            timeout_secs: self.timeout_secs,
            ..Default::default()
        }
    }

    /// `existing` (or a new entry at `path`) with this recipe applied. The
    /// fields in [`LOCAL_ONLY`] stay as the entry has them. A recipe that
    /// names no Dockerfile leaves the entry's own image source alone: its
    /// sender pins an image, which is never offered. One that names a
    /// Dockerfile replaces an image this node pinned only when
    /// `replace_image` says so.
    pub fn apply(&self, existing: Option<&Repo>, path: PathBuf, replace_image: bool) -> Applied {
        let mut notes = Vec::new();
        let mut entry = existing.cloned().unwrap_or_default();
        entry.path = path;
        entry.checks = self.checks.clone();
        entry.prepare = self.prepare.clone();
        entry.egress = self.egress.clone();
        entry.session_egress = self.session_egress;
        entry.timeout_secs = self.timeout_secs;
        match (&self.dockerfile, entry.image.is_some()) {
            (None, _) => {
                if entry.image.is_some() || entry.dockerfile.is_some() {
                    notes.push(
                        "The offer names no Dockerfile, so this entry keeps its own image source."
                            .into(),
                    );
                }
            }
            (Some(_), true) if !replace_image => notes.push(
                "This entry pins an image of its own, which it keeps; the offered Dockerfile is \
                 not applied unless the pinned image is replaced."
                    .into(),
            ),
            (Some(dockerfile), _) => {
                if entry.image.take().is_some() {
                    notes.push(
                        "The pinned image is replaced: this node builds the offered Dockerfile \
                         and pins its own image."
                            .into(),
                    );
                }
                entry.dockerfile = Some(dockerfile.clone());
                entry.context = self.context.clone();
            }
        }
        if !entry.prepare_credentials.is_empty() {
            notes.push(format!(
                "This node keeps lending {} to the default branch's preparation.",
                entry.prepare_credentials.join(", ")
            ));
        }
        Applied { entry, notes }
    }
}

/// The fields `before` and `after` differ in, as `{field, before, after}`.
pub fn changes(before: Option<&Repo>, after: &Repo) -> Vec<Value> {
    let blank = Repo::default();
    let was = before.unwrap_or(&blank);
    let pairs = [
        ("image", json!(was.image), json!(after.image)),
        ("dockerfile", json!(was.dockerfile), json!(after.dockerfile)),
        ("context", json!(was.context), json!(after.context)),
        ("checks", json!(was.checks), json!(after.checks)),
        ("prepare", json!(was.prepare), json!(after.prepare)),
        ("egress", json!(was.egress), json!(after.egress)),
        (
            "session_egress",
            json!(was.session_egress),
            json!(after.session_egress),
        ),
        (
            "timeout_secs",
            json!(was.timeout_secs),
            json!(after.timeout_secs),
        ),
    ];
    pairs
        .into_iter()
        .filter(|(_, before, after)| before != after)
        .map(|(field, before, after)| json!({ "field": field, "before": before, "after": after }))
        .collect()
}

/// Whether `repo` is a forge identity as `canonical_remote` spells one:
/// a lower-case host name, then at least an owner and a name, every segment
/// a plain path component.
pub fn valid_identity(repo: &str) -> Result<(), String> {
    let refuse = || {
        Err(format!(
            "a recipe names its repository by its forge identity (host/owner/name, like \
             github.com/owner/name), not {repo:?}"
        ))
    };
    let segments: Vec<&str> = repo.split('/').collect();
    if repo.len() > 512 || !(3..=8).contains(&segments.len()) {
        return refuse();
    }
    let host = segments[0];
    let host_ok = host.contains('.')
        && host.split('.').all(|label| {
            !label.is_empty()
                && label
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        });
    let rest_ok = segments[1..].iter().all(|part| {
        !part.is_empty()
            && !part.starts_with('.')
            && part
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    });
    if host_ok && rest_ok {
        Ok(())
    } else {
        refuse()
    }
}

/// The forge identity a `[[repo]]` path names by itself, if it does: a
/// relative path that is one (`github.com/owner/name`).
pub fn identity_of_path(path: &Path) -> Option<String> {
    let text = path.to_str()?;
    (!path.is_absolute() && valid_identity(text).is_ok()).then(|| text.to_string())
}

/// A path in `command` that only makes sense on one machine: under a home
/// directory, or under this node's own home or state directory.
fn local_path(command: &str) -> Option<String> {
    const HOMES: &[&str] = &["/home/", "/var/home/", "/Users/", "/root/"];
    let mut own: Vec<String> = Vec::new();
    for dir in [
        std::env::var_os("HOME").map(PathBuf::from),
        Some(crate::config::Config::state_dir()),
    ]
    .into_iter()
    .flatten()
    {
        let dir = dir.to_string_lossy().trim_end_matches('/').to_string();
        // `/` or a one-component directory would match far too much.
        if dir.matches('/').count() >= 2 {
            own.push(dir);
        }
    }
    command
        .split(|c: char| c.is_whitespace() || "=:'\"();,`".contains(c))
        .find(|token| {
            *token == "~"
                || *token == "/root"
                || token.starts_with("~/")
                || HOMES.iter().any(|home| token.starts_with(home))
                || own.iter().any(|dir| {
                    token
                        .strip_prefix(dir.as_str())
                        .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
                })
        })
        .map(str::to_string)
}

/// An operator's note on an offer, as it is shown: trimmed, without control
/// characters, and bounded.
pub fn clean_note(note: Option<String>) -> Option<String> {
    let note: String = note?
        .chars()
        .filter(|c| !c.is_control() || *c == '\n')
        .take(MAX_NOTE_CHARS)
        .collect();
    let note = note.trim();
    (!note.is_empty()).then(|| note.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry() -> Repo {
        Repo {
            path: "/var/home/op/src/app".into(),
            image: None,
            dockerfile: Some(".devcontainer/Dockerfile".into()),
            context: Some(".".into()),
            checks: Some(vec!["just check".into()]),
            timeout_secs: Some(1200),
            prepare: vec!["composer install".into(), "npm ci".into()],
            egress: vec!["npm".into(), "packagist".into(), "nova.laravel.com".into()],
            session_egress: true,
            prepare_credentials: vec!["nova".into()],
            commits: Some(crate::config::Commits::Keep),
            style: None,
        }
    }

    #[test]
    fn a_recipe_carries_no_image_credential_path_or_publication_setting() {
        let mut pinned = entry();
        pinned.image = Some(format!("localhost/app@sha256:{}", "a".repeat(64)));
        let recipe = RepoRecipe::from_entry(&pinned, "github.com/acme/app");
        let wire = serde_json::to_value(&recipe).unwrap();
        let text = wire.to_string();
        for leaked in [
            "sha256",
            "localhost/app",
            "nova\"",
            "/var/home",
            "prepare_credentials",
            "commits",
            "path",
        ] {
            assert!(!text.contains(leaked), "{leaked} leaked into {text}");
        }
        assert_eq!(wire["repo"], "github.com/acme/app");
        assert_eq!(wire["dockerfile"], ".devcontainer/Dockerfile");
        assert_eq!(RepoRecipe::parse(&wire).unwrap(), recipe);
    }

    #[test]
    fn a_recipe_with_a_local_field_is_refused_on_receipt() {
        let recipe = RepoRecipe::from_entry(&entry(), "github.com/acme/app");
        for (field, value) in [
            ("image", json!("localhost/app@sha256:00")),
            ("image_digest", json!("sha256:00")),
            ("prepare_credentials", json!(["nova"])),
            ("path", json!("/var/home/op/src/app")),
            ("commits", json!("keep")),
            ("something_new", json!(1)),
        ] {
            let mut wire = serde_json::to_value(&recipe).unwrap();
            wire[field] = value;
            let refused = RepoRecipe::parse(&wire).unwrap_err();
            assert!(
                refused.contains(field) || refused.contains("unknown field"),
                "{field}: {refused}"
            );
        }
    }

    #[test]
    fn a_recipe_names_a_forge_identity_and_no_machine_path() {
        for bad in [
            "/var/home/op/src/app",
            "acme/app",
            "app",
            "GitHub.com/acme/app",
            "github.com/../app",
            "github.com/acme/.hidden",
            "github.com//app",
            "~/src/app",
        ] {
            let mut recipe = RepoRecipe::from_entry(&entry(), "github.com/acme/app");
            recipe.repo = bad.into();
            assert!(recipe.validate().is_err(), "{bad} was accepted");
        }
        for good in ["github.com/acme/app", "gitlab.example.org/group/sub/app"] {
            assert!(valid_identity(good).is_ok(), "{good}");
        }
        for (field, command) in [
            (
                "prepare",
                "composer install --working-dir=/var/home/op/src/app",
            ),
            ("prepare", "cp ~/.npmrc ."),
            ("checks", "/Users/op/bin/lint"),
            ("checks", "cat /root/token"),
        ] {
            let mut recipe = RepoRecipe::from_entry(&entry(), "github.com/acme/app");
            match field {
                "prepare" => recipe.prepare = vec![command.into()],
                _ => recipe.checks = Some(vec![command.into()]),
            }
            let refused = recipe.validate().unwrap_err();
            assert!(refused.contains(field), "{command}: {refused}");
        }
        let mut fine = RepoRecipe::from_entry(&entry(), "github.com/acme/app");
        fine.prepare = vec!["/usr/bin/env bash -c 'cargo fetch --locked'".into()];
        fine.validate().unwrap();
        let mut outside = fine.clone();
        outside.dockerfile = Some("../Dockerfile".into());
        assert!(outside.validate().unwrap_err().contains("dockerfile"));
        let mut egress = fine;
        egress.egress = vec!["https://evil.example".into()];
        assert!(egress.validate().unwrap_err().contains("egress"));
    }

    #[test]
    fn applying_keeps_what_is_this_nodes_own() {
        let recipe = RepoRecipe {
            repo: "github.com/acme/app".into(),
            dockerfile: Some("Dockerfile".into()),
            context: None,
            checks: Some(vec!["just all".into()]),
            prepare: vec!["npm ci".into()],
            egress: vec!["npm".into()],
            session_egress: false,
            timeout_secs: None,
        };
        let mut mine = entry();
        let applied = recipe.apply(Some(&mine), mine.path.clone(), false);
        assert_eq!(applied.entry.path, mine.path);
        assert_eq!(applied.entry.prepare_credentials, ["nova"]);
        assert!(matches!(
            applied.entry.commits,
            Some(crate::config::Commits::Keep)
        ));
        assert_eq!(applied.entry.dockerfile.as_deref(), Some("Dockerfile"));
        assert_eq!(applied.entry.checks, Some(vec!["just all".to_string()]));
        assert!(!applied.entry.session_egress);

        // A pinned image stays unless the operator replaces it.
        mine.image = Some(format!("localhost/app@sha256:{}", "b".repeat(64)));
        mine.dockerfile = None;
        mine.context = None;
        let kept = recipe.apply(Some(&mine), mine.path.clone(), false);
        assert_eq!(kept.entry.image, mine.image);
        assert_eq!(kept.entry.dockerfile, None);
        assert!(kept.notes.iter().any(|n| n.contains("pins an image")));
        let replaced = recipe.apply(Some(&mine), mine.path.clone(), true);
        assert_eq!(replaced.entry.image, None);
        assert_eq!(replaced.entry.dockerfile.as_deref(), Some("Dockerfile"));

        // An offer with no Dockerfile leaves the entry's own source.
        let no_build = RepoRecipe {
            dockerfile: None,
            ..recipe
        };
        let applied = no_build.apply(Some(&mine), mine.path.clone(), true);
        assert_eq!(applied.entry.image, mine.image);
        let fresh = no_build.apply(None, "github.com/acme/app".into(), false);
        assert_eq!(fresh.entry.path, PathBuf::from("github.com/acme/app"));
        assert!(fresh.entry.prepare_credentials.is_empty());
    }

    #[test]
    fn changes_name_only_what_differs() {
        let before = entry();
        let mut after = before.clone();
        after.prepare = vec!["npm ci".into()];
        let changed = changes(Some(&before), &after);
        assert_eq!(changed.len(), 1);
        assert_eq!(changed[0]["field"], "prepare");
        assert!(changes(None, &before).len() > 3);
    }

    #[test]
    fn a_note_is_trimmed_and_bounded() {
        assert_eq!(
            clean_note(Some("  \u{7}hi\n".into())).as_deref(),
            Some("hi")
        );
        assert_eq!(clean_note(Some("   ".into())), None);
        assert_eq!(
            clean_note(Some("x".repeat(5000))).unwrap().chars().count(),
            MAX_NOTE_CHARS
        );
    }
}
