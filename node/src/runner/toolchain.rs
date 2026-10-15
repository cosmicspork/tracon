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
/// than prefixing it, so the pod has to name this explicitly. The layer that
/// puts OpenCode onto a repository's image copies it to the same path and
/// makes it that image's `ENTRYPOINT` too (`repo_image::harness_layer`).
pub const IMAGE_ENTRYPOINT: &str = "/opt/tracon/bin/harness-entrypoint";

/// The entrypoint a harness's image runs before the harness, when it has one:
/// its own image, or a repository's with the harness layered on. Only
/// OpenCode has one; the others put their CLI straight in the container's
/// command and have nothing to seed.
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
    /// What runs `command`, when it is a script: the baked node, named in
    /// front of it in the configuration the node renders. Named rather than
    /// left to the script's `#!/usr/bin/env node`, because on a repository's
    /// image the `node` a PATH finds is the repository's, at whatever version
    /// it chose, or none.
    #[serde(default)]
    pub interpreter: Option<String>,
    #[serde(default)]
    pub extensions: Vec<String>,
    #[serde(default)]
    pub initialization: Option<Value>,
    #[serde(default)]
    pub environment: Option<BTreeMap<String, String>>,
}

impl Tool {
    /// The tool itself: what `command` starts with. For a native binary that
    /// is what the runtime spawns; for a script, what its interpreter runs.
    pub fn binary(&self) -> &str {
        self.command.first().map(String::as_str).unwrap_or_default()
    }

    pub fn interpreter(&self) -> Option<&str> {
        self.interpreter.as_deref()
    }

    /// What OpenCode is configured to spawn: the interpreter, where there is
    /// one, then the command.
    pub fn argv(&self) -> Vec<String> {
        self.interpreter
            .iter()
            .chain(self.command.iter())
            .cloned()
            .collect()
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
        entry.insert("command".into(), json!(tool.argv()));
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
        entry.insert("command".into(), json!(tool.argv()));
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
    /// What runs `path`, when it is a script: a tool whose interpreter is
    /// missing, or does not run on the image, is `unavailable` too.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub interpreter: Option<String>,
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
///
/// An interpreter has to run, not just be there: the baked node is linked
/// against the harness image's C++ runtime, which a repository's image may
/// not have, and a node that cannot start leaves its tools as absent as a
/// missing file would.
fn probe_script(paths: &[String], interpreters: &[String]) -> String {
    let quote = |paths: &[String]| {
        paths
            .iter()
            .map(|p| format!("'{}'", p.replace('\'', "'\\''")))
            .collect::<Vec<_>>()
            .join(" ")
    };
    let mut script = format!(
        "sed -n 's/.*\"revision\": *\"\\([^\"]*\\)\".*/rev \\1/p' {MANIFEST_PATH} 2>/dev/null | head -n 1; \
         for p in {}; do if [ -x \"$p\" ]; then echo \"ok $p\"; else echo \"missing $p\"; fi; done",
        quote(paths)
    );
    if !interpreters.is_empty() {
        script.push_str(&format!(
            "; for p in {}; do if \"$p\" --version >/dev/null 2>&1; then echo \"runs $p\"; \
             else echo \"broken $p\"; fi; done",
            quote(interpreters)
        ));
    }
    script
}

/// Turn the probe's output into a status. A path the probe said nothing about
/// — a probe that could not run at all — is `unavailable`: an unanswered
/// question about the image is not a yes.
fn read_probe(out: &str) -> ToolchainStatus {
    let profile = profile();
    let mut present = std::collections::BTreeSet::new();
    let mut runs = std::collections::BTreeSet::new();
    let mut image_revision = None;
    for line in out.lines() {
        if let Some(rev) = line.strip_prefix("rev ") {
            image_revision = Some(rev.trim().to_string());
        } else if let Some(path) = line.strip_prefix("ok ") {
            present.insert(path.trim().to_string());
        } else if let Some(path) = line.strip_prefix("runs ") {
            runs.insert(path.trim().to_string());
        }
    }
    let mut tools = Vec::new();
    for (kind, list) in [("lsp", &profile.lsp), ("formatter", &profile.formatter)] {
        for tool in list {
            let path = tool.binary().to_string();
            let interpreter = tool.interpreter().map(str::to_string);
            let usable = present.contains(&path)
                && interpreter
                    .as_ref()
                    .is_none_or(|interpreter| runs.contains(interpreter));
            tools.push(ToolStatus {
                id: tool.id.clone(),
                kind,
                version: tool.version.clone(),
                state: match usable {
                    true => ToolState::Configured,
                    false => ToolState::Unavailable,
                },
                path,
                interpreter,
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
            interpreter: None,
            state: ToolState::Disabled,
        });
    }
    for id in &profile.disabled_formatter {
        tools.push(ToolStatus {
            id: id.clone(),
            kind: "formatter",
            version: String::new(),
            path: String::new(),
            interpreter: None,
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
    let tools = || profile.lsp.iter().chain(profile.formatter.iter());
    let paths: Vec<String> = tools().map(|tool| tool.binary().to_string()).collect();
    let mut interpreters: Vec<String> = tools()
        .filter_map(|tool| tool.interpreter().map(str::to_string))
        .collect();
    interpreters.sort();
    interpreters.dedup();
    let out = runner
        .run_probe(RunnerCommand {
            argv: vec![
                "sh".into(),
                "-c".into(),
                probe_script(&paths, &interpreters),
            ],
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

    const NODE: &str = "/opt/tracon/bin/node";

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
        // A JavaScript tool names the baked node in front of itself, so it
        // never runs on a `node` a repository's PATH puts first; a native one
        // names nothing in front.
        for tool in profile.lsp.iter().chain(profile.formatter.iter()) {
            let javascript = matches!(tool.id.as_str(), "typescript" | "prettier");
            assert_eq!(
                tool.interpreter(),
                javascript.then_some(NODE),
                "{} names the wrong interpreter",
                tool.id
            );
            // And the configuration spawns it in front of the tool.
            let (lsp, formatter) = lsp_formatter_config();
            let rendered = lsp
                .get(&tool.id)
                .or_else(|| formatter.get(&tool.id))
                .unwrap()["command"]
                .clone();
            assert_eq!(rendered, json!(tool.argv()), "{}", tool.id);
            assert_eq!(
                rendered[0].as_str().unwrap(),
                tool.interpreter().unwrap_or(tool.binary())
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
            for path in tool.interpreter().into_iter().chain([tool.binary()]) {
                assert!(
                    containerfile.contains(path),
                    "the Containerfile never mentions {path}"
                );
            }
        }
        // And the build checks the profile's revision against its own.
        assert!(containerfile.contains(&format!(
            "ARG TRACON_TOOLCHAIN_REVISION={}\n",
            profile().revision
        )));
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

    /// A JavaScript tool needs its script and the node in front of it, and a
    /// node that is there but cannot start (a repository's image without the
    /// C++ runtime it links) is as missing as one that is not.
    #[test]
    fn a_tool_whose_interpreter_does_not_run_is_unavailable() {
        let script = |id: &str| {
            profile()
                .lsp
                .iter()
                .chain(profile().formatter.iter())
                .find(|t| t.id == id)
                .unwrap()
                .binary()
                .to_string()
        };
        let all = format!(
            "rev 2\nok {}\nok {}\nok {}\nok {}\n",
            script("rust"),
            script("typescript"),
            script("rustfmt"),
            script("prettier")
        );
        let state = |status: &ToolchainStatus, id: &str| {
            status.tools.iter().find(|t| t.id == id).unwrap().state
        };

        let broken = read_probe(&format!("{all}broken {NODE}\n"));
        assert_eq!(state(&broken, "rust"), ToolState::Configured);
        assert_eq!(state(&broken, "rustfmt"), ToolState::Configured);
        assert_eq!(state(&broken, "typescript"), ToolState::Unavailable);
        assert_eq!(state(&broken, "prettier"), ToolState::Unavailable);
        assert!(!broken.complete());
        let typescript = broken.tools.iter().find(|t| t.id == "typescript").unwrap();
        assert_eq!(typescript.interpreter.as_deref(), Some(NODE));
        assert_eq!(
            typescript.path,
            "/opt/tracon/lsp/typescript-language-server"
        );

        // Not a word about it is no better.
        let unasked = read_probe(&all);
        assert_eq!(state(&unasked, "typescript"), ToolState::Unavailable);

        let running = read_probe(&format!("{all}runs {NODE}\n"));
        assert!(running.complete(), "{:?}", running.tools);

        // A native tool says nothing about an interpreter.
        let json = serde_json::to_value(&running.tools).unwrap();
        let rust = json
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["id"] == "rust")
            .unwrap();
        assert!(rust.get("interpreter").is_none());
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
        let script = probe_script(
            &["/opt/tracon/lsp/rust-analyzer".into(), "a'b".into()],
            &[NODE.into()],
        );
        assert!(script.contains("'/opt/tracon/lsp/rust-analyzer'"));
        assert!(script.contains("'a'\\''b'"));
        assert!(script.contains(MANIFEST_PATH));
        // The interpreter is run, not just looked at.
        assert!(script.contains(&format!("for p in '{NODE}'; do if \"$p\" --version")));
        assert!(!probe_script(&[], &[]).contains("--version"));
    }
}
