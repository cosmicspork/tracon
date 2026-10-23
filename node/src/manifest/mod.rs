//! The launch manifest: the operator's half of a session's configuration.
//!
//! The node writes the harness's whole configuration, and until now every
//! field in it was derived from something the node already knew — the wired
//! providers, the permission ruleset, the MCP entry. Customization is the part
//! an operator decides: which skills a session may call, what standing
//! instructions it starts with, which agents it may spawn.
//!
//! Three properties make that safe to hand to a harness, and they are the
//! reason this is a node-owned object rather than a directory the harness
//! discovers:
//!
//! * **It is named by a digest.** Everything the manifest renders is hashed
//!   into one content digest; a revision number counts the times that digest
//!   changed. A session records the digest it launched under, so what a
//!   transcript ran with is readable from the row months later. Editing the
//!   manifest mints a new revision and never touches a running session: the
//!   files were staged at launch and the next launch is what picks the change
//!   up.
//! * **A harness's own resolution is not trusted.** A duplicate skill name
//!   is refused here, at build time, rather than left to whichever copy a
//!   harness happens to load, and a URL source is refused outright — the node
//!   copies a package it has read into storage it owns, and the harness is
//!   pointed at that and nothing else.
//! * **Skill content is code.** A skill's scripts run in the runner, and a
//!   body carrying shell interpolation (`` !`cmd` ``) can become shell
//!   execution in a harness that registers skills as commands. Nothing here
//!   pretends otherwise. An import records that its scripts run in the runner,
//!   and a body that already carries shell interpolation says so by name.

pub mod skill;

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub use skill::{parse_source, read_package, SkillSource};

/// The directory the rendered skill packages sit under, relative to wherever
/// the adapter stages them: one `<name>/` per skill, the layout Claude Code
/// discovers as `skills/**/SKILL.md` in its config directory.
pub const SKILL_DIR: &str = "skills";

/// Where a built-in skill says it came from: the node itself, so an operator
/// reading the pane can tell it from a package they imported.
pub const BUILTIN_SOURCE: &str = "builtin";

/// Skills the node ships to every channel, as `(name, SKILL.md)`.
const BUILTIN_SKILLS: &[(&str, &str)] =
    &[("repo-setup", include_str!("builtin/repo-setup/SKILL.md"))];

/// The skills the node ships, in the shape an import produces.
pub fn builtin_skills() -> Vec<SkillEntry> {
    BUILTIN_SKILLS
        .iter()
        .map(|(name, text)| {
            let files = vec![ManifestFile {
                path: "SKILL.md".into(),
                text: (*text).to_string(),
            }];
            let description = skill::frontmatter(text)
                .map(|(_, description)| description)
                .unwrap_or_default();
            SkillEntry {
                name: (*name).to_string(),
                description,
                source: BUILTIN_SOURCE.into(),
                digest: skill::digest_of(&files),
                warnings: vec![format!(
                    "Ships with this node and changes when the node is upgraded. Import a \
                     skill named `{name}` to replace it on this channel."
                )],
                files,
            }
        })
        .collect()
}

/// A channel's imported skills with the node's own added, except where the
/// operator imported one by the same name: theirs replaces the node's rather
/// than colliding with it.
pub fn with_builtins(mut skills: Vec<SkillEntry>) -> Vec<SkillEntry> {
    for builtin in builtin_skills() {
        if !skills.iter().any(|skill| skill.name == builtin.name) {
            skills.push(builtin);
        }
    }
    skills
}

#[derive(Debug, thiserror::Error)]
pub enum ManifestError {
    /// Which of two same-named skills a harness loads is not the node's to
    /// leave to chance; it refuses the pair.
    #[error(
        "two skills in this manifest are named `{0}`, and which one a session would load is \
         not something the manifest leaves to chance. Rename or remove one before importing."
    )]
    DuplicateSkill(String),
    #[error(
        "`{0}` is a URL skill source. Import the package from a directory or a git revision \
         instead: the node copies what it has read into storage it owns, and a session never \
         fetches a skill itself."
    )]
    UrlSource(String),
    #[error("{0}")]
    Skill(String),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

/// One file of a skill package, as the node stored it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManifestFile {
    /// Relative to the package root. Validated at import: no absolute path,
    /// no `..`, no symlink.
    pub path: String,
    pub text: String,
}

/// A skill the operator imported, as the manifest carries it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillEntry {
    pub name: String,
    pub description: String,
    /// Where it came from, for the operator: `dir:<path>` or `git:<rev> <path>`.
    pub source: String,
    /// sha256 over the package's sorted `(path, content)` pairs.
    pub digest: String,
    /// What the import wants the operator to have read. Never empty: a skill
    /// package ships scripts and they run in the runner.
    pub warnings: Vec<String>,
    pub files: Vec<ManifestFile>,
}

/// A standing instruction or an agent definition: text the node owns and the
/// session is told, rather than a file in the worktree.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TextEntry {
    pub name: String,
    pub body: String,
}

/// What a channel's sessions launch with.
///
/// `revision` and `digest` are identity, not content: the digest is computed
/// over everything else, and the revision counts the times it changed. Two
/// nodes that built the same manifest from the same inputs agree on the
/// digest; a node that rebuilt it unchanged does not mint a revision.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LaunchManifest {
    pub channel: String,
    #[serde(default)]
    pub revision: i64,
    #[serde(default)]
    pub digest: String,
    #[serde(default)]
    pub skills: Vec<SkillEntry>,
    #[serde(default)]
    pub instructions: Vec<TextEntry>,
    #[serde(default)]
    pub agents: Vec<TextEntry>,
    /// The providers this channel could actually spend on when the manifest
    /// was built. Part of the digest because a session launched against a
    /// different provider set is a different session.
    #[serde(default)]
    pub providers: Vec<String>,
    /// The policy the gate ran under: the bundle's revision, in the shape
    /// `policy/bundle.rs` hands out.
    #[serde(default)]
    pub policy_revision: String,
}

/// The inputs a manifest is built from: what the operator imported, what the
/// node configured, and what the session's own wiring settled.
pub struct Inputs<'a> {
    pub channel: &'a str,
    pub skills: Vec<SkillEntry>,
    pub instructions: Vec<TextEntry>,
    pub agents: Vec<TextEntry>,
    pub providers: Vec<String>,
    pub policy_revision: String,
}

/// Build and validate. Every refusal here is a refusal *before* a session
/// exists: a manifest that would have been resolved nondeterministically, or
/// fetched over the network, never reaches a launch.
pub fn build(inputs: Inputs<'_>) -> Result<LaunchManifest, ManifestError> {
    let mut seen = BTreeSet::new();
    for entry in &inputs.skills {
        if skill::is_url(&entry.source) {
            return Err(ManifestError::UrlSource(entry.source.clone()));
        }
        if !seen.insert(entry.name.clone()) {
            return Err(ManifestError::DuplicateSkill(entry.name.clone()));
        }
    }

    let mut manifest = LaunchManifest {
        channel: inputs.channel.to_string(),
        revision: 0,
        digest: String::new(),
        skills: inputs.skills,
        instructions: inputs.instructions,
        agents: inputs.agents,
        providers: inputs.providers,
        policy_revision: inputs.policy_revision,
    };
    // Sorted, so the digest is a fact about the content rather than about the
    // order two operators happened to add things in.
    manifest.skills.sort_by(|a, b| a.name.cmp(&b.name));
    manifest.instructions.sort_by(|a, b| a.name.cmp(&b.name));
    manifest.agents.sort_by(|a, b| a.name.cmp(&b.name));
    manifest.providers.sort();
    manifest.digest = manifest.compute_digest();
    Ok(manifest)
}

impl LaunchManifest {
    /// The content digest: everything the manifest renders, in a canonical
    /// form, hashed. Deliberately not `serde_json` over the struct — the
    /// digest must not move when a field is renamed on the wire or a new
    /// optional one appears with an empty value.
    pub fn compute_digest(&self) -> String {
        let mut hasher = Sha256::new();
        let mut line = |key: &str, value: &str| {
            hasher.update(key.as_bytes());
            hasher.update([0u8]);
            hasher.update(value.as_bytes());
            hasher.update([0u8]);
        };
        line("channel", &self.channel);
        for skill in &self.skills {
            line("skill.name", &skill.name);
            line("skill.description", &skill.description);
            line("skill.digest", &skill.digest);
            for file in &skill.files {
                line("skill.file", &file.path);
                line("skill.text", &file.text);
            }
        }
        for entry in &self.instructions {
            line("instruction.name", &entry.name);
            line("instruction.body", &entry.body);
        }
        for entry in &self.agents {
            line("agent.name", &entry.name);
            line("agent.body", &entry.body);
        }
        for name in &self.providers {
            line("provider", name);
        }
        line("policy", &self.policy_revision);
        hex::encode(hasher.finalize())
    }

    /// The skill packages as files to stage, relative to the harness's state
    /// directory. Each one is mounted read-only; the harness reads them and
    /// writes nothing back.
    pub fn skill_files(&self) -> Vec<(String, String)> {
        let mut files = Vec::new();
        for skill in &self.skills {
            for file in &skill.files {
                files.push((
                    format!("{SKILL_DIR}/{}/{}", skill.name, file.path),
                    file.text.clone(),
                ));
            }
        }
        files
    }

    /// The operator's standing notes for this channel, as their own section of
    /// the orientation. They are separated from everything tracon generates
    /// because a session that cannot tell the node's account of itself from a
    /// human's standing advice cannot weigh either one correctly.
    ///
    /// Instructions are not written into any harness configuration: the node
    /// already owns one place a session's standing text goes, and two would be
    /// one too many. Instruction content grants no permission either — it is
    /// text, and every tool class is still `ask`.
    pub fn operator_notes(&self) -> String {
        if self.instructions.is_empty() {
            return String::new();
        }
        let mut out = String::from("## Operator notes\n\n");
        out.push_str(&format!(
            "Standing notes from the operator of this node, read by every session on this \
             channel (launch manifest revision {}, `{}`). They are the operator's words, not \
             tracon's, and nothing here can be changed from inside the session.\n\n",
            self.revision,
            &self.digest[..8.min(self.digest.len())]
        ));
        for entry in &self.instructions {
            out.push_str(&format!("### {}\n\n{}\n\n", entry.name, entry.body.trim()));
        }
        out
    }

    /// What the operator installed for this channel's launches: skills the
    /// session may call, agents it may spawn. Mechanics — the advice lives in
    /// [`Self::operator_notes`].
    pub fn customization(&self) -> String {
        if self.agents.is_empty() && self.skills.is_empty() {
            return String::new();
        }
        let mut out = String::from("## Customization\n\n");
        out.push_str(&format!(
            "What this channel's launch manifest installed (revision {}, `{}`). It is the \
             operator's, not yours: nothing here can be changed from inside the session.\n\n",
            self.revision,
            &self.digest[..8.min(self.digest.len())]
        ));
        if !self.skills.is_empty() {
            out.push_str("Skills available through the `skill` tool:\n\n");
            for skill in &self.skills {
                out.push_str(&format!("- `{}` — {}\n", skill.name, skill.description));
            }
            out.push('\n');
        }
        if !self.agents.is_empty() {
            out.push_str("Agents:\n\n");
            for agent in &self.agents {
                out.push_str(&format!("### {}\n\n{}\n\n", agent.name, agent.body.trim()));
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn skill(name: &str, body: &str) -> SkillEntry {
        SkillEntry {
            name: name.into(),
            description: "does a thing".into(),
            source: format!("dir:/srv/skills/{name}"),
            digest: skill::digest_of(&[ManifestFile {
                path: "SKILL.md".into(),
                text: body.into(),
            }]),
            warnings: vec![skill::CODE_WARNING.into()],
            files: vec![ManifestFile {
                path: "SKILL.md".into(),
                text: body.into(),
            }],
        }
    }

    fn inputs<'a>(skills: Vec<SkillEntry>) -> Inputs<'a> {
        Inputs {
            channel: "work",
            skills,
            instructions: Vec::new(),
            agents: Vec::new(),
            providers: vec!["anthropic".into()],
            policy_revision: "3".into(),
        }
    }

    /// The digest is a fact about content. Rebuilding the same inputs — in
    /// another order, on another day — has to produce the same string, or a
    /// session's recorded digest says nothing about what it ran.
    #[test]
    fn the_digest_is_stable_across_rebuilds_and_input_order() {
        let a = build(inputs(vec![skill("alpha", "a"), skill("beta", "b")])).unwrap();
        let b = build(inputs(vec![skill("beta", "b"), skill("alpha", "a")])).unwrap();
        assert_eq!(a.digest, b.digest);
        assert_eq!(a.digest, a.compute_digest());
    }

    #[test]
    fn any_content_change_changes_the_digest() {
        let base = build(inputs(vec![skill("alpha", "a")])).unwrap();
        let changed = build(inputs(vec![skill("alpha", "a "), skill("beta", "b")])).unwrap();
        assert_ne!(base.digest, changed.digest);

        // A change with no new skill at all still moves it.
        let mut only_body = inputs(vec![skill("alpha", "different")]);
        only_body.policy_revision = "3".into();
        assert_ne!(base.digest, build(only_body).unwrap().digest);

        // And so does anything else the manifest renders.
        let mut policy = inputs(vec![skill("alpha", "a")]);
        policy.policy_revision = "4".into();
        assert_ne!(base.digest, build(policy).unwrap().digest);
        let mut providers = inputs(vec![skill("alpha", "a")]);
        providers.providers = vec!["anthropic".into(), "local".into()];
        assert_ne!(base.digest, build(providers).unwrap().digest);
    }

    #[test]
    fn a_duplicate_skill_name_is_refused_rather_than_resolved() {
        let error = build(inputs(vec![skill("alpha", "a"), skill("alpha", "b")]))
            .expect_err("a duplicate must not build");
        assert!(matches!(error, ManifestError::DuplicateSkill(ref n) if n == "alpha"));
        // The message has to say why: an operator who sees "refused" needs
        // the reason.
        assert!(error.to_string().contains("leaves to chance"), "{error}");
    }

    #[test]
    fn a_url_skill_source_is_refused() {
        for url in [
            "https://skills.example/pack",
            "http://skills.example/pack",
            "HTTPS://SKILLS.EXAMPLE/pack",
        ] {
            let mut entry = skill("alpha", "a");
            entry.source = url.into();
            let error = build(inputs(vec![entry])).expect_err("a URL source must not build");
            assert!(matches!(error, ManifestError::UrlSource(_)), "{error}");
        }
    }

    /// The image is the trust root: a pure existence check at the cache path
    /// is the whole of upstream's offline resolution, so a name the image did
    /// not bake is refused here rather than discovered as a missing module
    /// inside a runner with no network.
    #[test]
    fn skills_render_under_one_root_named_by_skill() {
        let manifest = build(inputs(vec![skill("alpha", "a"), skill("beta", "b")])).unwrap();
        let files = manifest.skill_files();
        assert_eq!(
            files
                .iter()
                .map(|(path, _)| path.as_str())
                .collect::<Vec<_>>(),
            ["skills/alpha/SKILL.md", "skills/beta/SKILL.md"]
        );
    }

    /// The operator's own words and the mechanics the node installed are two
    /// sections, not one: an instruction is advice a session weighs, and a
    /// skill list is a fact about what it can call.
    #[test]
    fn the_operators_notes_and_the_installed_customization_are_separate_sections() {
        let mut manifest = build(inputs(vec![skill("alpha", "a")])).unwrap();
        manifest.revision = 7;
        manifest.instructions.push(TextEntry {
            name: "House style".into(),
            body: "Small commits.".into(),
        });

        let notes = manifest.operator_notes();
        assert!(notes.starts_with("## Operator notes"), "{notes}");
        assert!(notes.contains("revision 7"), "{notes}");
        assert!(notes.contains("Small commits."), "{notes}");
        assert!(notes.contains("not tracon's"), "{notes}");
        assert!(!notes.contains("`alpha`"), "{notes}");

        let custom = manifest.customization();
        assert!(custom.starts_with("## Customization"), "{custom}");
        assert!(custom.contains("revision 7"), "{custom}");
        assert!(custom.contains("`alpha`"), "{custom}");
        assert!(custom.contains("It is the operator's"), "{custom}");
        assert!(!custom.contains("Small commits."), "{custom}");
    }

    /// A channel with notes but nothing installed renders one section, not an
    /// empty "Customization" heading the session has to read past.
    #[test]
    fn notes_without_skills_render_no_customization_section() {
        let mut manifest = build(inputs(Vec::new())).unwrap();
        manifest.instructions.push(TextEntry {
            name: "House style".into(),
            body: "Small commits.".into(),
        });
        assert!(manifest.customization().is_empty());
        assert!(manifest.operator_notes().contains("Small commits."));
    }

    /// The node's own skill reaches every channel, parses the way an import
    /// would, and gives way to an operator's skill of the same name rather
    /// than tripping the duplicate refusal.
    #[test]
    fn the_builtin_setup_skill_is_added_unless_the_operator_imported_one() {
        let skills = with_builtins(Vec::new());
        let setup = skills
            .iter()
            .find(|skill| skill.name == "repo-setup")
            .expect("the setup skill ships");
        assert_eq!(setup.source, BUILTIN_SOURCE);
        assert!(setup.description.contains("Set this repository up once"));
        assert!(!setup.description.contains('<'));
        assert!(!setup.files[0].text.contains("!`"));
        assert!(setup.files[0].text.contains("run_notes"));
        let manifest = build(inputs(skills)).unwrap();
        assert!(manifest
            .skill_files()
            .iter()
            .any(|(path, _)| path == "skills/repo-setup/SKILL.md"));
        assert!(manifest.customization().contains("`repo-setup`"));

        let theirs = skill("repo-setup", "the operator's own");
        let skills = with_builtins(vec![theirs.clone()]);
        assert_eq!(skills, vec![theirs]);
        build(inputs(skills)).unwrap();
    }

    #[test]
    fn an_empty_manifest_renders_nothing() {
        let manifest = build(inputs(Vec::new())).unwrap();
        assert!(manifest.skills.is_empty());
        assert!(manifest.skill_files().is_empty());
        assert!(manifest.operator_notes().is_empty());
        assert!(manifest.customization().is_empty());
        // It still has a digest: "nothing customized" is a fact worth
        // recording on a session, and it must differ per channel.
        assert!(!manifest.digest.is_empty());
    }
}
