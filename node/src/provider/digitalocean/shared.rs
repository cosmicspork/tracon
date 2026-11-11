//! A builder's side of templates made for peers: a node with `[digitalocean]
//! registry` (an amd64 Podman node, or a Kubernetes node with BuildKit) that
//! another node asks, by portable key ([`super::portable`]), to make a
//! template (`do_template_prepare`).
//!
//! It prepares the repository at its own default branch, builds, pushes and
//! registers the template under the key's team-wide name, and says how each
//! one it holds stands in its hello ([`Advert`]), which is the only word a
//! peer takes of it. It refuses a key that differs from its own for the
//! repository, so a template never holds a recipe the asking node did not
//! describe; the recipe itself reaches it only as an offer its operator
//! accepted (`mesh::recipe`). The request runs under this node's own policy,
//! as a forwarded command does.
//!
//! An image a peer asked for is named for its key here too, so the builder's
//! own sessions in it start from the same template rather than a second one.
//! The template is this node's to sweep, and it keeps it while any peer has
//! asked for it within [`PEER_KEEP`]: a peer asks again as it uses it.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use super::portable;
use super::templates::TemplateState;

/// How long a template made for peers is kept after the last peer asked
/// for it.
pub const PEER_KEEP: Duration = Duration::from_secs(14 * 24 * 3600);

/// The file under the state directory that holds what peers asked for.
pub const FILE: &str = "digitalocean/shared-templates.json";

/// One template made, or being made, for peers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Record {
    pub key: String,
    /// Its team-wide name.
    pub name: String,
    /// The repository's forge identity; empty for the stock base.
    #[serde(default)]
    pub repository: String,
    /// The local image it is built from, `name@sha256:…`; empty until the
    /// preparation has made it.
    #[serde(default)]
    pub image: String,
    /// What this node would name the image's template of its own when it
    /// was taken for the key: once that moves (a release, a service bundle),
    /// the image's template is this node's own again.
    #[serde(default)]
    pub own: String,
    pub asked_ms: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failed: Option<String>,
}

/// The templates made for peers, kept on disk so a restart neither forgets
/// which it registered nor sweeps one a peer still asks for.
pub struct Shared {
    path: Option<PathBuf>,
    records: Mutex<Vec<Record>>,
}

impl Shared {
    pub fn in_memory() -> Self {
        Shared {
            path: None,
            records: Mutex::new(Vec::new()),
        }
    }

    /// Read from `path`; one that is missing or unreadable is empty.
    pub fn load(path: PathBuf) -> Self {
        let records = std::fs::read(&path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Vec<Record>>(&bytes).ok())
            .unwrap_or_default()
            .into_iter()
            .filter(|r| portable::valid_key(&r.key))
            .collect();
        Shared {
            path: Some(path),
            records: Mutex::new(records),
        }
    }

    fn save(&self, records: &[Record]) {
        let Some(path) = &self.path else { return };
        let write = || -> std::io::Result<()> {
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir)?;
            }
            let tmp = path.with_extension("json.tmp");
            std::fs::write(&tmp, serde_json::to_vec_pretty(records)?)?;
            std::fs::rename(&tmp, path)
        };
        if let Err(error) = write() {
            tracing::warn!(%error, path = %path.display(), "templates made for peers not saved");
        }
    }

    fn update<T>(&self, change: impl FnOnce(&mut Vec<Record>) -> T) -> T {
        let mut records = self.records.lock().unwrap();
        let out = change(&mut records);
        self.save(&records);
        out
    }

    pub fn records(&self) -> Vec<Record> {
        self.records.lock().unwrap().clone()
    }

    pub fn get(&self, key: &str) -> Option<Record> {
        self.records
            .lock()
            .unwrap()
            .iter()
            .find(|r| r.key == key)
            .cloned()
    }

    /// A peer asked for `key` at `now`: recorded, named under `team_stem`,
    /// and any failure cleared so it is tried again. The record as it now
    /// stands.
    pub fn ask(&self, team_stem: &str, key: &str, repository: &str, now: i64) -> Record {
        self.update(|records| match records.iter_mut().find(|r| r.key == key) {
            Some(record) => {
                record.asked_ms = now;
                record.clone()
            }
            None => {
                let record = Record {
                    key: key.to_string(),
                    name: portable::team_name(team_stem, key),
                    repository: repository.to_string(),
                    image: String::new(),
                    own: String::new(),
                    asked_ms: now,
                    failed: None,
                };
                records.push(record.clone());
                record
            }
        })
    }

    /// Take a failed record up again.
    pub fn retry(&self, key: &str) {
        self.update(|records| {
            if let Some(record) = records.iter_mut().find(|r| r.key == key) {
                record.failed = None;
            }
        })
    }

    /// `image`, whose template of this node's own would be `own`, is the
    /// one `key`'s template is built from.
    pub fn adopt(&self, key: &str, image: &str, own: &str) {
        self.update(|records| {
            if let Some(record) = records.iter_mut().find(|r| r.key == key) {
                record.image = image.to_string();
                record.own = own.to_string();
                record.failed = None;
            }
        })
    }

    pub fn fail(&self, key: &str, reason: &str) {
        self.update(|records| {
            if let Some(record) = records.iter_mut().find(|r| r.key == key) {
                record.failed = Some(reason.to_string());
            }
        })
    }

    /// The team-wide name `image` (by digest) has while what this node adds
    /// to it is as it was when a peer's key was taken for it (`own`): the
    /// most recently asked of them.
    pub fn name_for(&self, digest: &str, own: &str) -> Option<String> {
        self.records
            .lock()
            .unwrap()
            .iter()
            .filter(|r| r.own == own && super::templates::digest_of(&r.image).ok() == Some(digest))
            .max_by_key(|r| r.asked_ms)
            .map(|r| r.name.clone())
    }

    /// The templates some peer asked for within [`PEER_KEEP`] of `now`.
    pub fn wanted(&self, now: i64) -> Vec<String> {
        let keep = PEER_KEEP.as_millis() as i64;
        self.records
            .lock()
            .unwrap()
            .iter()
            .filter(|r| now - r.asked_ms < keep)
            .map(|r| r.name.clone())
            .collect()
    }

    /// Whether this node registered (or is registering) the template `name`.
    pub fn holds(&self, name: &str) -> bool {
        self.records.lock().unwrap().iter().any(|r| r.name == name)
    }

    /// Forget the templates `names`, swept.
    pub fn forget(&self, names: &[String]) {
        if names.is_empty() {
            return;
        }
        self.update(|records| records.retain(|r| !names.contains(&r.name)))
    }

    /// How each one stands, for this node's hello.
    pub fn adverts(&self, state_of: impl Fn(&str) -> Option<TemplateState>) -> Vec<Advert> {
        self.records
            .lock()
            .unwrap()
            .iter()
            .map(|record| {
                let (state, reason) = match (&record.failed, state_of(&record.name)) {
                    (Some(reason), _) => (AdvertState::Failed, Some(reason.clone())),
                    (None, Some(TemplateState::Ready { .. })) => (AdvertState::Ready, None),
                    (None, Some(TemplateState::Building { .. })) => (AdvertState::Building, None),
                    (None, Some(TemplateState::Failed { reason })) => {
                        (AdvertState::Failed, Some(reason))
                    }
                    (None, _) => (AdvertState::Preparing, None),
                };
                Advert {
                    key: record.key.clone(),
                    name: record.name.clone(),
                    state,
                    reason,
                }
            })
            .collect()
    }
}

/// Where a template made for peers stands, as its builder says.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AdvertState {
    Preparing,
    Building,
    Ready,
    Failed,
}

/// One template a builder lists in its hello.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Advert {
    pub key: String,
    pub name: String,
    pub state: AdvertState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// What a builder carries in its hello (`do_templates`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Adverts {
    pub templates: Vec<Advert>,
}

/// What a builder needs to prepare a repository as its own operator would.
pub struct Builder {
    pub backend: Arc<dyn crate::boundary::Backend>,
    pub cfg: Arc<crate::config::Config>,
    pub store: Arc<crate::store::Store>,
    pub broker: crate::broker::SharedBroker,
    pub node_id: String,
}

/// A peer's `do_template_prepare`: answered at once with where the
/// template stands, the preparation running on after. Refused when this
/// node builds no templates, or holds no recipe for the repository.
pub fn prepare_for_peer(
    builder: Builder,
    sender: &str,
    repository: String,
    commit: String,
    key: String,
) -> Result<serde_json::Value, String> {
    let runtime = super::runtime::global()
        .filter(|_| super::runtime::enabled(&builder.cfg))
        .ok_or("this node runs no DigitalOcean provider")?;
    let templates = runtime.provider().templates().clone();
    if !matches!(templates.source(), super::templates::Source::Built { .. }) {
        return Err(
            "this node builds no DigitalOcean templates (no [digitalocean] registry)".into(),
        );
    }
    if !portable::valid_key(&key) {
        return Err("the template key is not 64 lower-case hex characters".into());
    }
    let repo = match repository.is_empty() {
        true => None,
        false => {
            crate::mesh::recipe::valid_identity(&repository)?;
            let path =
                crate::forge::session_repo(&crate::config::Config::state_dir(), &repository)?;
            let repos = builder.cfg.repos();
            let entry = repos.iter().find(|e| e.matches(&path)).ok_or_else(|| {
                format!(
                    "{} has no recipe for {repository}: offer it one, and accept it there \
                     (Settings, Repositories)",
                    builder.cfg.node_name
                )
            })?;
            if let Some(hold) = crate::mesh::recipe::held(&builder.store, entry) {
                return Err(crate::mesh::recipe::held_reason(&hold));
            }
            Some(path)
        }
    };
    let known = templates.shared().get(&key);
    templates.shared().ask(
        templates.team_stem(),
        &key,
        &repository,
        crate::store::now_ms(),
    );
    tracing::info!(from = %sender, %repository, %key, "a peer asked for a DigitalOcean template");
    match known {
        // Made: looked at again, and rebuilt if it has gone.
        Some(known) if known.failed.is_none() && !known.image.is_empty() => {
            runtime.prepare(known.image)
        }
        Some(known) if known.failed.is_none() && runtime.is_preparing_key(&key) => {}
        // New, failed (tried again), or left half made by an earlier process.
        _ => {
            templates.shared().retry(&key);
            runtime.start_peer_job(builder, repo, repository, commit, key.clone());
        }
    }
    let advert = templates
        .shared()
        .adverts(|name| templates.state_named(name))
        .into_iter()
        .find(|a| a.key == key);
    Ok(serde_json::json!(advert))
}

/// Make `key`'s template: prepare the repository here (or take the pinned
/// base), check the key this node computes for it is `key`, then build,
/// push and register it. A failure is recorded for the hello.
pub(super) async fn peer_job(
    runtime: Arc<super::runtime::DoRuntime>,
    builder: Builder,
    repo: Option<PathBuf>,
    repository: String,
    commit: String,
    key: String,
) {
    let templates = runtime.provider().templates().clone();
    let made = async {
        let image = match &repo {
            None => {
                let image = crate::sandbox_images::active_base()
                    .ok_or("this node has not pinned its sandbox base yet")?;
                let mine = portable::base_inputs(&builder.cfg).await?.key();
                diverged(&mine, &key, "the stock base", &commit, "")?;
                complete(&builder.cfg)?;
                image
            }
            Some(path) => {
                let clone = crate::forge::ensure_clone_for_build(
                    &builder.broker,
                    &builder.node_id,
                    &repository,
                )
                .await
                .map_err(|e| format!("clone {repository}: {e}"))?;
                if let Err(error) =
                    crate::forge::refresh_for_build(&builder.broker, &clone, &builder.node_id).await
                {
                    tracing::warn!(%repository, %error, "could not fetch before preparing for a peer");
                }
                let (inputs, mine_commit) =
                    portable::repo_inputs(&builder.cfg, path, &repository).await?;
                diverged(&inputs.key(), &key, &repository, &commit, &mine_commit)?;
                complete(&builder.cfg)?;
                prepare_repo(&runtime, &builder, path, &repository, &key).await?
            }
        };
        Ok::<_, String>(image)
    }
    .await;
    runtime.done_preparing_key(&key);
    match made {
        Ok(image) => {
            runtime.adopt(&image, &key);
            runtime.prepare(image);
        }
        Err(reason) => {
            tracing::warn!(%repository, %key, %reason, "no DigitalOcean template made for a peer");
            templates.shared().fail(&key, &reason);
        }
    }
}

/// The repository's snapshot, prepared here as its own sessions would have
/// it: its image built, then `prepare` run and snapshotted.
async fn prepare_repo(
    runtime: &Arc<super::runtime::DoRuntime>,
    builder: &Builder,
    path: &Path,
    repository: &str,
    key: &str,
) -> Result<String, String> {
    use crate::repo_image::Ensured;
    let backend = builder.backend.as_ref();
    match crate::repo_image::ensure_base(backend, &builder.cfg, &builder.store, path, false).await {
        Ensured::Stale { error, .. } => return Err(format!("{repository}'s image: {error}")),
        Ensured::Unavailable { reason } => return Err(format!("{repository}'s image: {reason}")),
        _ => {}
    }
    let environment = crate::environment::environment_for(&builder.cfg, &builder.store, Some(path));
    if environment.prepare.is_empty() {
        return Err(format!(
            "{repository}'s entry here prepares nothing, so it has no template of its own"
        ));
    }
    // A snapshot this makes is taken for the key as it is made.
    runtime.expect_snapshot(path, key);
    let snapshot = crate::sandbox_images::snapshot_for(
        backend,
        &builder.cfg,
        &builder.store,
        path,
        &environment,
        true,
    )
    .await;
    runtime.unexpect_snapshot(path);
    snapshot
        .ok_or_else(|| format!("{repository} could not be prepared here; the node's log says why"))
}

/// Refuse a key this node does not compute for the same thing.
fn diverged(mine: &str, asked: &str, what: &str, commit: &str, at: &str) -> Result<(), String> {
    if mine == asked {
        return Ok(());
    }
    let commits = match (commit.is_empty(), at.is_empty()) {
        (false, false) if commit != at => format!(
            " (the asking node read its default branch at {}, this node at {})",
            &commit[..commit.len().min(12)],
            &at[..at.len().min(12)]
        ),
        _ => String::new(),
    };
    Err(format!(
        "this node's template key for {what} differs from the asking node's: their recipes, \
         sandbox base, release or service bundles have diverged{commits}"
    ))
}

/// Refuse while a catalogue service's bundle the key names is not here: the
/// template would hold less than the key says.
fn complete(cfg: &crate::config::Config) -> Result<(), String> {
    for service in cfg.service.iter().filter(|s| s.bundle.is_some()) {
        if !crate::service_bundle::resolve(service).is_some_and(|dir| dir.is_dir()) {
            return Err(format!(
                "service {}'s bundle is not on this node yet",
                service.name
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    const IMAGE: &str = "localhost/tracon-snapshot-app@sha256:3333333333333333444444444444444444444444444444444444444444444444";

    /// A key asked for is named under the team stem, survives a restart,
    /// is wanted while asked for within the window, and names its image's
    /// template only while what the node adds to it is unchanged.
    #[test]
    fn what_peers_asked_for_is_kept_named_and_let_go() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("shared.json");
        let shared = Shared::load(path.clone());
        let record = shared.ask("tracon-tp-", KEY, "github.com/acme/app", 1_000);
        assert_eq!(record.name, "tracon-tp-0123456789abcdef");
        assert!(shared.holds(&record.name));
        let digest = super::super::templates::digest_of(IMAGE).unwrap();
        assert_eq!(shared.name_for(digest, "own-1"), None, "no image yet");
        shared.adopt(KEY, IMAGE, "own-1");
        assert_eq!(shared.name_for(digest, "own-1"), Some(record.name.clone()));
        assert_eq!(shared.name_for(digest, "own-2"), None, "a release moved");

        let again = Shared::load(path);
        assert_eq!(again.records(), shared.records());
        let keep = PEER_KEEP.as_millis() as i64;
        assert_eq!(
            again.wanted(1_000 + keep - 1),
            std::slice::from_ref(&record.name)
        );
        assert!(again.wanted(1_000 + keep).is_empty());
        again.ask("tracon-tp-", KEY, "", 1_000 + keep);
        assert_eq!(
            again.wanted(1_000 + keep),
            std::slice::from_ref(&record.name)
        );

        again.fail(KEY, "diverged");
        let adverts = again.adverts(|_| None);
        assert_eq!(adverts[0].state, AdvertState::Failed);
        again.retry(KEY);
        let adverts = again.adverts(|_| {
            Some(TemplateState::Ready {
                template_id: "t".into(),
            })
        });
        assert_eq!(adverts[0].state, AdvertState::Ready);
        again.forget(std::slice::from_ref(&record.name));
        assert!(!again.holds(&record.name));
    }

    #[test]
    fn a_diverged_key_says_so() {
        assert!(diverged("a", "a", "x", "", "").is_ok());
        let why = diverged(
            "a",
            "b",
            "github.com/acme/app",
            "1111111111111",
            "2222222222222",
        )
        .unwrap_err();
        assert!(
            why.contains("diverged") && why.contains("111111111111"),
            "{why}"
        );
    }
}
