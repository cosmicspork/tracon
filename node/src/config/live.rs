//! What a running node reads from `node.toml` after it has started.
//!
//! The node loads its configuration once, into an `Arc<Config>` that every
//! part of it holds. A key the schema calls live (`config_schema::Apply`) is
//! read through [`Config::live`] at the moment it is used, so an edit applies
//! to the next session, turn, check or push without a restart. Every other
//! key keeps the value the node started with, and a change to one is owed a
//! restart, which [`pending_restart`] names.
//!
//! The newest configuration is never handed out whole. What `live()` returns
//! is the configuration the node started with, with only the live keys taken
//! from the newest file ([`overlay`]), so a reader that reaches a restart key
//! through it still sees what the node is actually running. That is what
//! makes it safe to pass `&cfg.live()` to code that reads both kinds.

use std::sync::Arc;

use super::{edit, Config};
use crate::config_schema::{self, Apply, KEYS};

/// The newest configuration a running node has taken in, shared by every
/// holder of the `Config` it started with.
///
/// It lives inside that `Config` (a `#[serde(skip)]` field) rather than
/// beside it, because the node's parts each hold the `Arc<Config>` they were
/// given at start: the manager, the tools, the notifier, the mesh client.
/// Carried in the config, the handle reaches all of them without a second
/// type threaded through each.
#[derive(Debug, Default)]
pub struct ConfigHandle {
    current: parking_lot::RwLock<Option<Arc<Config>>>,
    boot: parking_lot::RwLock<Option<Boot>>,
    /// The hash of the file `current` was taken from.
    applied: parking_lot::RwLock<Option<String>>,
}

/// The file as the node loaded it at start.
#[derive(Debug, Clone)]
struct Boot {
    text: String,
    hash: String,
}

impl Clone for ConfigHandle {
    /// A copy of a config carries what it saw and is on its own from then on,
    /// as `LiveRepos` is: a clone made to change one setting for one caller
    /// must not have a later edit reach under it.
    fn clone(&self) -> Self {
        Self {
            current: parking_lot::RwLock::new(self.current.read().clone()),
            boot: parking_lot::RwLock::new(self.boot.read().clone()),
            applied: parking_lot::RwLock::new(self.applied.read().clone()),
        }
    }
}

impl ConfigHandle {
    fn current(&self) -> Option<Arc<Config>> {
        self.current.read().clone()
    }
}

/// A configuration a running node reads from: the one it started with, or
/// that one with the live keys of a newer file laid over it.
pub enum Live<'a> {
    Boot(&'a Config),
    Newer(Arc<Config>),
}

impl std::ops::Deref for Live<'_> {
    type Target = Config;

    fn deref(&self) -> &Config {
        match self {
            Live::Boot(cfg) => cfg,
            Live::Newer(cfg) => cfg,
        }
    }
}

impl Config {
    /// The configuration as it applies now: live keys from the newest file
    /// the node has taken in, every other key as the node started.
    pub fn live(&self) -> Live<'_> {
        match self.live.current() {
            Some(newer) => Live::Newer(newer),
            None => Live::Boot(self),
        }
    }

    /// Record the file this node started from, so a later change to a key
    /// that waits for a restart can be named. `serve` calls it once.
    pub fn set_boot_text(&self, text: &str) {
        *self.live.boot.write() = Some(Boot {
            text: text.to_string(),
            hash: edit::hash(text.as_bytes()),
        });
    }

    /// The hash of the file this node started from, when `serve` recorded it.
    pub fn boot_hash(&self) -> Option<String> {
        self.live.boot.read().as_ref().map(|boot| boot.hash.clone())
    }

    /// Take in `newer`, the file as it now loads (its hash `hash`): its live
    /// keys apply from here on, and the `[[repo]]` table is replaced whole,
    /// as a save of it always did. Called on the node's own `Arc<Config>`
    /// after an edit, and when the file changed under the node.
    pub fn apply_live(&self, newer: &Config, hash: &str) {
        match overlay(self, newer) {
            Ok(merged) => {
                self.live_repo.set(merged.repo.clone());
                *self.live.current.write() = Some(Arc::new(merged));
                *self.live.applied.write() = Some(hash.to_string());
            }
            Err(error) => {
                tracing::error!(%error, "node.toml changed, but its live keys could not be applied; they wait for a restart")
            }
        }
    }

    /// What an edit of the file left, applied to this running node.
    pub fn apply_edit(&self, outcome: &edit::EditOutcome) {
        if outcome.wrote() {
            self.apply_live(&outcome.config, &outcome.new_hash);
        }
    }

    /// Take in the file at `path` as it is now, if it differs from what was
    /// last taken in and it loads. An edit made by another process (the
    /// CLI, an editor) is noticed this way; the node's own edits apply as
    /// they land.
    pub fn refresh_live_from(&self, path: &std::path::Path) {
        let Ok(hash) = edit::file_hash(path) else {
            return;
        };
        let seen = self
            .live
            .applied
            .read()
            .clone()
            .or_else(|| self.boot_hash());
        if seen.as_deref() == Some(hash.as_str()) {
            return;
        }
        match edit::snapshot_at(path) {
            Ok(snapshot) => {
                tracing::info!(hash = %snapshot.hash, "node.toml changed outside the node; applying its live keys");
                self.apply_live(&snapshot.config, &snapshot.hash);
            }
            // Said once per version, not every look.
            Err(error) => {
                tracing::warn!(%error, "node.toml changed outside the node and does not load; nothing applied");
                *self.live.applied.write() = Some(hash);
            }
        }
    }
}

/// The restart-only keys whose values differ between the file the node
/// started from and `now`: what a restart would change. Live keys are left
/// out, since they already apply.
pub fn pending_restart(boot: &str, now: &str) -> Vec<String> {
    // An empty table written or removed (`[gateway]` left behind when its
    // last key went) changes nothing the node reads.
    let nothing = |value: &Option<toml::Value>| match value {
        None => true,
        Some(toml::Value::Table(table)) => table.is_empty(),
        Some(_) => false,
    };
    let mut keys: Vec<String> = edit::diff(boot, now)
        .into_iter()
        .filter(|change| !(nothing(&change.before) && nothing(&change.after)))
        .filter(|change| config_schema::lookup(&change.key.schema_key()).apply == Apply::Restart)
        .map(|change| change.key.to_string())
        .collect();
    keys.dedup();
    keys
}

/// What the node's own config says a restart would change, read against the
/// file at `path` now. `None` when `serve` never recorded the file it
/// started from (a test's node), or the file cannot be read.
pub fn pending_restart_at(cfg: &Config, path: &std::path::Path) -> Option<PendingRestart> {
    let boot = cfg.live.boot.read().clone()?;
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(_) => return None,
    };
    let hash = edit::hash(&bytes);
    let text = String::from_utf8_lossy(&bytes);
    let unparseable = text.parse::<toml::Table>().is_err();
    let keys = match hash == boot.hash || unparseable {
        true => Vec::new(),
        false => pending_restart(&boot.text, &text),
    };
    Some(PendingRestart {
        boot_hash: boot.hash,
        hash,
        keys,
        unparseable,
    })
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct PendingRestart {
    /// The file the node started from.
    pub boot_hash: String,
    /// The file now: what a restart would load, and what a restart request
    /// names as the version it was decided against.
    pub hash: String,
    /// Restart-only keys that differ, as the file spells them.
    pub keys: Vec<String>,
    /// The file does not parse now; a restart would not start.
    pub unparseable: bool,
}

/// `boot` with the live keys of `newer` laid over it. Done on the files'
/// own shape (each `Config` serialized), walking the schema, so that what is
/// live is decided in one place, `config_schema::KEYS`, and nowhere else.
pub fn overlay(boot: &Config, newer: &Config) -> Result<Config, String> {
    let as_table = |cfg: &Config| match toml::Value::try_from(cfg) {
        Ok(toml::Value::Table(table)) => Ok(table),
        Ok(_) => Err("a configuration is not a table".to_string()),
        Err(error) => Err(error.to_string()),
    };
    let mut base = as_table(boot)?;
    let next = as_table(newer)?;
    overlay_table(&mut base, &next, &mut Vec::new());
    let mut merged: Config = toml::Value::Table(base)
        .try_into()
        .map_err(|e: toml::de::Error| e.to_string())?;
    // What is not in the file stays as the node started.
    merged.declared_providers = boot.declared_providers.clone();
    Ok(merged)
}

/// How the schema's keys at or under `path` apply.
#[derive(Debug, PartialEq, Eq)]
enum Under {
    /// Every key there is live: the whole value is taken, entries added and
    /// removed included.
    Live,
    /// None is.
    Restart,
    /// Some are: look inside.
    Mixed,
}

fn under(path: &[String]) -> Under {
    let matches = |pattern: &str| {
        let parts: Vec<&str> = pattern.split('.').collect();
        parts.len() >= path.len()
            && path
                .iter()
                .zip(&parts)
                .all(|(segment, part)| *part == "*" || part == segment)
    };
    let (mut live, mut restart) = (false, false);
    for spec in KEYS.iter().filter(|spec| matches(spec.key)) {
        match spec.apply {
            Apply::Live => live = true,
            Apply::Restart => restart = true,
        }
    }
    match (live, restart) {
        (true, false) => Under::Live,
        (true, true) => Under::Mixed,
        _ => Under::Restart,
    }
}

fn overlay_table(base: &mut toml::Table, next: &toml::Table, path: &mut Vec<String>) {
    let keys: Vec<String> = base
        .keys()
        .chain(next.keys().filter(|k| !base.contains_key(*k)))
        .cloned()
        .collect();
    for key in keys {
        path.push(key.clone());
        match under(path) {
            Under::Live => match next.get(&key) {
                Some(value) => {
                    base.insert(key.clone(), value.clone());
                }
                None => {
                    base.remove(&key);
                }
            },
            // Only into a table both sides have: a table the node did not
            // start with (a new provider) is a restart's to add.
            Under::Mixed => {
                if let (Some(toml::Value::Table(into)), Some(toml::Value::Table(from))) =
                    (base.get_mut(&key), next.get(&key))
                {
                    overlay_table(into, from, path);
                }
            }
            Under::Restart => {}
        }
        path.pop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn load(text: &str) -> Config {
        Config::from_toml(text, std::path::Path::new("node.toml")).unwrap()
    }

    #[test]
    fn a_config_laid_over_itself_is_unchanged() {
        for text in [
            "",
            "[session]\nbudget_tokens = 5\n[[repo]]\npath = \"/src/a\"\nchecks = [\"x\"]\n\
             [providers.anthropic]\nprice = { input_per_mtok = 1.0, output_per_mtok = 2.0 }\n\
             [github.dispatch]\n\"o/n\" = [\"ci.yml\"]\n",
            // Every section a pod-hosted node writes, so a type that does not
            // survive the trip through its own serialization is caught here.
            "node_name = \"work\"\n[runtime]\nkind = \"kubernetes\"\napproved_images = [\"x:1\"]\n\
             [runtime.kubernetes]\nnamespace = \"tracon-node\"\ninternal_cidrs = [\"10.0.0.0/8\"]\n\
             image_pull_secrets = [\"docr\"]\n\
             [runtime.kubernetes.resources.session]\nmemory_request = \"512Mi\"\n\
             [gateway]\nharness_listen = \"0.0.0.0:7421\"\nallow_hosts = [\"example.com\"]\n\
             [session]\nworktree_root = \"/state/work\"\n\
             [consulta]\ncommand = \"uv\"\nargs = [\"run\"]\n\
             [auth]\npublic_origin = \"https://tracon.example.com\"\n\
             [ui]\nopencode_url = \"https://ui.example.com\"\n\
             [docs]\nexport_dir = \"/state/docs\"\n\
             [embed]\nenabled = true\n\
             [mesh]\nhub_url = \"https://hub.example.com\"\n\
             [[service]]\nname = \"pg\"\nimage = \"postgres@sha256:0000000000000000000000000000000000000000000000000000000000000000\"\nport = 5432\n\
             [supervision]\nchecks = [\"just check\"]\n[supervision.dependency_inputs]\nlock = \"Cargo.lock\"\n",
        ] {
            let cfg = load(text);
            let merged = overlay(&cfg, &cfg).unwrap();
            assert_eq!(
                toml::Value::try_from(&merged).unwrap(),
                toml::Value::try_from(&cfg).unwrap(),
                "{text}"
            );
        }
    }

    #[test]
    fn only_live_keys_are_taken_from_the_newer_file() {
        let boot =
            load("node_name = \"a\"\n[session]\nbudget_tokens = 5\nworktree_root = \"/w\"\n");
        let newer = load(
            "node_name = \"b\"\n[session]\nbudget_tokens = 9\nworktree_root = \"/elsewhere\"\n\
             [notify]\ncontact = \"mailto:ops@example.com\"\n\
             [[repo]]\npath = \"/src/new\"\n\
             [providers.anthropic]\nprice = { input_per_mtok = 3.0, output_per_mtok = 4.0 }\n\
             [providers.brandnew]\ncredential = \"x\"\nupstream = \"https://x.example\"\n",
        );
        let merged = overlay(&boot, &newer).unwrap();
        assert_eq!(merged.session.budget_tokens, 9);
        assert_eq!(
            merged.notify.contact.as_deref(),
            Some("mailto:ops@example.com")
        );
        assert_eq!(merged.repo.len(), 1);
        assert_eq!(
            merged.providers["anthropic"]
                .price
                .map(|p| p.input_per_mtok),
            Some(3.0)
        );
        // Restart keys stay as the node started.
        assert_eq!(merged.node_name, "a");
        assert_eq!(merged.session.worktree_root, std::path::Path::new("/w"));
        assert!(!merged.providers.contains_key("brandnew"));

        // And a live value removed from the file goes back to its default.
        let again = overlay(&merged, &boot).unwrap();
        assert_eq!(again.notify.contact, None);
        assert!(again.repo.is_empty());
    }

    #[test]
    fn a_handle_serves_the_newer_file_to_every_holder_and_not_to_copies() {
        let cfg = Arc::new(load("[session]\nbudget_tokens = 5\n"));
        let shared = cfg.clone();
        let copied = (*cfg).clone();
        cfg.apply_live(
            &load("[session]\nbudget_tokens = 7\nworktree_root = \"/x\"\n"),
            "h",
        );
        assert_eq!(shared.live().session.budget_tokens, 7);
        assert_ne!(
            shared.live().session.worktree_root,
            std::path::Path::new("/x")
        );
        assert_eq!(copied.live().session.budget_tokens, 5);
        // The boot value itself is untouched: `cfg.session` is what started.
        assert_eq!(cfg.session.budget_tokens, 5);
    }

    #[test]
    fn only_restart_keys_are_pending() {
        let boot = "node_name = \"a\"\n[session]\nbudget_tokens = 5\n";
        let now = "node_name = \"b\"\n[session]\nbudget_tokens = 9\n[gateway]\nproxy_port = 1\n";
        assert_eq!(
            pending_restart(boot, now),
            vec!["node_name".to_string(), "gateway.proxy_port".to_string()]
        );
        assert!(
            pending_restart(boot, "node_name = \"a\"\n[session]\nbudget_tokens = 1\n").is_empty()
        );
    }
}
