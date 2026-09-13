//! The launch manifest: the operator's half of a session's configuration.
//!
//! The node writes the harness's whole configuration, and until now every
//! field in it was derived from something the node already knew — the wired
//! providers, the permission ruleset, the MCP entry. Customization is the part
//! an operator decides: which skills a session may call, what standing
//! instructions it starts with, which agents it may spawn, which plugins the
//! image is allowed to load, which language servers and formatters are on.
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
//! * **Upstream's own resolution is not trusted.** OpenCode warns and then
//!   overwrites on a duplicate skill name, and which of the two wins varies
//!   run to run (`config-state.md` §3.4). It fetches `skills.urls` over plain
//!   HTTP before any user interaction, with no path-traversal hardening on the
//!   v1 path and nothing that disables it (§3.6). So duplicates are refused
//!   here, at build time, and a URL source is refused outright — the node
//!   copies a package it has read into storage it owns, and the harness is
//!   pointed at that and nothing else.
//! * **Skill content is code.** A `SKILL.md` body is also registered as a
//!   slash command, and command templates are shell-interpolated: a body
//!   containing `` !`cmd` `` becomes shell execution on `/name`, on a path
//!   that never reaches the `skill` permission check (§3.7). Nothing here
//!   pretends otherwise. An import records that its scripts run in the runner,
//!   and a body that already carries shell interpolation says so by name.

pub mod skill;

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub use skill::{parse_source, read_package, SkillSource};

/// Where the rendered skill packages are mounted, relative to the harness's
/// state directory — outside the worktree, and outside every directory
/// OpenCode discovers on its own.
pub const SKILL_DIR: &str = "skills";

/// The variable the rendered config resolves the skill root through.
///
/// The config file is written before the node knows the harness's home, and
/// `skills.paths` entries are resolved against the *worktree* when they are
/// relative (`config-state.md` §3.2). `{env:VAR}` substitution is applied to
/// every config text OpenCode loads, including the one `OPENCODE_CONFIG`
/// names, so the launch environment supplies the absolute path and the file
/// stays independent of where the runner puts the harness's home.
pub const SKILL_ROOT_ENV: &str = "TRACON_SKILL_ROOT";

/// The shape of the image's offline package cache, relative to
/// `$XDG_CACHE_HOME/opencode`. A pure existence check at this path is the
/// whole of OpenCode's offline resolution — no version verification, no
/// registry contact (`config-state.md` §4.4) — so the image that creates it is
/// the trust root and the manifest may name nothing else.
pub fn plugin_cache_path(package: &str) -> String {
    let (name, _) = split_package(package).unwrap_or((package, ""));
    format!("packages/{package}/node_modules/{name}")
}

/// `@scope/pkg@1.2.3` → `("@scope/pkg", "1.2.3")`. A bare name has no version
/// and is refused: `<pkg>@latest` resolves against whatever the image happened
/// to bake, which is not a pin.
fn split_package(package: &str) -> Option<(&str, &str)> {
    let at = package.rfind('@').filter(|at| *at > 0)?;
    let (name, version) = package.split_at(at);
    let version = &version[1..];
    if name.is_empty() || version.is_empty() {
        return None;
    }
    Some((name, version))
}

/// The language servers the harness image's toolchain profile bakes, in the
/// shape the manifest records them.
///
/// The manifest does not choose these and does not render them — the profile
/// writes the harness's `lsp` key, because only the image knows the paths.
/// What the manifest adds is memory: the names and commands go into the
/// digest, so a session's row still says which toolchain produced its
/// transcript after the profile has moved on.
pub fn toolchain_lsp(harness_id: &str) -> Vec<ToolEntry> {
    if harness_id != crate::adapter::opencode::OpenCodeAdapter::ID {
        return Vec::new();
    }
    crate::runner::toolchain::profile()
        .lsp
        .iter()
        .map(|tool| ToolEntry {
            name: tool.id.clone(),
            command: tool.command.clone(),
        })
        .collect()
}

/// The same for formatters.
pub fn toolchain_formatters(harness_id: &str) -> Vec<ToolEntry> {
    if harness_id != crate::adapter::opencode::OpenCodeAdapter::ID {
        return Vec::new();
    }
    crate::runner::toolchain::profile()
        .formatter
        .iter()
        .map(|tool| ToolEntry {
            name: tool.id.clone(),
            command: tool.command.clone(),
        })
        .collect()
}

#[derive(Debug, thiserror::Error)]
pub enum ManifestError {
    /// Upstream warns and overwrites nondeterministically; the node refuses.
    #[error(
        "two skills in this manifest are named `{0}`; OpenCode resolves a duplicate by \
         overwriting one of them and which one wins varies run to run, so the manifest \
         refuses it. Rename or remove one before importing."
    )]
    DuplicateSkill(String),
    #[error(
        "`{0}` is a URL skill source. OpenCode fetches `skills.urls` over plain HTTP before \
         any interaction, with no flag that disables it and no path-traversal hardening on \
         the version this node pins. Import the package from a directory or a git revision \
         instead: the node copies what it has read into storage it owns."
    )]
    UrlSource(String),
    #[error(
        "plugin `{name}` is not in this image's offline package cache. The image bakes \
         `$XDG_CACHE_HOME/opencode/{cache}`, and a name it did not bake would either \
         resolve to nothing or send the runner to a registry it cannot reach. Add it to \
         the harness image and to `[launch] plugins` in node.toml, then rebuild."
    )]
    UnapprovedPlugin { name: String, cache: String },
    #[error(
        "plugin `{0}` has no exact version. Name it `<package>@<version>`: a bare name \
         resolves to whatever the image happened to bake, which is not a pin."
    )]
    UnpinnedPlugin(String),
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

/// A language server or formatter the image's toolchain profile bakes:
/// OpenCode's own name for it, and the absolute command the image installed
/// it at.
///
/// The command travels with the name because it is what distinguishes two
/// otherwise identical launches: the same `rust` server at another path is a
/// different toolchain, and a digest that could not tell them apart would say
/// less than it appears to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolEntry {
    pub name: String,
    pub command: Vec<String>,
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
    /// Approved plugin packages, `<pkg>@<version>`, every one of them present
    /// in the image's offline cache.
    #[serde(default)]
    pub plugins: Vec<String>,
    /// The language servers the harness image's toolchain profile bakes, as
    /// this launch was configured with them. Recorded rather than rendered:
    /// the profile writes the harness's `lsp` key, and this is what makes the
    /// session's digest say which toolchain produced its transcript.
    #[serde(default)]
    pub lsp: Vec<ToolEntry>,
    #[serde(default)]
    pub formatters: Vec<ToolEntry>,
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
    /// Plugin packages the operator approved.
    pub plugins: &'a [String],
    /// Packages the harness image's offline cache contains, read from the
    /// image's own toolchain profile. The only seam where the node's approval
    /// and the image's contents have to agree.
    pub baked: &'a [String],
    pub lsp: Vec<ToolEntry>,
    pub formatters: Vec<ToolEntry>,
    pub providers: Vec<String>,
    pub policy_revision: String,
}

/// Build and validate. Every refusal here is a refusal *before* a session
/// exists: a manifest that would have been resolved nondeterministically, or
/// fetched over the network, or pointed at a package the image cannot
/// resolve, never reaches a launch.
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

    let baked: BTreeSet<&str> = inputs.baked.iter().map(String::as_str).collect();
    for plugin in inputs.plugins {
        if split_package(plugin).is_none() {
            return Err(ManifestError::UnpinnedPlugin(plugin.clone()));
        }
        if !baked.contains(plugin.as_str()) {
            return Err(ManifestError::UnapprovedPlugin {
                name: plugin.clone(),
                cache: plugin_cache_path(plugin),
            });
        }
    }

    let mut manifest = LaunchManifest {
        channel: inputs.channel.to_string(),
        revision: 0,
        digest: String::new(),
        skills: inputs.skills,
        instructions: inputs.instructions,
        agents: inputs.agents,
        plugins: inputs.plugins.to_vec(),
        lsp: inputs.lsp,
        formatters: inputs.formatters,
        providers: inputs.providers,
        policy_revision: inputs.policy_revision,
    };
    // Sorted, so the digest is a fact about the content rather than about the
    // order two operators happened to add things in.
    manifest.skills.sort_by(|a, b| a.name.cmp(&b.name));
    manifest.instructions.sort_by(|a, b| a.name.cmp(&b.name));
    manifest.agents.sort_by(|a, b| a.name.cmp(&b.name));
    manifest.plugins.sort();
    manifest.lsp.sort_by(|a, b| a.name.cmp(&b.name));
    manifest.formatters.sort_by(|a, b| a.name.cmp(&b.name));
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
        for plugin in &self.plugins {
            line("plugin", plugin);
        }
        for entry in &self.lsp {
            line("lsp", &entry.name);
            line("lsp.command", &entry.command.join(" "));
        }
        for entry in &self.formatters {
            line("formatter", &entry.name);
            line("formatter.command", &entry.command.join(" "));
        }
        for name in &self.providers {
            line("provider", name);
        }
        line("policy", &self.policy_revision);
        hex::encode(hasher.finalize())
    }

    /// The skill packages as files to stage, relative to the harness's state
    /// directory. Each one is mounted read-only; the harness reads them and
    /// writes nothing back (`config-state.md` §3.5 — loading is glob, read,
    /// parse, with no lock and no index).
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

    /// What the session is told, appended to the node's orientation the way
    /// the other adapters' system prompts already are.
    ///
    /// Instructions are *not* written into the harness's `instructions` config
    /// key. That key takes globs resolved against the project and re-rooted
    /// when the project is disabled (`config-state.md` §1.4); the node already
    /// owns one place a session's standing text goes, and two would be one too
    /// many. Instruction content grants no permission here either — it is
    /// text, and every tool class is still `ask`.
    pub fn orientation(&self) -> String {
        if self.instructions.is_empty() && self.agents.is_empty() && self.skills.is_empty() {
            return String::new();
        }
        let mut out = String::from("## Customization\n\n");
        out.push_str(&format!(
            "This channel's launch manifest is revision {} (`{}`). It is the operator's, \
             not yours: nothing here can be changed from inside the session.\n\n",
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
        for entry in &self.instructions {
            out.push_str(&format!("### {}\n\n{}\n\n", entry.name, entry.body.trim()));
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
            plugins: &[],
            baked: &[],
            lsp: Vec::new(),
            formatters: Vec::new(),
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
        // The message has to say why, because upstream's own behaviour is to
        // accept it: an operator who sees "refused" needs the reason.
        assert!(error.to_string().contains("overwriting"), "{error}");
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
    fn a_plugin_the_image_did_not_bake_is_refused_with_the_cache_path() {
        let approved = ["@tracon/audit@1.0.0".to_string()];
        let mut bad = inputs(Vec::new());
        bad.plugins = &approved;
        let error = build(bad).expect_err("an unbaked plugin must not build");
        assert!(
            error
                .to_string()
                .contains("packages/@tracon/audit@1.0.0/node_modules/@tracon/audit"),
            "{error}"
        );

        let mut good = inputs(Vec::new());
        good.plugins = &approved;
        let baked = ["@tracon/audit@1.0.0".to_string()];
        good.baked = &baked;
        assert_eq!(build(good).unwrap().plugins, approved);
    }

    #[test]
    fn a_plugin_without_an_exact_version_is_refused() {
        let approved = ["@tracon/audit".to_string()];
        let mut unpinned = inputs(Vec::new());
        unpinned.plugins = &approved;
        unpinned.baked = &approved;
        let error = build(unpinned).expect_err("a bare name is not a pin");
        assert!(matches!(error, ManifestError::UnpinnedPlugin(_)), "{error}");
    }

    #[test]
    fn the_cache_path_is_the_shape_the_image_bakes() {
        assert_eq!(
            plugin_cache_path("@tracon/audit@1.0.0"),
            "packages/@tracon/audit@1.0.0/node_modules/@tracon/audit"
        );
        assert_eq!(
            plugin_cache_path("plain@2.3.4"),
            "packages/plain@2.3.4/node_modules/plain"
        );
    }

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

    #[test]
    fn the_orientation_names_the_revision_and_the_skills() {
        let mut manifest = build(inputs(vec![skill("alpha", "a")])).unwrap();
        manifest.revision = 7;
        manifest.instructions.push(TextEntry {
            name: "House style".into(),
            body: "Small commits.".into(),
        });
        let text = manifest.orientation();
        assert!(text.contains("revision 7"), "{text}");
        assert!(text.contains("`alpha`"), "{text}");
        assert!(text.contains("Small commits."), "{text}");
        assert!(text.contains("It is the operator's"), "{text}");
    }

    #[test]
    fn an_empty_manifest_renders_nothing() {
        let manifest = build(inputs(Vec::new())).unwrap();
        assert!(manifest.skills.is_empty());
        assert!(manifest.skill_files().is_empty());
        assert!(manifest.orientation().is_empty());
        // It still has a digest: "nothing customized" is a fact worth
        // recording on a session, and it must differ per channel.
        assert!(!manifest.digest.is_empty());
    }
}
