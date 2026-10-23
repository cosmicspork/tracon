//! What the retired harnesses left on disk, and getting rid of it deliberately.
//!
//! Removing an adapter does not remove the state its harness wrote. omp kept
//! a credential database in the node-owned harness-state directory, and after
//! the OpenCode cutover nothing reads it, nothing refreshes it, and nothing
//! would ever notice it was still there — which is the whole problem. A
//! credential that no longer has a client is a credential nobody is watching.
//!
//! So `tracon setup` retires it, by name, and says what it removed. Two things
//! it deliberately does not touch:
//!
//! * **The broker.** `credentials.sealed` holds the tokens the node lifted out
//!   of that database and has used ever since; they are provider credentials,
//!   not harness state, and the harness they were obtained through is
//!   irrelevant to them. Deleting them would sign the operator out of
//!   providers that still work.
//! * **Workspaces and session state.** Archiving a legacy session keeps them
//!   on purpose (`Store::archive_legacy_sessions`), and a reopened session is
//!   given its workspace back.
//!
//! The provider login volumes are no longer used at all; `tracon gc` lists
//! them for removal.
//!
//! `node.toml` is the other thing they left. Versions before the OpenCode
//! cutover wrote that file in full, so an operator who never touched
//! `[harness]` may still have `id = "omp"` in it, and an OpenCode node has
//! `id = "opencode"`; a node that refused to start on either would turn an
//! app update into a crash loop nobody chose. So a file that still names a
//! retired harness, or one of their images, is migrated as it is loaded
//! (`migrate_config`): the harness becomes Claude Code, and the image values
//! the retired harness's defaults put beside it become the current defaults —
//! only values that are recognisably those defaults, never something the
//! operator chose. The original is kept beside it. The sessions a retired
//! harness ran are not touched here: a node serves with them in place, and
//! archiving them stays the explicit `tracon session archive-legacy`.

use std::path::{Path, PathBuf};

use crate::config::Config;

/// One thing the retired harness left behind, and what became of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Artifact {
    pub path: PathBuf,
    /// What it was, for the operator reading the list.
    pub what: &'static str,
    /// False when it was not there, which is the ordinary case on a node that
    /// never ran omp or has already been migrated.
    pub removed: bool,
}

/// Everything the retired harness wrote into node-owned state, as paths
/// relative to the harness-state directory.
///
/// omp's state directory was mounted at the harness's `~/.omp`, and this is
/// the host side of that mount. Only omp's own subtree is named: the same
/// directory is now Claude Code's and tracon-agent's mount, and removing it whole
/// would take state a supported harness is using.
const ARTIFACTS: &[(&str, &str)] = &[(
    "agent",
    "omp's login credential database (agent.db) and the provider document it persisted (models.yml)",
)];

/// Remove them, and report what was there. Idempotent: a second run finds
/// nothing and says so.
pub fn retire_credentials() -> Vec<Artifact> {
    retire_credentials_in(&Config::harness_state_dir())
}

fn retire_credentials_in(harness_state: &Path) -> Vec<Artifact> {
    ARTIFACTS
        .iter()
        .map(|(relative, what)| {
            let path = harness_state.join(relative);
            // `exists()` first, so "removed" means this run removed it rather
            // than that the removal did not error on an absent path.
            let removed = path.exists() && std::fs::remove_dir_all(&path).is_ok();
            Artifact {
                path,
                what,
                removed,
            }
        })
        .collect()
}

/// What `migrate_config` made of a `node.toml` that still named a retired
/// harness.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigMigration {
    /// The retired harness the file named, by id or by image.
    pub from: &'static str,
    /// The migrated file, as it is written back and then loaded.
    pub text: String,
    /// The keys it changed, dotted, for the operator reading the warning.
    pub changed: Vec<String>,
}

/// The repositories each retired harness's image defaults named: the local
/// build, and the published image the pod runtime pulled at a versioned tag.
const RETIRED_IMAGES: &[(&str, &[&str])] = &[
    (
        "omp",
        &[
            "localhost/tracon-harness",
            "ghcr.io/cosmicspork/tracon-harness",
        ],
    ),
    (
        "opencode",
        &[
            "localhost/tracon-harness-opencode",
            "ghcr.io/cosmicspork/tracon-harness-opencode",
        ],
    ),
];

/// The retired harness whose default image this is, if it is one.
fn retired_image(value: &str) -> Option<&'static str> {
    RETIRED_IMAGES.iter().find_map(|(harness, repos)| {
        repos
            .iter()
            .any(|repo| {
                value.strip_prefix(repo).is_some_and(|rest| {
                    rest.is_empty() || rest.starts_with(':') || rest.starts_with('@')
                })
            })
            .then_some(*harness)
    })
}

fn is_retired_image(value: &str) -> bool {
    retired_image(value).is_some()
}

fn table<'a>(doc: &'a mut toml::Table, path: &[&str]) -> Option<&'a mut toml::Table> {
    path.iter()
        .try_fold(doc, |t, key| t.get_mut(*key)?.as_table_mut())
}

/// The retired harness the image at `path` names, if it does.
fn image_retired_by(doc: &mut toml::Table, path: &[&str]) -> Option<&'static str> {
    table(doc, path)?
        .get("harness_image")?
        .as_str()
        .and_then(retired_image)
}

fn replace_retired_image(
    doc: &mut toml::Table,
    path: &[&str],
    current: &str,
    changed: &mut Vec<String>,
) {
    let Some(image) = table(doc, path).and_then(|t| t.get_mut("harness_image")) else {
        return;
    };
    if image.as_str().is_some_and(is_retired_image) {
        *image = toml::Value::String(current.to_string());
        changed.push(format!("{}.harness_image", path.join(".")));
    }
}

/// Migrate the text of a `node.toml` whose `[harness] id`, or whose harness
/// image, is a retired harness's. None for every other file — including one
/// that does not parse, which the ordinary load reports with its line and
/// column.
///
/// Only keys the file already has are touched, so a key the operator left out
/// keeps following the defaults; and only values that are a retired
/// harness's own defaults are replaced, so an image or host the operator
/// chose survives.
pub fn migrate_config(text: &str) -> Option<ConfigMigration> {
    let mut doc: toml::Table = toml::from_str(text).ok()?;
    let named = table(&mut doc, &["harness"])
        .and_then(|h| h.get("id"))
        .and_then(toml::Value::as_str)
        .and_then(|id| crate::adapter::RETIRED.iter().find(|r| **r == id).copied());
    let from = named
        .or_else(|| image_retired_by(&mut doc, &["boundary"]))
        .or_else(|| image_retired_by(&mut doc, &["runtime", "kubernetes"]))?;
    let mut changed = Vec::new();
    if named.is_some() {
        let harness = table(&mut doc, &["harness"])?;
        changed.push("harness.id".to_string());
        harness.insert(
            "id".into(),
            toml::Value::String(crate::adapter::claude::ClaudeAdapter::ID.into()),
        );
        // A retired harness's pinned version means nothing to Claude Code;
        // empty is the version this node's Claude Code image installs.
        if let Some(version) = harness.get_mut("version") {
            if version.as_str().is_some_and(|v| !v.is_empty()) {
                *version = toml::Value::String(String::new());
                changed.push("harness.version".into());
            }
        }
    }

    let defaults = Config::default();
    replace_retired_image(
        &mut doc,
        &["boundary"],
        &defaults.boundary.harness_image,
        &mut changed,
    );
    replace_retired_image(
        &mut doc,
        &["runtime", "kubernetes"],
        &defaults.runtime.kubernetes.harness_image,
        &mut changed,
    );
    let text = toml::to_string_pretty(&doc).ok()?;
    Some(ConfigMigration {
        from,
        text,
        changed,
    })
}

/// Where the original of a `node.toml` migrated off `from` is kept:
/// `node.toml.pre-<from>`, so a node that has been through both retirements
/// keeps each original.
pub fn config_backup_path(path: &Path, from: &str) -> PathBuf {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "node.toml".into());
    path.with_file_name(format!("{name}.pre-{from}"))
}

/// Write a migration back, keeping the original beside it. An existing backup
/// is never replaced: it is the operator's real original, and a file put back
/// to omp by hand and migrated a second time is not.
pub fn persist_config_migration(
    path: &Path,
    migration: &ConfigMigration,
) -> std::io::Result<PathBuf> {
    let backup = config_backup_path(path, migration.from);
    if !backup.exists() {
        // A copy, so the backup keeps the original's permissions.
        std::fs::copy(path, &backup)?;
    }
    std::fs::write(path, &migration.text)?;
    Ok(backup)
}

/// Persist a migration and say so loudly. A file that cannot be written back
/// still loads migrated — the node runs either way — and the warning repeats
/// on every load until it can be.
pub fn apply_config_migration(path: &Path, migration: &ConfigMigration) {
    let changed = migration.changed.join(", ");
    let from = migration.from;
    let next = "run `tracon setup` to build the harness images, and \
                `tracon session archive-legacy` to put the retired harness's sessions away read-only";
    match persist_config_migration(path, migration) {
        Ok(backup) => tracing::warn!(
            path = %path.display(),
            backup = %backup.display(),
            %changed,
            "node.toml named the retired `{from}` harness, so it was migrated to `claude`; {next}"
        ),
        Err(error) => tracing::warn!(
            path = %path.display(),
            %changed,
            %error,
            "node.toml names the retired `{from}` harness; running as `claude`, but the migration could not be written back; {next}"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The state a supported harness is using shares the directory, so the
    /// removal has to be by name. Taking the directory whole would delete
    /// whatever Claude Code or tracon-agent keeps there.
    #[test]
    fn only_the_retired_harnesss_own_subtree_is_removed() {
        let dir = std::env::temp_dir().join(format!("tracon-legacy-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(dir.join("agent")).unwrap();
        std::fs::write(dir.join("agent/agent.db"), b"credentials").unwrap();
        std::fs::create_dir_all(dir.join("statsig")).unwrap();
        std::fs::write(dir.join("statsig/keep"), b"not omp's").unwrap();

        let report = retire_credentials_in(&dir);
        assert_eq!(report.len(), 1);
        assert!(report[0].removed);
        assert!(report[0].path.ends_with("agent"));
        assert!(!dir.join("agent").exists());
        assert!(dir.join("statsig/keep").exists());

        // Idempotent: nothing left to remove, and it says so rather than
        // reporting a second removal that did not happen.
        let again = retire_credentials_in(&dir);
        assert!(!again[0].removed);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// What a node.toml written in full by a pre-cutover version looks like,
    /// trimmed to the keys the migration reads plus a few it must leave alone,
    /// and one host the operator added.
    const PRE_CUTOVER: &str = r#"
node_name = "laptop"

[harness]
id = "omp"
version = "18.0.4"
tools = []

[boundary]
network = "tracon-int"
gateway_image = "localhost/tracon-gateway"
harness_image = "localhost/tracon-harness"

[gateway]
allow_hosts = ['^api\.anthropic\.com$', '^api\.openai\.com$', '^chatgpt\.com$', '^auth\.openai\.com$', '^git\.internal\.example$']
proxy_port = 8888

[runtime.kubernetes]
harness_image = "ghcr.io/cosmicspork/tracon-harness:0.2.1"
state_claim = "tracon-state"
"#;

    #[test]
    fn a_pre_cutover_node_toml_becomes_claude_and_keeps_what_the_operator_chose() {
        let migration = migrate_config(PRE_CUTOVER).expect("an omp file migrates");
        assert_eq!(migration.from, "omp");
        let cfg: Config = toml::from_str(&migration.text).unwrap();
        let defaults = Config::default();

        assert_eq!(cfg.harness.id, "claude");
        assert_eq!(cfg.harness.version, "");
        assert_eq!(cfg.boundary.harness_image, defaults.boundary.harness_image);
        assert_eq!(
            cfg.runtime.kubernetes.harness_image,
            defaults.runtime.kubernetes.harness_image
        );
        // The migrated config resolves to an adapter: the node starts.
        assert!(crate::adapter::adapter_for(&cfg).is_ok());

        // The operator's egress, their own host included, is left as it was.
        assert_eq!(
            cfg.gateway.allow_hosts,
            [
                r"^api\.anthropic\.com$",
                r"^api\.openai\.com$",
                r"^chatgpt\.com$",
                r"^auth\.openai\.com$",
                r"^git\.internal\.example$",
            ]
        );

        // What the migration had no business with is exactly as it was.
        assert_eq!(cfg.node_name, "laptop");
        assert_eq!(cfg.gateway.proxy_port, 8888);

        assert_eq!(
            migration.changed,
            [
                "harness.id",
                "harness.version",
                "boundary.harness_image",
                "runtime.kubernetes.harness_image",
            ]
        );
        // And the result is not migrated a second time.
        assert_eq!(migrate_config(&migration.text), None);
    }

    /// An OpenCode node: the id and its image defaults move to Claude Code's,
    /// and an image the operator chose stays.
    #[test]
    fn an_opencode_node_toml_becomes_claude() {
        let text = r#"
[harness]
id = "opencode"
version = "1.18.30"
experimental = ["native"]

[boundary]
harness_image = "localhost/tracon-harness-opencode"

[runtime.kubernetes]
harness_image = "registry.example/team/harness:3"
"#;
        let migration = migrate_config(text).expect("an opencode file migrates");
        assert_eq!(migration.from, "opencode");
        let cfg: Config = toml::from_str(&migration.text).unwrap();
        assert_eq!(cfg.harness.id, "claude");
        assert_eq!(cfg.harness.version, "");
        assert_eq!(cfg.harness.experimental, ["native"]);
        assert_eq!(
            cfg.boundary.harness_image,
            Config::default().boundary.harness_image
        );
        assert_eq!(
            cfg.runtime.kubernetes.harness_image,
            "registry.example/team/harness:3"
        );
        assert!(crate::adapter::adapter_for(&cfg).is_ok());
        assert_eq!(
            migration.changed,
            ["harness.id", "harness.version", "boundary.harness_image"]
        );
        assert_eq!(migrate_config(&migration.text), None);
    }

    /// A node already on Claude Code whose file still names the OpenCode
    /// image, which an OpenCode-era version wrote in full: the image moves,
    /// nothing else does.
    #[test]
    fn a_retired_image_alone_is_migrated() {
        let text = r#"
[harness]
id = "claude"

[runtime.kubernetes]
harness_image = "ghcr.io/cosmicspork/tracon-harness-opencode:0.15.0"
"#;
        let migration = migrate_config(text).expect("a retired image migrates");
        assert_eq!(migration.from, "opencode");
        let cfg: Config = toml::from_str(&migration.text).unwrap();
        assert_eq!(cfg.harness.id, "claude");
        assert_eq!(
            cfg.runtime.kubernetes.harness_image,
            Config::default().runtime.kubernetes.harness_image
        );
        assert_eq!(migration.changed, ["runtime.kubernetes.harness_image"]);
    }

    #[test]
    fn only_a_retired_harnesss_own_image_defaults_are_replaced() {
        let text = r#"
[harness]
id = "omp"

[boundary]
harness_image = "registry.example/team/harness:3"
"#;
        let migration = migrate_config(text).unwrap();
        let cfg: Config = toml::from_str(&migration.text).unwrap();
        assert_eq!(cfg.harness.id, "claude");
        assert_eq!(
            cfg.boundary.harness_image,
            "registry.example/team/harness:3"
        );
        // Keys the file never had keep following the defaults.
        assert_eq!(
            cfg.gateway.allow_hosts,
            Config::default().gateway.allow_hosts
        );
        assert_eq!(migration.changed, ["harness.id"]);

        assert!(is_retired_image("localhost/tracon-harness"));
        assert!(is_retired_image("ghcr.io/cosmicspork/tracon-harness:0.2.1"));
        assert!(is_retired_image(
            "ghcr.io/cosmicspork/tracon-harness@sha256:abc"
        ));
        assert!(!is_retired_image("localhost/tracon-harness-claude"));
        assert!(!is_retired_image("localhost/tracon-harness-native"));
        assert_eq!(
            retired_image("ghcr.io/cosmicspork/tracon-harness-opencode:0.16.0"),
            Some("opencode")
        );
        assert_eq!(retired_image("localhost/tracon-harness"), Some("omp"));
    }

    #[test]
    fn supported_unknown_and_unparseable_files_are_not_migrated() {
        assert_eq!(migrate_config("[harness]\nid = \"native\"\n"), None);
        assert_eq!(
            migrate_config("[harness]\nid = \"claude\"\nversion = \"2.0.0\"\n"),
            None
        );
        // An unknown id is still the adapter's refusal, not a guess.
        assert_eq!(migrate_config("[harness]\nid = \"aider\"\n"), None);
        assert_eq!(migrate_config("node_name = \"n\"\n"), None);
        assert_eq!(migrate_config("[harness\nid = \"omp\"\n"), None);
    }

    /// A pod-hosted node: the `tracon-node` image reading a node.toml an older
    /// version wrote onto its state volume. Nobody edits that file by hand,
    /// so the next release has to come up healthy on the migration alone.
    #[test]
    fn a_pre_cutover_kubernetes_node_comes_up_on_the_published_claude_image() {
        let dir = std::env::temp_dir().join(format!("tracon-legacy-k8s-{}", uuid::Uuid::now_v7()));
        let path = dir.join("state/config/tracon/node.toml");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let original = r#"
node_name = "tracon-node-0"

[harness]
id = "omp"
version = "18.0.4"

[boundary]
harness_image = "localhost/tracon-harness"

[gateway]
allow_hosts = ['^api\.anthropic\.com$', '^api\.openai\.com$', '^chatgpt\.com$', '^auth\.openai\.com$']

[runtime]
kind = "kubernetes"

[runtime.kubernetes]
harness_image = "ghcr.io/cosmicspork/tracon-harness:0.12.1"
state_claim = "tracon-state"
"#;
        std::fs::write(&path, original).unwrap();

        let cfg = Config::try_load_from(&path).unwrap();
        assert!(crate::adapter::adapter_for(&cfg).is_ok());
        assert_eq!(cfg.harness.id, "claude");
        assert_eq!(
            crate::adapter::pinned_version(&cfg),
            crate::adapter::image_version("claude")
        );
        assert_eq!(cfg.runtime.kind, crate::config::RuntimeKind::Kubernetes);
        assert_eq!(
            cfg.runtime.kubernetes.harness_image,
            format!(
                "ghcr.io/cosmicspork/tracon-harness-claude:{}",
                env!("CARGO_PKG_VERSION")
            )
        );
        assert_eq!(cfg.runtime.kubernetes.state_claim, "tracon-state");
        assert_eq!(
            cfg.boundary.harness_image,
            "localhost/tracon-harness-claude"
        );
        assert_eq!(
            cfg.gateway.allow_hosts,
            [
                r"^api\.anthropic\.com$",
                r"^api\.openai\.com$",
                r"^chatgpt\.com$",
                r"^auth\.openai\.com$",
            ]
        );

        // Written back onto the volume, with the original beside it, so the
        // next start (and every one after) loads it without migrating again.
        let written = std::fs::read_to_string(&path).unwrap();
        assert_eq!(migrate_config(&written), None);
        assert_eq!(
            std::fs::read_to_string(path.with_file_name("node.toml.pre-omp")).unwrap(),
            original
        );
        let again = Config::try_load_from(&path).unwrap();
        assert_eq!(
            again.runtime.kubernetes.harness_image,
            cfg.runtime.kubernetes.harness_image
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn loading_a_pre_cutover_file_persists_the_migration_beside_a_backup() {
        let dir = std::env::temp_dir().join(format!("tracon-legacy-cfg-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("node.toml");
        std::fs::write(&path, PRE_CUTOVER).unwrap();

        let cfg = Config::try_load_from(&path).unwrap();
        assert_eq!(cfg.harness.id, "claude");
        let backup = dir.join("node.toml.pre-omp");
        assert_eq!(std::fs::read_to_string(&backup).unwrap(), PRE_CUTOVER);
        let written = std::fs::read_to_string(&path).unwrap();
        assert_eq!(migrate_config(&written), None, "{written}");
        assert_eq!(
            toml::from_str::<Config>(&written).unwrap().harness.id,
            "claude"
        );

        // Put back to omp by hand and loaded again: migrated again, but the
        // backup is still the real original.
        std::fs::write(&path, "[harness]\nid = \"omp\"\n").unwrap();
        assert_eq!(Config::try_load_from(&path).unwrap().harness.id, "claude");
        assert_eq!(std::fs::read_to_string(&backup).unwrap(), PRE_CUTOVER);

        // An OpenCode node's file keeps its own original beside it.
        std::fs::write(&path, "[harness]\nid = \"opencode\"\n").unwrap();
        assert_eq!(Config::try_load_from(&path).unwrap().harness.id, "claude");
        assert_eq!(
            std::fs::read_to_string(dir.join("node.toml.pre-opencode")).unwrap(),
            "[harness]\nid = \"opencode\"\n"
        );

        // An unknown id is refused when the file loads.
        std::fs::write(&path, "[harness]\nid = \"aider\"\n").unwrap();
        let err = Config::try_load_from(&path).unwrap_err();
        assert!(err.contains("aider"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
