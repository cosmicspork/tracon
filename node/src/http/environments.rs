//! The `[[repo]]` table as the node holds it, and the images it built for it.

use std::path::{Path, PathBuf};

use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};

use super::api::{ApiError, ApiResult, AppState};
use super::auth::Loopback;
use crate::config::Config;
use crate::repo_image;

/// Every entry, with the newest build of each repository it answers for. A
/// relative `path` can match several clones, so an entry may carry more than
/// one build.
pub async fn list(State(s): State<AppState>) -> ApiResult<Json<Value>> {
    let builds = s.store().repo_images()?;
    let entries: Vec<Value> = s
        .cfg
        .repo
        .iter()
        .enumerate()
        .map(|(index, entry)| {
            // The first matching entry wins, here as everywhere else.
            let builds: Vec<_> = builds
                .iter()
                .filter(|build| {
                    s.cfg
                        .repo
                        .iter()
                        .position(|entry| entry.matches(Path::new(&build.repo_path)))
                        == Some(index)
                })
                .collect();
            json!({ "entry": entry, "builds": builds })
        })
        .collect();
    Ok(Json(json!({
        "can_build": s.manager.backend().image_builder().is_some(),
        "entries": entries,
    })))
}

#[derive(Deserialize)]
pub struct BuildBody {
    repo: String,
}

/// Build a repository's image now, whether or not its recipe changed. The
/// recipe is resolved before answering, so a Dockerfile the default branch
/// does not hold is refused here rather than discovered by polling; the build
/// itself runs on after the response and is read back from the list.
pub async fn build(
    _: Loopback,
    State(s): State<AppState>,
    Json(body): Json<BuildBody>,
) -> ApiResult<Json<Value>> {
    let unprocessable = |message: String| ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, message);
    let repo = repo_root(&body.repo).map_err(unprocessable)?;
    let entry = repo_image::building_entry(&s.cfg, &repo).ok_or_else(|| {
        unprocessable(format!(
            "no [[repo]] entry with a dockerfile matches {}",
            repo.display()
        ))
    })?;
    if s.manager.backend().image_builder().is_none() {
        return Err(unprocessable(format!(
            "the {} runtime cannot build images",
            s.manager.backend().kind()
        )));
    }
    let recipe = repo_image::recipe(&repo, entry)
        .await
        .map_err(unprocessable)?;
    let since_ms = crate::store::now_ms();
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
    })))
}

/// The repository a caller named: a checkout by its path, or a clone the node
/// manages by the end of its path (`owner/name`).
fn repo_root(given: &str) -> Result<PathBuf, String> {
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
        (None, _) => Err(format!("no managed clone ends in {given}")),
        (Some(_), Some(_)) => Err(format!(
            "more than one managed clone ends in {given}; name its host too"
        )),
    }
}
