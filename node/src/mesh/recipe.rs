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

/// The least time an offer is staged for: one lapsing sooner is refused.
pub const MIN_OFFER_TTL_MS: i64 = 3600 * 1000;

/// How many undecided offers one sender may have staged here at once. A
/// newer offer for the same repository replaces its older one, so this
/// bounds a peer offering many repositories, not one offered often.
pub const MAX_PENDING_PER_SENDER: i64 = 32;

const MAX_COMMANDS: usize = 64;
const MAX_COMMAND_BYTES: usize = 4096;
const MAX_NOTE_CHARS: usize = 1000;
/// The most a recipe may take, as JSON: well past any real one.
const MAX_RECIPE_BYTES: usize = 64 * 1024;
/// The most of a peer's node name an operator is shown in one line.
const MAX_NAME_CHARS: usize = 40;

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

/// How the operator asked for an offer to be applied.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Choices {
    /// Build the offered Dockerfile in place of an image this node pinned.
    pub replace_image: bool,
    /// Keep lending the entry's `prepare_credentials` to a preparation the
    /// offer changes. Off unless asked for: see [`Credentials::Dropped`].
    pub keep_credentials: bool,
    /// Not the operator's choice but the entry's state: it is [`held`], so
    /// its preparation is a peer's the operator has not started, and an
    /// offer that leaves it as it is does not make it any less so.
    pub held: bool,
}

/// What becomes of the credentials an entry lends its default branch's
/// preparation when an offer is applied to it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Credentials {
    /// The entry lends none.
    None,
    /// The offer leaves what preparation runs and reaches as it was, so the
    /// credentials are lent to the same commands as before.
    Unaffected(Vec<String>),
    /// The offer changes the commands, the hosts they reach or the image
    /// they run in, so a peer would choose what runs with the credentials
    /// and where it may send them. They are no longer lent; the operator
    /// re-adds them at the node once they have read what would run.
    Dropped(Vec<String>),
    /// The operator chose to keep lending them to the changed preparation.
    /// The node then builds nothing until the operator starts it.
    KeptOnChange(Vec<String>),
    /// The entry was held and the offer leaves its preparation as it is: it
    /// stays held, building nothing until the operator starts it.
    StillHeld(Vec<String>),
}

impl Credentials {
    pub fn names(&self) -> &[String] {
        match self {
            Credentials::None => &[],
            Credentials::Unaffected(n)
            | Credentials::Dropped(n)
            | Credentials::KeptOnChange(n)
            | Credentials::StillHeld(n) => n,
        }
    }

    /// Whether a build may start on its own once the offer is written: not
    /// while credentials are lent to a peer's preparation the operator has
    /// not started.
    pub fn allows_build(&self) -> bool {
        !self.holds()
    }

    /// Whether the entry is held once the offer is written.
    pub fn holds(&self) -> bool {
        matches!(
            self,
            Credentials::KeptOnChange(_) | Credentials::StillHeld(_)
        )
    }
}

/// An entry with an offer applied, and what the operator should know about
/// how it was applied.
#[derive(Debug, Clone)]
pub struct Applied {
    pub entry: Repo,
    pub notes: Vec<String>,
    /// The fields in [`LOCAL_ONLY`] the entry still has its own value for.
    pub kept: Vec<&'static str>,
    pub credentials: Credentials,
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
        if value.to_string().len() > MAX_RECIPE_BYTES {
            return Err(format!(
                "the recipe is larger than {} KiB",
                MAX_RECIPE_BYTES / 1024
            ));
        }
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
        for (field, value) in [
            ("dockerfile", self.dockerfile.as_deref()),
            ("context", self.context.as_deref()),
        ] {
            if value.is_some_and(|v| hidden_char(v).is_some()) {
                return Err(format!("{field} has a control or invisible character"));
            }
        }
        for entry in &self.egress {
            if hidden_char(entry).is_some() {
                return Err("egress has a control or invisible character".into());
            }
            if address_like(entry) {
                return Err(format!(
                    "egress {entry:?} is an IP address; a recipe names registries by host name"
                ));
            }
        }
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
                if command.len() > MAX_COMMAND_BYTES {
                    return Err(format!("{field} has a command that is too long"));
                }
                // What the operator reads must be what runs: no control
                // characters, and nothing that reorders or hides text.
                if let Some(c) = hidden_char(command) {
                    return Err(format!(
                        "{field} has a command with a control or invisible character (U+{:04X})",
                        c as u32
                    ));
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
    /// fields in [`LOCAL_ONLY`] stay as the entry has them, with two
    /// exceptions the operator controls. A recipe that names a Dockerfile
    /// replaces an image this node pinned only when `replace_image` says so
    /// (one that names none leaves the entry's own image source alone: its
    /// sender pins an image, which is never offered). And credentials the
    /// entry lends its preparation are dropped when the offer changes what
    /// that preparation runs, reaches or runs in, unless `keep_credentials`.
    pub fn apply(&self, existing: Option<&Repo>, path: PathBuf, choices: Choices) -> Applied {
        let mut notes = Vec::new();
        let was = existing.cloned().unwrap_or_default();
        let mut entry = was.clone();
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
            (Some(_), true) if !choices.replace_image => notes.push(
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
        let lent = entry.prepare_credentials.clone();
        let preparation_changed = was.prepare != entry.prepare
            || was.egress != entry.egress
            || was.dockerfile != entry.dockerfile
            || was.context != entry.context;
        let credentials = if lent.is_empty() {
            Credentials::None
        } else if !preparation_changed && choices.held {
            notes.push(format!(
                "This entry is held: it keeps lending {} to a peer's preparation you have not \
                 started, and the offer leaves that preparation as it is. It stays held; nothing \
                 is built until you start it.",
                lent.join(", ")
            ));
            Credentials::StillHeld(lent)
        } else if !preparation_changed {
            notes.push(format!(
                "This node keeps lending {} to the default branch's preparation, which the offer \
                 leaves as it was.",
                lent.join(", ")
            ));
            Credentials::Unaffected(lent)
        } else if choices.keep_credentials {
            notes.push(format!(
                "This node keeps lending {} to a preparation the offer changes. Nothing is built \
                 until you start it.",
                lent.join(", ")
            ));
            Credentials::KeptOnChange(lent)
        } else {
            entry.prepare_credentials.clear();
            notes.push(format!(
                "Credentials no longer lent: {}. The offer changes what preparation runs or \
                 reaches; re-add them at the node once you have read it.",
                lent.join(", ")
            ));
            Credentials::Dropped(lent)
        };
        let kept = LOCAL_ONLY
            .iter()
            .copied()
            .filter(|field| match *field {
                "path" => existing.is_some(),
                "image" => entry.image.is_some(),
                "prepare_credentials" => !entry.prepare_credentials.is_empty(),
                "commits" => entry.commits.is_some(),
                "style" => entry.style.is_some(),
                _ => false,
            })
            .collect();
        Applied {
            entry,
            notes,
            kept,
            credentials,
        }
    }
}

/// The fields `before` and `after` differ in, as `{field, before, after}`.
pub fn changes(before: Option<&Repo>, after: &Repo) -> Vec<Value> {
    let blank = Repo::default();
    let was = before.unwrap_or(&blank);
    let pairs = [
        ("image", json!(was.image), json!(after.image)),
        (
            "prepare_credentials",
            json!(was.prepare_credentials),
            json!(after.prepare_credentials),
        ),
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

/// Whether an egress entry is, or could be read as, an address rather than a
/// host name: anything `inet_aton` accepts (one to four parts, each decimal,
/// octal or `0x` hex, as in `127.1` or `0x7f.0.0.1`), or a last label of
/// digits, which no top-level domain is. An address names a
/// machine on someone's network, not a registry.
pub fn address_like(host: &str) -> bool {
    let host = host.trim().trim_end_matches('.').to_ascii_lowercase();
    if host.contains(':') || host.parse::<std::net::IpAddr>().is_ok() {
        return true;
    }
    let labels: Vec<&str> = host.split('.').collect();
    let numeric = |label: &str| {
        !label.is_empty()
            && (label.bytes().all(|b| b.is_ascii_digit())
                || label
                    .strip_prefix("0x")
                    .is_some_and(|hex| hex.bytes().all(|b| b.is_ascii_hexdigit())))
    };
    (labels.len() <= 4 && labels.iter().all(|l| numeric(l)))
        || labels
            .last()
            .is_some_and(|l| l.bytes().all(|b| b.is_ascii_digit()))
}

/// A peer's node name as this node shows it to its operator: without
/// control, format or separator characters, and short enough that what
/// follows it on a line is still seen.
pub fn display_name(name: &str) -> String {
    let clean = without_hidden(name);
    let clean = clean.trim();
    if clean.chars().count() > MAX_NAME_CHARS {
        let cut: String = clean.chars().take(MAX_NAME_CHARS - 1).collect();
        format!("{}…", cut.trim_end())
    } else {
        clean.to_string()
    }
}

/// `text` without control, format or separator characters: how a name a
/// peer chose for itself is recorded and shown.
pub fn without_hidden(text: &str) -> String {
    text.chars()
        .filter(|&c| !(c.is_control() || is_format(c) || c == '\u{2028}' || c == '\u{2029}'))
        .collect()
}

/// What a hold is on: the commands credentials are lent to, and the
/// credentials. An operator who rewrites either at the node has decided what
/// runs with what, and the hold lapses; one who narrows the egress or changes
/// the Dockerfile has not, and it stands.
pub fn lending_hash(entry: &Repo) -> String {
    use sha2::{Digest, Sha256};
    let lent = json!({
        "prepare": entry.prepare,
        "prepare_credentials": entry.prepare_credentials,
    });
    hex::encode(Sha256::digest(lent.to_string().as_bytes()))
}

/// The hold on `entry`, if it keeps lending credentials to a preparation a
/// peer changed and the operator has not yet started its build. Read by
/// everything that would build its image or warm its cache with them.
pub fn held(
    store: &crate::store::Store,
    entry: &Repo,
) -> Option<crate::store::recipe_offers::CredentialHoldRow> {
    if entry.prepare_credentials.is_empty() {
        return None;
    }
    store
        .credential_hold(&entry.path.to_string_lossy())
        .ok()
        .flatten()
        .filter(|hold| hold.lending_hash == lending_hash(entry))
}

/// What a held entry's runs say instead of building.
pub fn held_reason(hold: &crate::store::recipe_offers::CredentialHoldRow) -> String {
    format!(
        "held: {} is lent to a preparation a peer's recipe changed, so nothing is built or \
         warmed for {} until you start its build in Settings, Repositories",
        hold.credentials.join(", "),
        hold.path
    )
}

/// An operator's note on an offer: trimmed, empty as none, and refused when
/// it is too long or carries a control or invisible character.
pub fn note(note: Option<String>) -> Result<Option<String>, String> {
    let Some(note) = note else {
        return Ok(None);
    };
    // Checked before trimming, which would take a trailing separator.
    if let Some(c) = hidden_char(&note) {
        return Err(format!(
            "the note has a control or invisible character (U+{:04X})",
            c as u32
        ));
    }
    let note = note.trim();
    if note.chars().count() > MAX_NOTE_CHARS {
        return Err(format!(
            "the note is longer than {MAX_NOTE_CHARS} characters"
        ));
    }
    Ok((!note.is_empty()).then(|| note.to_string()))
}

/// The first character in `text` that is a control (Unicode Cc), a format
/// character (Cf: bidirectional overrides and isolates, zero-width joiners
/// and spaces, the byte-order mark, tag characters) or a line or paragraph
/// separator (Zl, Zp). An operator reading a command must see what runs.
pub fn hidden_char(text: &str) -> Option<char> {
    text.chars()
        .find(|&c| c.is_control() || is_format(c) || c == '\u{2028}' || c == '\u{2029}')
}

/// Unicode general category Cf, as of Unicode 15.1.
fn is_format(c: char) -> bool {
    matches!(c as u32,
        0x00AD
        | 0x0600..=0x0605
        | 0x061C
        | 0x06DD
        | 0x070F
        | 0x0890..=0x0891
        | 0x08E2
        | 0x180E
        | 0x200B..=0x200F
        | 0x202A..=0x202E
        | 0x2060..=0x2064
        | 0x2066..=0x206F
        | 0xFEFF
        | 0xFFF9..=0xFFFB
        | 0x110BD
        | 0x110CD
        | 0x13430..=0x1343F
        | 0x1BCA0..=0x1BCA3
        | 0x1D173..=0x1D17A
        | 0xE0001
        | 0xE0020..=0xE007F)
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

    fn offered() -> RepoRecipe {
        RepoRecipe {
            repo: "github.com/acme/app".into(),
            dockerfile: Some("Dockerfile".into()),
            context: None,
            checks: Some(vec!["just all".into()]),
            prepare: vec!["npm ci".into()],
            egress: vec!["npm".into()],
            session_egress: false,
            timeout_secs: None,
        }
    }

    const KEEP_IMAGE: Choices = Choices {
        replace_image: false,
        keep_credentials: false,
        held: false,
    };

    #[test]
    fn applying_keeps_what_is_this_nodes_own() {
        let recipe = offered();
        let mut mine = entry();
        let applied = recipe.apply(Some(&mine), mine.path.clone(), KEEP_IMAGE);
        assert_eq!(applied.entry.path, mine.path);
        assert!(matches!(
            applied.entry.commits,
            Some(crate::config::Commits::Keep)
        ));
        assert_eq!(applied.entry.dockerfile.as_deref(), Some("Dockerfile"));
        assert_eq!(applied.entry.checks, Some(vec!["just all".to_string()]));
        assert!(!applied.entry.session_egress);
        assert_eq!(applied.kept, ["path", "commits"]);

        // A pinned image stays unless the operator replaces it.
        mine.image = Some(format!("localhost/app@sha256:{}", "b".repeat(64)));
        mine.dockerfile = None;
        mine.context = None;
        mine.prepare_credentials.clear();
        let kept = recipe.apply(Some(&mine), mine.path.clone(), KEEP_IMAGE);
        assert_eq!(kept.entry.image, mine.image);
        assert_eq!(kept.entry.dockerfile, None);
        assert!(kept.notes.iter().any(|n| n.contains("pins an image")));
        assert!(kept.kept.contains(&"image"));
        let replace = Choices {
            replace_image: true,
            ..KEEP_IMAGE
        };
        let replaced = recipe.apply(Some(&mine), mine.path.clone(), replace);
        assert_eq!(replaced.entry.image, None);
        assert_eq!(replaced.entry.dockerfile.as_deref(), Some("Dockerfile"));
        assert!(!replaced.kept.contains(&"image"));

        // An offer with no Dockerfile leaves the entry's own source.
        let no_build = RepoRecipe {
            dockerfile: None,
            ..recipe
        };
        let applied = no_build.apply(Some(&mine), mine.path.clone(), replace);
        assert_eq!(applied.entry.image, mine.image);
        let fresh = no_build.apply(None, "github.com/acme/app".into(), KEEP_IMAGE);
        assert_eq!(fresh.entry.path, PathBuf::from("github.com/acme/app"));
        assert!(fresh.entry.prepare_credentials.is_empty());
        assert!(fresh.kept.is_empty());
    }

    /// A peer that changes what preparation runs or reaches would choose
    /// what runs with the credentials it is lent: they are dropped unless the
    /// operator keeps them, and kept ones stop the node building on its own.
    #[test]
    fn credentials_are_not_lent_to_a_preparation_a_peer_changed() {
        let mine = entry();
        let dropped = offered().apply(Some(&mine), mine.path.clone(), KEEP_IMAGE);
        assert!(dropped.entry.prepare_credentials.is_empty());
        assert_eq!(
            dropped.credentials,
            Credentials::Dropped(vec!["nova".into()])
        );
        assert!(dropped.credentials.allows_build());
        assert!(dropped.notes.iter().any(|n| n.contains("no longer lent")));
        assert!(changes(Some(&mine), &dropped.entry)
            .iter()
            .any(|c| c["field"] == "prepare_credentials"));

        let keep = Choices {
            keep_credentials: true,
            ..KEEP_IMAGE
        };
        let kept = offered().apply(Some(&mine), mine.path.clone(), keep);
        assert_eq!(kept.entry.prepare_credentials, ["nova"]);
        assert_eq!(
            kept.credentials,
            Credentials::KeptOnChange(vec!["nova".into()])
        );
        assert!(!kept.credentials.allows_build());

        // Only checks changed: the preparation they are lent to is the same.
        let same = RepoRecipe {
            checks: Some(vec!["just all".into()]),
            ..RepoRecipe::from_entry(&mine, "github.com/acme/app")
        };
        let unaffected = same.apply(Some(&mine), mine.path.clone(), KEEP_IMAGE);
        assert_eq!(unaffected.entry.prepare_credentials, ["nova"]);
        assert_eq!(
            unaffected.credentials,
            Credentials::Unaffected(vec!["nova".into()])
        );
        assert!(unaffected.credentials.allows_build());
        for (field, changed) in [
            (
                "egress",
                RepoRecipe {
                    egress: vec!["npm".into()],
                    ..same.clone()
                },
            ),
            (
                "dockerfile",
                RepoRecipe {
                    dockerfile: Some("Other".into()),
                    ..same.clone()
                },
            ),
            (
                "context",
                RepoRecipe {
                    context: Some("sub".into()),
                    ..same.clone()
                },
            ),
        ] {
            let applied = changed.apply(Some(&mine), mine.path.clone(), KEEP_IMAGE);
            assert!(applied.entry.prepare_credentials.is_empty(), "{field}");
        }
    }

    /// An offer onto a held entry that leaves its preparation as it is does
    /// not release it.
    #[test]
    fn a_held_entry_stays_held_when_the_preparation_is_unchanged() {
        let mine = entry();
        let same = RepoRecipe {
            checks: Some(vec!["just all".into()]),
            timeout_secs: Some(5),
            ..RepoRecipe::from_entry(&mine, "github.com/acme/app")
        };
        let held = Choices {
            held: true,
            ..KEEP_IMAGE
        };
        let applied = same.apply(Some(&mine), mine.path.clone(), held);
        assert_eq!(
            applied.credentials,
            Credentials::StillHeld(vec!["nova".into()])
        );
        assert!(!applied.credentials.allows_build());
        assert_eq!(applied.entry.prepare_credentials, ["nova"]);
        // Changed, it is dropped unless kept, as for any entry.
        let changed = offered().apply(Some(&mine), mine.path.clone(), held);
        assert_eq!(
            changed.credentials,
            Credentials::Dropped(vec!["nova".into()])
        );
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
        assert_eq!(note(Some("  hi  ".into())).unwrap().as_deref(), Some("hi"));
        assert_eq!(note(Some("   ".into())).unwrap(), None);
        assert!(note(Some("x".repeat(5000))).is_err());
        for hidden in [
            "a\u{7}b",
            "two\nlines",
            "rtl\u{202E}txt",
            "zero\u{200B}width",
            "sep\u{2028}",
        ] {
            assert!(note(Some(hidden.into())).is_err(), "{hidden:?}");
        }
    }

    #[test]
    fn anything_inet_aton_reads_as_an_address_is_refused() {
        for address in [
            "10.0.0.1",
            "127.1",
            "2130706433",
            "0x7f.0.0.1",
            "0x7f000001",
            "017700000001",
            "0177.0.0.1",
            "169.254.169.254.",
            "evil.example.123",
        ] {
            assert!(address_like(address), "{address}");
        }
        for host in [
            "registry.npmjs.org",
            "cafe.be",
            "nova.laravel.com",
            "1password.com",
            "a1.b2.example",
        ] {
            assert!(!address_like(host), "{host}");
        }
        let mut recipe = offered();
        recipe.egress = vec!["0x7f.1".into()];
        assert!(recipe.validate().unwrap_err().contains("IP address"));
    }

    #[test]
    fn a_peer_name_is_shown_clean_and_short() {
        assert_eq!(display_name("laptop"), "laptop");
        assert_eq!(display_name("lap\u{202E}top\u{200B}\n"), "laptop");
        let long = display_name(&"x".repeat(200));
        assert_eq!(long.chars().count(), MAX_NAME_CHARS);
        assert!(long.ends_with('…'));
    }

    #[test]
    fn an_oversized_recipe_is_refused() {
        let mut wire = serde_json::to_value(offered()).unwrap();
        wire["prepare"] = json!(vec!["x".repeat(3000); 40]);
        assert!(RepoRecipe::parse(&wire).unwrap_err().contains("KiB"));
    }

    /// A hold is on what the credentials would be lent to: an entry edited
    /// since, or one that lends nothing, is not held.
    #[test]
    fn a_hold_covers_the_entry_as_it_was_kept() {
        let store = crate::store::Store::open_in_memory().unwrap();
        let mine = entry();
        store
            .credential_hold_put(&crate::store::recipe_offers::CredentialHoldRow {
                path: mine.path.to_string_lossy().into(),
                offer_id: "o".into(),
                sender_node: "n".into(),
                repo: "github.com/acme/app".into(),
                credentials: vec!["nova".into()],
                lending_hash: lending_hash(&mine),
                created_ms: 1,
            })
            .unwrap();
        let hold = held(&store, &mine).unwrap();
        assert!(held_reason(&hold).starts_with("held: nova"));
        let mut edited = mine.clone();
        edited.prepare.push("npm test".into());
        assert!(held(&store, &edited).is_none());
        // Narrowing what it reaches, or the image, is not deciding what runs.
        let mut narrowed = mine.clone();
        narrowed.egress = vec!["npm".into()];
        narrowed.dockerfile = Some("Other".into());
        assert!(held(&store, &narrowed).is_some());
        let mut lends_nothing = mine.clone();
        lends_nothing.prepare_credentials.clear();
        assert!(held(&store, &lends_nothing).is_none());
        assert!(store
            .credential_hold_release(&mine.path.to_string_lossy())
            .unwrap());
        assert!(held(&store, &mine).is_none());
    }

    /// What the operator reads must be what runs: a command, an egress entry
    /// or a path with a control, bidirectional or zero-width character is
    /// refused, and so is an egress entry that is an address.
    #[test]
    fn hidden_characters_and_addresses_are_refused() {
        for hidden in [
            "npm ci\u{202E}",
            "npm\u{200B} ci",
            "npm ci\ttest",
            "npm ci\r",
            "npm\u{2066}ci\u{2069}",
            "npm\u{FEFF}ci",
            "npm\u{E0041}ci",
            "npm\u{2029}ci",
        ] {
            let mut recipe = offered();
            recipe.prepare = vec![hidden.into()];
            assert!(
                recipe.validate().unwrap_err().contains("invisible"),
                "{hidden:?}"
            );
            let mut checks = offered();
            checks.checks = Some(vec![hidden.into()]);
            assert!(checks.validate().is_err(), "{hidden:?}");
        }
        let mut egress = offered();
        egress.egress = vec!["registry.npmjs.org\u{200D}".into()];
        assert!(egress.validate().is_err());
        let mut dockerfile = offered();
        dockerfile.dockerfile = Some("Docker\u{202E}file".into());
        assert!(dockerfile.validate().is_err());
        for address in ["10.0.0.1", "169.254.169.254", "127.0.0.1"] {
            let mut recipe = offered();
            recipe.egress = vec![address.into()];
            assert!(
                recipe.validate().unwrap_err().contains("IP address"),
                "{address}"
            );
        }
        // Plain text, including non-Latin letters, is fine.
        let mut fine = offered();
        fine.prepare = vec!["echo 'héllo wörld'".into()];
        fine.validate().unwrap();
        assert_eq!(hidden_char("plain"), None);
    }
}
