//! The language toolchain the OpenCode harness image bakes, and the `lsp` and
//! `formatter` fragments the node renders from it.
//!
//! One profile, in one file, read from two sides. The image build copies
//! `containers/harness-opencode/toolchain.json` to `/opt/tracon/toolchain.json`
//! and adds the digest of every binary it names, so an operator can tell what
//! an image actually holds; this module compiles the same file in, so the
//! configuration the node writes for a session names exactly the paths the
//! image was built to provide. A profile that names a path the image does not
//! have is caught twice: the image build refuses it, and the launch status
//! below reports it as `unavailable` rather than letting the first edit skip a
//! server in silence.
//!
//! Why the node names paths at all, when OpenCode can find these itself:
//! finding it means downloading it. `OPENCODE_DISABLE_LSP_DOWNLOAD=true` stops
//! that for LSP servers, but the three auto-installing formatters (`prettier`,
//! `oxfmt`, `biome`) are not covered by it, and naming a `command` is the only
//! lever there (`config-state.md` §6.6). Naming a builtin's `command` also
//! discards the builtin's `initialization`, so every one this profile overrides
//! carries its own copy (§6.4).

use std::collections::BTreeMap;
use std::sync::OnceLock;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use super::{Runner, RunnerCommand};

/// Where the image keeps its own copy of this profile, with digests added. The
/// launch-manifest builder reads it by this path; nothing else in the node
/// depends on its contents.
pub const MANIFEST_PATH: &str = "/opt/tracon/toolchain.json";

/// The entrypoint the OpenCode image runs before the harness: it seeds the
/// per-session package caches from the image and `exec`s, so the harness is
/// what the container's init supervises. Podman uses the image's own
/// `ENTRYPOINT`; a Kubernetes `command` *replaces* an image entrypoint rather
/// than prefixing it, so the pod has to name this explicitly.
pub const IMAGE_ENTRYPOINT: &str = "/opt/tracon/bin/harness-entrypoint";

/// The entrypoint a harness image runs before the harness, when it has one.
/// Only the OpenCode image does; the others put their CLI straight in the
/// container's command and have nothing to seed.
pub fn image_entrypoint(harness_id: &str) -> Option<String> {
    (harness_id == crate::adapter::opencode::OpenCodeAdapter::ID)
        .then(|| IMAGE_ENTRYPOINT.to_string())
}

/// The committed profile. Compiled in rather than read at run time: the node
/// has to be able to render a launch configuration on a machine that has never
/// pulled the image, and the two must agree by construction.
const PROFILE_JSON: &str = include_str!("../../../containers/harness-opencode/toolchain.json");

#[derive(Debug, Clone, Deserialize)]
pub struct Tool {
    /// The OpenCode builtin id this overrides (`rust`, `typescript`,
    /// `rustfmt`, `prettier`).
    pub id: String,
    pub version: String,
    /// Absolute, in the image. `$FILE` in a formatter's arguments is
    /// OpenCode's substitution, not this node's.
    pub command: Vec<String>,
    #[serde(default)]
    pub extensions: Vec<String>,
    #[serde(default)]
    pub initialization: Option<Value>,
    #[serde(default)]
    pub environment: Option<BTreeMap<String, String>>,
}

impl Tool {
    /// The path the runtime spawns, which is the path whose existence decides
    /// whether this tool is `configured` or `unavailable`.
    pub fn binary(&self) -> &str {
        self.command.first().map(String::as_str).unwrap_or_default()
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct PluginSeed {
    pub package: String,
    pub version: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Profile {
    pub revision: String,
    pub opencode_version: String,
    pub plugin: PluginSeed,
    pub lsp: Vec<Tool>,
    pub formatter: Vec<Tool>,
    /// Formatters that must never run: the auto-installing ones this image
    /// does not bake. `disabled` deletes them from the registry, which is the
    /// only thing that makes their install path unreachable.
    pub disabled_formatter: Vec<String>,
    /// Every builtin LSP id in the pinned release. Enabling `lsp` at all
    /// registers all of them (`lsp.ts:154-156`), so the ones this image does
    /// not bake are disabled by name rather than left to a download flag.
    pub builtin_lsp: Vec<String>,
}

pub fn profile() -> &'static Profile {
    static PROFILE: OnceLock<Profile> = OnceLock::new();
    PROFILE.get_or_init(|| {
        serde_json::from_str(PROFILE_JSON)
            .expect("containers/harness-opencode/toolchain.json is not a valid toolchain profile")
    })
}

/// The `lsp` and `formatter` fragments for the harness configuration, naming
/// the image's absolute paths.
///
/// Both are records rather than `true`: a boolean enables every builtin, and
/// every builtin carries its own way of fetching itself. Everything this image
/// does not bake is named and disabled, so the set of servers and formatters
/// that can ever spawn is the set below and nothing else.
pub fn lsp_formatter_config() -> (Value, Value) {
    let profile = profile();

    let mut lsp = serde_json::Map::new();
    for id in &profile.builtin_lsp {
        lsp.insert(id.clone(), json!({ "disabled": true }));
    }
    for tool in &profile.lsp {
        let mut entry = serde_json::Map::new();
        entry.insert("command".into(), json!(tool.command));
        if !tool.extensions.is_empty() {
            entry.insert("extensions".into(), json!(tool.extensions));
        }
        // Copied from the builtin, because naming `command` discards the
        // builtin's own (`config-state.md` §6.4): overriding `typescript`
        // without this silently drops the `tsserver.path` it needs.
        if let Some(init) = &tool.initialization {
            entry.insert("initialization".into(), init.clone());
        }
        if let Some(env) = &tool.environment {
            entry.insert("env".into(), json!(env));
        }
        lsp.insert(tool.id.clone(), Value::Object(entry));
    }

    let mut formatter = serde_json::Map::new();
    for id in &profile.disabled_formatter {
        formatter.insert(id.clone(), json!({ "disabled": true }));
    }
    for tool in &profile.formatter {
        let mut entry = serde_json::Map::new();
        entry.insert("command".into(), json!(tool.command));
        if !tool.extensions.is_empty() {
            entry.insert("extensions".into(), json!(tool.extensions));
        }
        if let Some(env) = &tool.environment {
            entry.insert("environment".into(), json!(env));
        }
        formatter.insert(tool.id.clone(), Value::Object(entry));
    }

    (Value::Object(lsp), Value::Object(formatter))
}

/// Put both fragments into a harness configuration document. The one place
/// this profile crosses into the adapter's file, so the adapter's own
/// rendering stays about providers and permissions.
pub fn merge_into_config(document: &mut Value) {
    let (lsp, formatter) = lsp_formatter_config();
    document["lsp"] = lsp;
    document["formatter"] = formatter;
}

/// What the node can honestly say about one tool at launch. OpenCode emits no
/// LSP status events at all — a server that fails to spawn is added to a
/// `broken` set with no log line and no event (`config-state.md` §6.5) — so
/// `starting`, `running` and `failed` are not observable from here. These
/// three are, and they are the ones that matter to an operator reading a
/// session header: what was asked for, and whether the image can provide it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolState {
    /// Named by the profile and present in the image.
    Configured,
    /// Named by the profile and missing from the image: the session will run,
    /// and this language will silently have no server.
    Unavailable,
    /// A builtin this image does not bake, disabled by name in the config.
    Disabled,
}

impl ToolState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Configured => "configured",
            Self::Unavailable => "unavailable",
            Self::Disabled => "disabled",
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ToolStatus {
    pub id: String,
    /// `lsp` or `formatter`.
    pub kind: &'static str,
    pub version: String,
    pub path: String,
    pub state: ToolState,
}

/// What the node learned about the image's toolchain at launch.
#[derive(Debug, Clone, Serialize)]
pub struct ToolchainStatus {
    /// The profile revision this node rendered from.
    pub revision: String,
    /// The revision the image reports, when it reports one. A mismatch means
    /// the image was built from other definitions than this binary's, which
    /// `tracon check-boundary` also refuses.
    pub image_revision: Option<String>,
    pub tools: Vec<ToolStatus>,
}

impl ToolchainStatus {
    /// True when every tool the profile names is present.
    pub fn complete(&self) -> bool {
        !self
            .tools
            .iter()
            .any(|tool| tool.state == ToolState::Unavailable)
    }
}

/// The shell the probe runs in the image: one line per named binary, plus the
/// image's own profile revision. `sed` rather than a JSON parser because the
/// harness image has no general-purpose one and this needs exactly one field.
fn probe_script(paths: &[String]) -> String {
    let quoted = paths
        .iter()
        .map(|p| format!("'{}'", p.replace('\'', "'\\''")))
        .collect::<Vec<_>>()
        .join(" ");
    format!(
        "sed -n 's/.*\"revision\": *\"\\([^\"]*\\)\".*/rev \\1/p' {MANIFEST_PATH} 2>/dev/null | head -n 1; \
         for p in {quoted}; do if [ -x \"$p\" ]; then echo \"ok $p\"; else echo \"missing $p\"; fi; done"
    )
}

/// Turn the probe's output into a status. A path the probe said nothing about
/// — a probe that could not run at all — is `unavailable`: an unanswered
/// question about the image is not a yes.
fn read_probe(out: &str) -> ToolchainStatus {
    let profile = profile();
    let mut present = std::collections::BTreeSet::new();
    let mut image_revision = None;
    for line in out.lines() {
        if let Some(rev) = line.strip_prefix("rev ") {
            image_revision = Some(rev.trim().to_string());
        } else if let Some(path) = line.strip_prefix("ok ") {
            present.insert(path.trim().to_string());
        }
    }
    let mut tools = Vec::new();
    for (kind, list) in [("lsp", &profile.lsp), ("formatter", &profile.formatter)] {
        for tool in list {
            let path = tool.binary().to_string();
            tools.push(ToolStatus {
                id: tool.id.clone(),
                kind,
                version: tool.version.clone(),
                state: if present.contains(&path) {
                    ToolState::Configured
                } else {
                    ToolState::Unavailable
                },
                path,
            });
        }
    }
    for id in &profile.builtin_lsp {
        if profile.lsp.iter().any(|tool| &tool.id == id) {
            continue;
        }
        tools.push(ToolStatus {
            id: id.clone(),
            kind: "lsp",
            version: String::new(),
            path: String::new(),
            state: ToolState::Disabled,
        });
    }
    for id in &profile.disabled_formatter {
        tools.push(ToolStatus {
            id: id.clone(),
            kind: "formatter",
            version: String::new(),
            path: String::new(),
            state: ToolState::Disabled,
        });
    }
    ToolchainStatus {
        revision: profile.revision.clone(),
        image_revision,
        tools,
    }
}

/// Ask the image which of the profile's binaries it actually has. One short
/// container run; the answer is the same for every session on one image, so
/// callers cache it.
pub async fn probe(runner: &dyn Runner) -> ToolchainStatus {
    let profile = profile();
    let paths: Vec<String> = profile
        .lsp
        .iter()
        .chain(profile.formatter.iter())
        .map(|tool| tool.binary().to_string())
        .collect();
    let out = runner
        .run_capture(RunnerCommand {
            argv: vec!["sh".into(), "-c".into(), probe_script(&paths)],
            name: "opencode-toolchain".into(),
            ..Default::default()
        })
        .await;
    match out {
        Ok(out) => read_probe(&String::from_utf8_lossy(&out.stdout)),
        // A probe that could not run reports every tool unavailable rather
        // than claiming a toolchain it never saw.
        Err(e) => {
            tracing::warn!(error = %e, "toolchain probe failed");
            read_probe("")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_committed_profile_parses_and_names_absolute_paths() {
        let profile = profile();
        assert!(!profile.lsp.is_empty());
        assert!(!profile.formatter.is_empty());
        for tool in profile.lsp.iter().chain(profile.formatter.iter()) {
            assert!(
                tool.binary().starts_with("/opt/tracon/"),
                "{} is not an image-baked absolute path",
                tool.binary()
            );
        }
        // The two languages this repository's own workflows use.
        assert!(profile.lsp.iter().any(|t| t.id == "rust"));
        assert!(profile.lsp.iter().any(|t| t.id == "typescript"));
        assert!(profile.formatter.iter().any(|t| t.id == "rustfmt"));
        assert!(profile.formatter.iter().any(|t| t.id == "prettier"));
    }

    /// The profile and the image build must name the same paths and versions:
    /// the Containerfile asserts each one is executable, so a profile entry it
    /// does not mention would ship as a config pointing at nothing.
    #[test]
    fn the_image_definition_asserts_every_path_the_profile_names() {
        let containerfile = include_str!("../../../containers/harness-opencode/Containerfile");
        for tool in profile().lsp.iter().chain(profile().formatter.iter()) {
            assert!(
                containerfile.contains(tool.binary()),
                "the Containerfile never mentions {}",
                tool.binary()
            );
        }
        assert!(containerfile.contains(IMAGE_ENTRYPOINT));
        assert!(containerfile.contains(&profile().opencode_version));
    }

    #[test]
    fn every_builtin_is_either_configured_or_disabled() {
        let (lsp, formatter) = lsp_formatter_config();
        let lsp = lsp.as_object().unwrap();
        for id in &profile().builtin_lsp {
            let entry = lsp.get(id).expect("every builtin server is named");
            let named = profile().lsp.iter().any(|tool| &tool.id == id);
            if named {
                assert!(entry.get("command").is_some(), "{id} has no command");
                assert!(entry.get("disabled").is_none(), "{id} is both on and off");
            } else {
                assert_eq!(entry["disabled"], json!(true), "{id} is not disabled");
            }
        }
        // The three formatters that install themselves are the ones that must
        // never be left to their own devices.
        let formatter = formatter.as_object().unwrap();
        assert!(formatter["prettier"]["command"].is_array());
        assert_eq!(formatter["oxfmt"]["disabled"], json!(true));
        assert_eq!(formatter["biome"]["disabled"], json!(true));
    }

    /// Overriding a builtin's `command` discards the builtin's
    /// `initialization`. TypeScript's carries the `tsserver` path, without
    /// which the server starts and does nothing.
    #[test]
    fn the_typescript_override_carries_the_builtin_initialization() {
        let (lsp, _) = lsp_formatter_config();
        let tsserver = lsp["typescript"]["initialization"]["tsserver"]["path"]
            .as_str()
            .expect("the typescript override names a tsserver");
        assert!(tsserver.starts_with("/opt/tracon/"));
        assert!(tsserver.ends_with("tsserver.js"));
    }

    #[test]
    fn the_config_merge_leaves_the_rest_of_the_document_alone() {
        let mut document = json!({ "provider": { "x": {} }, "permission": { "*": "ask" } });
        merge_into_config(&mut document);
        assert_eq!(document["permission"]["*"], json!("ask"));
        assert!(document["lsp"]["rust"]["command"].is_array());
        assert!(document["formatter"]["rustfmt"]["command"].is_array());
    }

    #[test]
    fn a_missing_binary_reads_as_unavailable_not_as_absent() {
        let rust = profile().lsp.iter().find(|t| t.id == "rust").unwrap();
        let status = read_probe(&format!("rev 1\nok {}\n", rust.binary()));
        assert_eq!(status.image_revision.as_deref(), Some("1"));
        let by_id = |id: &str| {
            status
                .tools
                .iter()
                .find(|t| t.id == id)
                .map(|t| t.state)
                .unwrap()
        };
        assert_eq!(by_id("rust"), ToolState::Configured);
        assert_eq!(by_id("typescript"), ToolState::Unavailable);
        assert_eq!(by_id("prettier"), ToolState::Unavailable);
        assert_eq!(by_id("gopls"), ToolState::Disabled);
        assert!(!status.complete());
    }

    #[test]
    fn a_probe_that_never_ran_claims_nothing() {
        let status = read_probe("");
        assert!(status.image_revision.is_none());
        assert!(!status.complete());
        assert!(status
            .tools
            .iter()
            .filter(|t| !t.path.is_empty())
            .all(|t| t.state == ToolState::Unavailable));
    }

    #[test]
    fn the_probe_script_quotes_every_path_it_tests() {
        let script = probe_script(&["/opt/tracon/lsp/rust-analyzer".into(), "a'b".into()]);
        assert!(script.contains("'/opt/tracon/lsp/rust-analyzer'"));
        assert!(script.contains("'a'\\''b'"));
        assert!(script.contains(MANIFEST_PATH));
    }
}
