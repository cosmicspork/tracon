//! What ships under the operator's name besides the tree: the branch and the
//! commit message.
//!
//! The candidate is the reviewed tree. The commits an agent made on the way to
//! it, and the branch it happened to be on, are not reviewed bytes; they are
//! prose, like the forge description, proposed by the agent and edited by the
//! operator. By default (`commits = "squash"`) publication pushes one commit
//! holding exactly the reviewed tree, carrying the approved message, on the
//! approved branch, so neither the candidate nor its evidence nor the verdict
//! changes. `keep` pushes the agent's commits, each with the message the
//! operator approved for it: as written unless they edited it, and then
//! with the same tree, author and dates.
//!
//! The rules here are deterministic and come from configuration: the
//! repository's entry, then the channel's bindings, then `[publish]`. A message
//! or branch that breaks them is refused when it is submitted, before the card
//! reaches the operator, and again when the operator edits it.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::config::{Commits, Config};
use crate::store::Store;

/// The commit types a conventional subject may open with when `types` names
/// none.
pub const CONVENTIONAL_TYPES: &[&str] = &[
    "feat", "fix", "docs", "style", "refactor", "perf", "test", "build", "ci", "chore", "revert",
];

/// Subject and branch rules. Every rule is off unless configured: a node that
/// says nothing about style checks nothing.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Style {
    /// `type(scope): subject`, with `type` one of `types`.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub conventional: bool,
    /// The types `conventional` allows. Empty means the usual set
    /// ([`CONVENTIONAL_TYPES`]).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub types: Vec<String>,
    /// The subject reads as a command: "add", not "added", "adds" or "adding".
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub imperative: bool,
    /// The longest subject line, in characters. 0 is no limit.
    #[serde(skip_serializing_if = "is_zero")]
    pub max_subject: usize,
    /// Lowercase words joined by `-`, segments by `/`: `feat/follow-requests`.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub kebab_branch: bool,
    /// No tracker keys (`PROJ-123`) in the subject or the branch.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub no_ticket_keys: bool,
}

fn is_zero(n: &usize) -> bool {
    *n == 0
}

impl Style {
    /// Every rule on, with the usual types and a 72-character subject.
    pub fn conventional() -> Self {
        Self {
            conventional: true,
            types: Vec::new(),
            imperative: true,
            max_subject: 72,
            kebab_branch: true,
            no_ticket_keys: true,
        }
    }

    /// What is wrong with `message`, one finding per broken rule.
    pub fn check_message(&self, message: &str) -> Vec<String> {
        let mut found = Vec::new();
        let subject = message.lines().next().unwrap_or("").trim();
        if subject.is_empty() {
            found.push("the commit message has no subject line".to_string());
            return found;
        }
        if self.max_subject > 0 && subject.chars().count() > self.max_subject {
            found.push(format!(
                "the subject is {} characters; the limit is {}",
                subject.chars().count(),
                self.max_subject
            ));
        }
        let mut text = subject;
        if self.conventional {
            match conventional_type(subject) {
                Some((kind, rest)) => {
                    let allowed = if self.types.is_empty() {
                        CONVENTIONAL_TYPES.contains(&kind)
                    } else {
                        self.types.iter().any(|t| t == kind)
                    };
                    if !allowed {
                        found.push(format!(
                            "`{kind}` is not a commit type this repository uses"
                        ));
                    }
                    text = rest;
                }
                None => found.push(
                    "the subject is not `type(scope): description` (conventional commits)"
                        .to_string(),
                ),
            }
        }
        if self.imperative {
            let first = text.split_whitespace().next().unwrap_or("");
            if !imperative(first) {
                found.push(format!(
                    "the subject should read as a command, not \"{first}\" (\"add\", not \
                     \"added\" or \"adds\")"
                ));
            }
        }
        if subject.ends_with('.') && (self.conventional || self.imperative) {
            found.push("the subject ends with a full stop".to_string());
        }
        if self.no_ticket_keys {
            if let Some(key) = ticket_key(subject) {
                found.push(format!(
                    "the subject names the tracker key {key}; link it in the description instead"
                ));
            }
        }
        found
    }

    /// What is wrong with `branch`, one finding per broken rule.
    pub fn check_branch(&self, branch: &str) -> Vec<String> {
        let mut found = Vec::new();
        if self.kebab_branch && !kebab(branch) {
            found.push(format!(
                "branch {branch} is not lowercase words joined by `-` (segments by `/`)"
            ));
        }
        if self.no_ticket_keys {
            if let Some(key) = ticket_key(branch) {
                found.push(format!("branch {branch} names the tracker key {key}"));
            }
        }
        found
    }
}

/// `type(scope)!: rest` → (`type`, `rest`).
fn conventional_type(subject: &str) -> Option<(&str, &str)> {
    let (head, rest) = subject.split_once(':')?;
    if !rest.starts_with(' ') || rest.trim().is_empty() {
        return None;
    }
    let head = head.strip_suffix('!').unwrap_or(head);
    let kind = match head.split_once('(') {
        Some((kind, scope)) => {
            let scope = scope.strip_suffix(')')?;
            if scope.is_empty() || scope.contains(['(', ')']) {
                return None;
            }
            kind
        }
        None => head,
    };
    (!kind.is_empty() && kind.chars().all(|c| c.is_ascii_lowercase())).then(|| (kind, rest.trim()))
}

/// Verbs whose third-person form is the usual non-imperative slip.
const VERBS: &[&str] = &[
    "add",
    "allow",
    "bump",
    "change",
    "clean",
    "drop",
    "enable",
    "disable",
    "ensure",
    "fix",
    "handle",
    "implement",
    "improve",
    "keep",
    "make",
    "merge",
    "move",
    "prevent",
    "refactor",
    "remove",
    "rename",
    "replace",
    "return",
    "revert",
    "show",
    "simplify",
    "skip",
    "support",
    "update",
    "use",
];

/// Words ending in `ed` or `ing` that are commands all the same.
const NOT_TENSES: &[&str] = &[
    "embed", "feed", "need", "seed", "shed", "speed", "bring", "ring",
];

fn imperative(word: &str) -> bool {
    let word = word.to_ascii_lowercase();
    let word = word.trim_matches(|c: char| !c.is_ascii_alphanumeric());
    if word.is_empty() || NOT_TENSES.contains(&word) {
        return true;
    }
    if (word.len() > 4 && word.ends_with("ed")) || (word.len() > 5 && word.ends_with("ing")) {
        return false;
    }
    let third = word
        .strip_suffix("es")
        .filter(|stem| VERBS.contains(stem))
        .or_else(|| word.strip_suffix('s').filter(|stem| VERBS.contains(stem)));
    third.is_none()
}

fn kebab(branch: &str) -> bool {
    !branch.is_empty()
        && branch.split('/').all(|segment| {
            !segment.is_empty()
                && segment.split('-').all(|word| {
                    !word.is_empty()
                        && word
                            .chars()
                            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '.')
                })
        })
}

/// The first tracker key in `text`: `PROJ-123` in any case.
fn ticket_key(text: &str) -> Option<String> {
    static KEY: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let key = KEY.get_or_init(|| {
        regex::Regex::new(r"(?:^|[^A-Za-z0-9])([A-Z][A-Z0-9]{1,9}-[0-9]+)(?:$|[^0-9])")
            .expect("valid")
    });
    key.captures(text).map(|c| c[1].to_string())
}

/// A forge branch for a change, from its title: `feat: follow requests`
/// becomes `feat/follow-requests`. For an agent that proposed none while
/// working on the node's own placeholder branch.
pub fn branch_from_title(title: &str) -> Option<String> {
    let (prefix, words) = match conventional_type(title.trim()) {
        Some((kind, rest)) => (Some(kind), rest),
        None => (None, title.trim()),
    };
    let mut slug = String::new();
    for word in words
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|w| !w.is_empty())
    {
        if slug.len() + word.len() + 1 > 48 {
            break;
        }
        if !slug.is_empty() {
            slug.push('-');
        }
        slug.push_str(&word.to_ascii_lowercase());
    }
    if slug.is_empty() {
        return None;
    }
    Some(match prefix {
        Some(kind) => format!("{kind}/{slug}"),
        None => slug,
    })
}

/// Whether git would take `name` as a branch: no spaces or control
/// characters, none of `~^:?*[\\`, no `..`, `@{` or `//`, and no segment
/// that starts with `.` or `-` or ends with `.lock`.
pub fn ref_name(name: &str) -> bool {
    !name.is_empty()
        && !name.ends_with('/')
        && !name.ends_with('.')
        && !name.contains("..")
        && !name.contains("@{")
        && name != "@"
        && name
            .chars()
            .all(|c| !c.is_control() && !" ~^:?*[\\".contains(c))
        && name.split('/').all(|segment| {
            !segment.is_empty()
                && !segment.starts_with('.')
                && !segment.starts_with('-')
                && !segment.ends_with(".lock")
        })
}

/// The node's placeholder for a session that named no branch. Never a name
/// worth shipping.
pub fn placeholder_branch(branch: &str) -> bool {
    branch.starts_with("feat/tracon-")
}

/// The commit message a squash carries when nobody wrote one: the change's
/// title, then its description.
pub fn default_message(title: &str, body: &str) -> String {
    let body = body.trim();
    if body.is_empty() {
        title.trim().to_string()
    } else {
        format!("{}\n\n{body}", title.trim())
    }
}

/// The message a squash carries: the one written for it, else the forge
/// description's, else the review's own title and body.
pub fn message_for(outputs: &super::publish::Outputs, title: &str, body: &str) -> String {
    match (&outputs.commit, &outputs.description) {
        (Some(message), _) => message.trim().to_string(),
        (None, Some(description)) => default_message(&description.title, &description.body),
        (None, None) => default_message(title, body),
    }
}

/// How commits ship and what they are held to, for a repository on a
/// channel: the repository's entry, then the channel's `publish` binding,
/// then `[publish]`.
pub fn rules(cfg: &Config, store: &Store, channel: &str, repo: Option<&Path>) -> (Commits, Style) {
    let repos = cfg.repos();
    let entry = repo.and_then(|repo| repos.iter().find(|entry| entry.matches(repo)));
    let bindings = store
        .channel_get(channel)
        .ok()
        .flatten()
        .and_then(|row| serde_json::from_str::<serde_json::Value>(&row.bindings_json).ok())
        .unwrap_or_default();
    let bound = &bindings["publish"];
    let commits = entry
        .and_then(|e| e.commits)
        .or_else(|| serde_json::from_value(bound["commits"].clone()).ok())
        .unwrap_or(cfg.publish.commits);
    let style = entry
        .and_then(|e| e.style.clone())
        .or_else(|| serde_json::from_value(bound["style"].clone()).ok())
        .unwrap_or_else(|| cfg.publish.style.clone());
    (commits, style)
}

/// One commit the agent made, as listed beside the diff.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommitLine {
    pub sha: String,
    pub subject: String,
    /// The message after its subject, for the operator to edit from. Absent
    /// for a revision listed before it was kept.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub body: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_conventional_imperative_subject_passes() {
        let style = Style::conventional();
        assert!(style
            .check_message("feat(review): squash the candidate\n\nbody")
            .is_empty());
        assert!(style.check_message("fix!: drop the old flag").is_empty());
        assert!(style.check_branch("feat/squash-the-candidate").is_empty());
    }

    #[test]
    fn each_broken_rule_is_named() {
        let style = Style::conventional();
        let found = style.check_message("Added PROJ-12 support.");
        assert_eq!(found.len(), 4, "{found:?}");
        assert!(found[0].contains("conventional"));
        assert!(found[1].contains("\"Added\""));
        assert!(found[2].contains("full stop"));
        assert!(found[3].contains("PROJ-12"));
        assert!(style.check_message("feat: adds a thing")[0].contains("\"adds\""));
        assert!(style.check_message("feat: adding a thing")[0].contains("command"));
        assert!(style.check_message("wip: a thing")[0].contains("`wip`"));
        assert!(style.check_message("feat: embed the docs").is_empty());
        assert!(!style
            .check_message(&format!("feat: {}", "x".repeat(80)))
            .is_empty());
        assert_eq!(style.check_branch("Feature/PROJ-12_Thing").len(), 2);
    }

    #[test]
    fn nothing_is_checked_unless_configured() {
        let style = Style::default();
        assert!(style.check_message("Added stuff.").is_empty());
        assert!(style.check_branch("My_Branch").is_empty());
        assert_eq!(
            style.check_message("")[0],
            "the commit message has no subject line"
        );
    }

    #[test]
    fn only_a_name_git_accepts_is_a_branch() {
        assert!(ref_name("feat/x-1"));
        for bad in [
            "", "a b", "a..b", "-x", "a/.b", "a.lock", "a/", "x~1", "a//b",
        ] {
            assert!(!ref_name(bad), "{bad}");
        }
    }

    #[test]
    fn a_branch_is_named_from_the_title() {
        assert_eq!(
            branch_from_title("feat(review): ship the reviewed tree").as_deref(),
            Some("feat/ship-the-reviewed-tree")
        );
        assert_eq!(
            branch_from_title("Follow published requests!").as_deref(),
            Some("follow-published-requests")
        );
        assert_eq!(branch_from_title("…"), None);
        let long = branch_from_title(&"word ".repeat(30)).unwrap();
        assert!(long.len() <= 48, "{long}");
        assert!(Style::conventional()
            .check_branch(&branch_from_title("fix: a v2.1 bug").unwrap())
            .is_empty());
    }
}
