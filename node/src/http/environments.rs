//! The `[[repo]]` table as the node holds it, and the images it built for it.

use std::path::{Path, PathBuf};

use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};

use super::api::{ApiError, ApiResult, AppState};
use super::passkey::AdminRequest;
use crate::config::{Config, Repo};
use crate::repo_image;

/// Every entry, with the newest build of each repository it answers for. A
/// relative `path` can match several clones, so an entry may carry more than
/// one build. `hash` is `node.toml`'s version, which a save names.
pub async fn list(State(s): State<AppState>) -> ApiResult<Json<Value>> {
    let hash = crate::config::edit::file_hash(&Config::config_path())?;
    let builds = s.store().repo_images()?;
    let repos = s.cfg.repos();
    let entries: Vec<Value> = repos
        .iter()
        .enumerate()
        .map(|(index, entry)| {
            // The first matching entry wins, here as everywhere else.
            let builds: Vec<_> = builds
                .iter()
                .filter(|build| {
                    repos
                        .iter()
                        .position(|entry| entry.matches(Path::new(&build.repo_path)))
                        == Some(index)
                })
                .collect();
            // An entry that keeps lending credentials to a preparation a
            // peer's recipe changed builds nothing until it is started.
            let hold = crate::mesh::recipe::held(s.store(), entry);
            json!({ "entry": entry, "builds": builds, "hold": hold })
        })
        .collect();
    Ok(Json(json!({
        "can_build": s.manager.backend().image_builder().is_some(),
        "presets": crate::config::egress_presets(),
        "entries": entries,
        "hash": hash,
    })))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SaveBody {
    entries: Vec<crate::config::Repo>,
    /// `node.toml`'s `hash` as the list gave it. Required: the table is
    /// saved whole, so a save decided against an older file would undo
    /// whatever changed since.
    #[serde(default)]
    expected_hash: Option<String>,
}

/// Replace the table: refused whole if any entry is one the node could not
/// honour, written to `node.toml`, and in effect for the next session and
/// the next check without a restart. A session already running keeps the
/// image and the egress it started with. An admin action: off this machine
/// a passkey confirms it, and the prompt lists the keys it changes.
pub async fn save(
    State(s): State<AppState>,
    req: AdminRequest,
) -> Result<Json<Value>, super::config_api::Refused> {
    let raw: Value = req.json()?;
    super::config_api::refuse_hidden(&raw)?;
    let body: SaveBody = serde_json::from_value(raw).map_err(|e| {
        ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            format!("the request body does not read: {e}"),
        )
    })?;
    let expected = super::config_api::expected_version(body.expected_hash.as_deref(), &req)?;
    crate::config::validate_repos(&body.entries)
        .map_err(|error| ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, error))?;
    let (text, now) = super::config_api::read_expected(&expected)?;
    let ops = crate::config::Op::replace_repos(&now.repo, &body.entries)
        .map_err(super::config_api::edit_refusal)?;
    let (changes, summary) = super::config_api::prompt(&text, &ops)?;
    if changes.is_empty() {
        return Ok(Json(json!({
            "changed": false,
            "restart_required": false,
            "hash": expected,
        })));
    }
    req.confirm(&s, &summary).await?;
    let outcome = crate::config::edit::replace_repos_expecting(
        &s.cfg,
        s.store(),
        &expected,
        &body.entries,
        "Settings (repositories)",
    )
    .map_err(super::config_api::edit_refusal)?;
    if outcome.wrote() {
        warm_newly_lent(&s, &now.repo, &body.entries);
    }
    let (restart_required, pending) = super::config_api::restart_owed(&s, &outcome).await;
    Ok(Json(json!({
        "changed": outcome.wrote(),
        "restart_required": restart_required,
        "pending_restart": pending,
        "hash": outcome.new_hash,
    })))
}

/// The entries of `after` that lend their preparation credentials `before`
/// did not lend them: newly lent, or lent a different set. An entry that
/// lends none, or the same ones in another order, is not among them, nor is
/// one held for a peer's recipe, which lends nothing until its build is
/// started.
fn newly_lent<'a>(
    store: &crate::store::Store,
    before: &[Repo],
    after: &'a [Repo],
) -> Vec<&'a Repo> {
    let lent = |entry: &Repo| {
        let mut names = entry.prepare_credentials.clone();
        names.sort();
        names.dedup();
        names
    };
    after
        .iter()
        .filter(|entry| !entry.prepare_credentials.is_empty())
        .filter(|entry| {
            before
                .iter()
                .find(|was| was.path == entry.path)
                .is_none_or(|was| lent(was) != lent(entry))
        })
        .filter(|entry| crate::mesh::recipe::held(store, entry).is_none())
        .collect()
}

/// Warm the base cache of each repository whose entry now lends its
/// preparation a credential it did not, without a rebuild: lending one
/// changes no recipe, so nothing else would warm with it, and the next
/// check would install from a cache filled without it.
fn warm_newly_lent(s: &AppState, before: &[Repo], after: &[Repo]) {
    for entry in newly_lent(s.store(), before, after) {
        let Ok(repo) = repo_root(&entry.path.to_string_lossy()) else {
            tracing::debug!(path = %entry.path.display(), "newly lent entry names no one repository; not warmed");
            continue;
        };
        repo_image::forget_warmed(&repo);
        let (backend, cfg, store) = (
            s.manager.backend().clone(),
            s.cfg.clone(),
            s.store().clone(),
        );
        tokio::spawn(async move {
            repo_image::warm_base_cache(backend.as_ref(), &cfg, &store, &repo).await;
        });
    }
}

#[derive(Deserialize)]
pub struct BuildBody {
    repo: String,
}

/// Build a repository's image now, whether or not its recipe changed. The
/// recipe is resolved before answering, so a Dockerfile the default branch
/// does not hold is refused here rather than discovered by polling; the build
/// itself runs on after the response and is read back from the list. An
/// admin action: a build runs the repository's Dockerfile with the node's
/// builder and registry credentials.
pub async fn build(
    State(s): State<AppState>,
    req: AdminRequest,
) -> Result<Json<Value>, super::passkey::Refusal> {
    let body: BuildBody = req.json()?;
    if let Some(c) = super::config_api::hidden_char(&body.repo, &[]) {
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            format!(
                "repo holds U+{:04X}, a character a confirmation could not show",
                c as u32
            ),
        )
        .into());
    }
    // Confirmed before the name is resolved: resolving it looks at the
    // host's directories, which is not for a caller who has not confirmed.
    req.confirm(
        &s,
        &format!(
            "Build the image for the repository {}",
            super::config_prompt::quote(body.repo.trim())
        ),
    )
    .await?;
    let unprocessable = |message: String| ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, message);
    // A repository named by its forge identity that is not on this node yet
    // is cloned, once its entry is known to build: the usual order is `repo
    // set` and then `repo build`, before any session has made the clone.
    let state_dir = Config::state_dir();
    let (repo, uncloned) = match repo_root(&body.repo) {
        Ok(repo) => (repo, None),
        Err(error) => match uncloned(&crate::forge::managed_root(&state_dir), &body.repo) {
            Some((identity, dest)) => (dest, Some(identity)),
            None => return Err(unprocessable(error).into()),
        },
    };
    let entry = repo_image::building_entry(&s.cfg, &repo).ok_or_else(|| {
        unprocessable(format!(
            "no [[repo]] entry with a dockerfile matches {}",
            repo.display()
        ))
    })?;
    if let Some(hold) = crate::mesh::recipe::held(s.store(), &entry) {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            format!(
                "{}; start it with the held entry's Start the build, which says what it lends",
                crate::mesh::recipe::held_reason(&hold)
            ),
        )
        .into());
    }
    if s.manager.backend().image_builder().is_none() {
        return Err(unprocessable(format!(
            "the {} runtime cannot build images",
            s.manager.backend().kind()
        ))
        .into());
    }
    if let Some(identity) = &uncloned {
        crate::forge::ensure_clone_for_build(&s.tools.broker, &s.node_id, identity)
            .await
            .map_err(|error| {
                ApiError::new(
                    super::api::clone_status(&error),
                    format!("{identity} could not be cloned, so nothing was built: {error}"),
                )
            })?;
    } else {
        // A managed clone is fetched first: its default branch is otherwise
        // whatever the last session fetched, and a Dockerfile change that
        // just merged would not be built. Refused rather than built from a
        // branch that may be stale, which would look like a build of the
        // new one.
        crate::forge::refresh_for_build(&s.tools.broker, &repo, &s.node_id)
            .await
            .map_err(|error| {
                ApiError::new(
                    StatusCode::BAD_GATEWAY,
                    format!(
                        "the managed clone could not be fetched, so nothing was built: {error}"
                    ),
                )
            })?;
    }
    let recipe = repo_image::recipe(&repo, &entry, &repo_image::RunAs::of(&s.cfg))
        .await
        .map_err(unprocessable)?;
    // A build of this recipe already running is the one this request gets
    // (`ensure_base` joins it), so the caller follows that build's record.
    let running = repo_image::in_flight(s.store(), &repo, &recipe.hash);
    let since_ms = running.unwrap_or_else(crate::store::now_ms);
    let (backend, cfg, store) = (
        s.manager.backend().clone(),
        s.cfg.clone(),
        s.store().clone(),
    );
    let building = repo.clone();
    tokio::spawn(async move {
        repo_image::ensure_base(backend.as_ref(), &cfg, &store, &building, true).await;
    });
    Ok(Json(json!({
        "repo": repo,
        "since_ms": since_ms,
        "source_ref": recipe.source_ref,
        "source_commit": recipe.source_commit,
        "joined": running.is_some(),
        "cloned": uncloned.is_some(),
    })))
}

/// The repository a caller named: a checkout by its path, or a clone the node
/// manages by the end of its path (`owner/name`).
pub(super) fn repo_root(given: &str) -> Result<PathBuf, String> {
    let path = crate::config::expand_home(Path::new(given.trim()));
    if path.as_os_str().is_empty() {
        return Err("name the repository to build".into());
    }
    if path.is_absolute() {
        return match path.is_dir() {
            true => Ok(path),
            false => Err(format!("{} is not a directory", path.display())),
        };
    }
    let root = crate::forge::managed_root(&Config::state_dir());
    let want: Vec<_> = path.components().collect();
    let mut matches = crate::forge::managed_repos(&Config::state_dir())
        .into_iter()
        .map(|repo| root.join(&repo.host).join(&repo.owner).join(&repo.name))
        .filter(|clone| {
            let have: Vec<_> = clone.components().collect();
            have.len() >= want.len() && have[have.len() - want.len()..] == want[..]
        });
    match (matches.next(), matches.next()) {
        (Some(only), None) => Ok(only),
        (None, _) => Err(format!(
            "no managed clone ends in {given}; name the repository by its forge identity \
             (host/owner/name) to clone it, or clone it from the repository picker"
        )),
        (Some(_), Some(_)) => Err(format!(
            "more than one managed clone ends in {given}; name its host too"
        )),
    }
}

/// A repository named by its forge identity, `host/owner/name`, with no
/// managed clone under `root`: the identity and where its clone goes. A bare
/// suffix names no forge to clone from, and is not one.
fn uncloned(root: &Path, given: &str) -> Option<(String, PathBuf)> {
    let identity = given.trim();
    let (host, owner, name) = crate::forge::identity_parts(identity).ok()?;
    let dest = crate::forge::clone_dest(root, host, owner, name).ok()?;
    (!dest.join(".git").exists()).then(|| (identity.to_string(), dest))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lending(path: &str, credentials: &[&str]) -> Repo {
        Repo {
            path: path.into(),
            prepare_credentials: credentials.iter().map(|name| name.to_string()).collect(),
            ..Default::default()
        }
    }

    /// Only lending a credential anew warms: adding one, or changing which,
    /// to an entry old or new. Saving the same ones, in any order, taking
    /// them all away, or lending them to a held entry does not.
    #[test]
    fn only_an_entry_newly_lent_a_credential_is_warmed_again() {
        let before = [
            lending("/src/same", &["nova", "npm"]),
            lending("/src/added", &[]),
            lending("/src/changed", &["nova"]),
            lending("/src/removed", &["nova"]),
        ];
        let after = [
            lending("/src/same", &["npm", "nova"]),
            lending("/src/added", &["nova"]),
            lending("/src/changed", &["npm"]),
            lending("/src/removed", &[]),
            lending("/src/new", &["nova"]),
            lending("/src/new-bare", &[]),
            lending("/src/held", &["nova"]),
        ];
        let store = crate::store::Store::open_in_memory().unwrap();
        store
            .credential_hold_put(&crate::store::recipe_offers::CredentialHoldRow {
                path: "/src/held".into(),
                offer_id: "o1".into(),
                sender_node: "peer".into(),
                repo: "github.com/o/r".into(),
                credentials: vec!["nova".into()],
                lending_hash: crate::mesh::recipe::lending_hash(&after[6]),
                created_ms: 0,
            })
            .unwrap();
        let warmed: Vec<_> = newly_lent(&store, &before, &after)
            .into_iter()
            .map(|entry| entry.path.to_string_lossy().into_owned())
            .collect();
        assert_eq!(warmed, ["/src/added", "/src/changed", "/src/new"]);
    }

    #[test]
    fn only_a_forge_identity_without_a_clone_is_cloned_for_a_build() {
        let root = tempfile::tempdir().unwrap();
        let root = root.path();
        let (identity, dest) = uncloned(root, " gitlab.com/group/sub/proj ").unwrap();
        assert_eq!(identity, "gitlab.com/group/sub/proj");
        assert_eq!(dest, root.join("gitlab.com/group/sub/proj"));
        assert!(uncloned(root, "group/proj").is_none());
        assert!(uncloned(root, "gitlab.com/proj").is_none());
        assert!(uncloned(root, "gitlab.com/../proj").is_none());
        std::fs::create_dir_all(dest.join(".git")).unwrap();
        assert!(uncloned(root, "gitlab.com/group/sub/proj").is_none());
    }
}
