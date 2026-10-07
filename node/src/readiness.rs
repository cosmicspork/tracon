//! Whether a repository is ready for what a session would be started to do.
//!
//! Three paths ask for different things. An investigation needs a place to run
//! and a model: it reads, reproduces and reports, and asking it for a forge
//! credential or a product brief would refuse work that never touches either.
//! Verification adds what the node needs to vouch for a result: required
//! checks, and an image those checks can be pinned to. Publication adds what
//! the forge needs: a remote the node can name and a credential this channel
//! may use. Each gap is said before a session is spent finding it.
//!
//! This only reads. Nothing here builds an image, contacts a forge or starts a
//! session, so asking costs a git read and nothing else.

use std::path::Path;

use serde::Serialize;

use crate::forge::Forge;

pub const INVESTIGATE: &str = "investigate";
pub const VERIFY: &str = "verify";
pub const PUBLISH: &str = "publish";

/// `environment::RepoEnvironment::image_source` for a repository with no
/// image of its own.
const HARNESS_IMAGE: &str = "harness image";

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Gap {
    /// A stable name the client can key on.
    pub key: &'static str,
    pub message: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct PathReadiness {
    pub purpose: &'static str,
    pub ready: bool,
    /// What stops a session on this path from finishing it.
    pub missing: Vec<Gap>,
    /// What the path would lack without being stopped by it.
    pub notes: Vec<Gap>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Readiness {
    pub channel: String,
    pub repo: String,
    pub investigate: PathReadiness,
    pub verify: PathReadiness,
    pub publish: PathReadiness,
}

/// Everything the verdict is drawn from, read once by the caller.
#[derive(Debug, Clone, Default)]
pub struct Facts {
    pub repo_exists: bool,
    /// Why this node would refuse to start a session here: not ready, the
    /// channel archived, no usable model or harness. `None` when it would.
    pub launch_refused: Option<String>,
    pub checks: Vec<String>,
    /// The image the checks would run in, and whose it is.
    pub image: String,
    pub image_source: &'static str,
    /// The repository's `origin`, as `host/owner/name`.
    pub remote: Option<String>,
    /// Why the forge credential cannot be used from this channel; `None` when
    /// it can, or when there is no forge to ask about.
    pub credential_refused: Option<String>,
    /// Whether the work item the session would serve has a brief. `None` for
    /// a session with no item.
    pub item_has_brief: Option<bool>,
}

/// The forge a remote is on, by its host.
pub fn forge_of(remote: &str) -> Option<Forge> {
    let host = remote.split('/').next().unwrap_or_default();
    if host.contains("github") {
        Some(Forge::Github)
    } else if host.contains("gitlab") {
        Some(Forge::Gitlab)
    } else {
        None
    }
}

fn gap(key: &'static str, message: impl Into<String>) -> Gap {
    Gap {
        key,
        message: message.into(),
    }
}

fn path(purpose: &'static str, missing: Vec<Gap>, notes: Vec<Gap>) -> PathReadiness {
    PathReadiness {
        purpose,
        ready: missing.is_empty(),
        missing,
        notes,
    }
}

pub fn assess(channel: &str, repo: &str, facts: &Facts) -> Readiness {
    let mut launch = Vec::new();
    if !facts.repo_exists {
        launch.push(gap(
            "repo",
            format!("{repo} is not a directory on this node"),
        ));
    }
    if let Some(reason) = &facts.launch_refused {
        launch.push(gap("launch", reason.clone()));
    }

    let mut verifying = launch.clone();
    if facts.checks.is_empty() {
        verifying.push(gap(
            "checks",
            "no required checks are configured for this repository, so nothing the node runs \
             can verify a result; add `checks` to its [[repo]] entry",
        ));
    }
    let mut verify_notes = Vec::new();
    if facts.image_source.contains("not built") {
        verifying.push(gap(
            "image",
            "the repository's Dockerfile has not been built on this node yet, so its checks \
             have no image to run in",
        ));
    } else if facts.image_source == HARNESS_IMAGE {
        verify_notes.push(gap(
            "image",
            format!(
                "checks run in the node's harness image ({}); the repository names no \
                 toolchain of its own",
                facts.image
            ),
        ));
    }

    let mut publishing = verifying.clone();
    let forge = facts.remote.as_deref().and_then(forge_of);
    match (&facts.remote, forge) {
        (None, _) => publishing.push(gap(
            "remote",
            "the repository has no `origin` remote to publish to",
        )),
        (Some(remote), None) => publishing.push(gap(
            "remote",
            format!("{remote} is not on a forge the node publishes to (GitHub or GitLab)"),
        )),
        (Some(_), Some(forge)) => {
            if let Some(reason) = &facts.credential_refused {
                publishing.push(gap(
                    "credential",
                    format!(
                        "{} credential `{}`: {reason}",
                        forge.name(),
                        forge.credential()
                    ),
                ));
            }
        }
    }
    let mut publish_notes = Vec::new();
    if facts.item_has_brief == Some(false) {
        publish_notes.push(gap(
            "brief",
            "the work item has no brief, so its review shows no requirements to judge against",
        ));
    }

    Readiness {
        channel: channel.to_string(),
        repo: repo.to_string(),
        investigate: path(INVESTIGATE, launch, Vec::new()),
        verify: path(VERIFY, verifying, verify_notes),
        publish: path(PUBLISH, publishing, publish_notes),
    }
}

/// Read the facts for `repo` on `channel` and judge them.
pub async fn readiness(
    manager: &crate::session::Manager,
    tools: &crate::mcp::Tools,
    node_id: &str,
    channel: &str,
    repo: &str,
    work_item: Option<&str>,
) -> Readiness {
    let cfg = manager.cfg();
    let store = manager.store();
    let repo_path = Path::new(repo);
    let repo_exists = repo_path.is_dir();

    let spec: Option<crate::session::NewSession> = serde_json::from_value(serde_json::json!({
        "channel": channel,
        "repo_path": repo,
        "work_item_id": work_item,
    }))
    .ok();
    let mut launch_refused = spec
        .and_then(|spec| manager.preflight(&spec).err())
        .map(|e| e.to_string());
    if launch_refused.is_none() {
        launch_refused = match store.get_node(node_id) {
            Ok(Some(node)) if node.state == "ready" => None,
            Ok(Some(node)) => Some(format!(
                "this node is {}{}",
                node.state,
                node.failed_detail
                    .or(node.failed_check)
                    .map(|why| format!(": {why}"))
                    .unwrap_or_default()
            )),
            _ => Some("this node has not checked its own readiness yet".into()),
        };
    }

    let environment = crate::environment::environment_for(cfg, store, Some(repo_path));
    let remote = if repo_exists {
        crate::corpus::project::identify(channel, repo_path, &cfg.publish.git)
            .await
            .2
    } else {
        None
    };
    let credential_refused = remote.as_deref().and_then(forge_of).and_then(|forge| {
        tools
            .broker
            .read()
            .unwrap()
            .env_for(forge.credential(), channel, node_id)
            .err()
            .map(|e| e.to_string())
    });
    let item_has_brief = work_item.map(|id| {
        store
            .work_get(id)
            .ok()
            .flatten()
            .is_some_and(|item| item.brief_slug.is_some())
    });

    assess(
        channel,
        repo,
        &Facts {
            repo_exists,
            launch_refused,
            checks: environment.checks,
            image: environment.image,
            image_source: environment.image_source,
            remote,
            credential_refused,
            item_has_brief,
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const DIGEST: &str =
        "img@sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    fn ready() -> Facts {
        Facts {
            repo_exists: true,
            launch_refused: None,
            checks: vec!["just check".into()],
            image: DIGEST.into(),
            image_source: "repository toolchain",
            remote: Some("github.com/o/r".into()),
            credential_refused: None,
            item_has_brief: Some(true),
        }
    }

    fn keys(p: &PathReadiness) -> Vec<&'static str> {
        p.missing.iter().map(|g| g.key).collect()
    }

    #[test]
    fn everything_in_place_is_ready_for_all_three() {
        let r = assess("personal", "/r", &ready());
        assert!(r.investigate.ready && r.verify.ready && r.publish.ready);
        assert!(r.publish.notes.is_empty());
    }

    #[test]
    fn an_investigation_needs_no_checks_forge_or_brief() {
        let facts = Facts {
            checks: Vec::new(),
            remote: None,
            credential_refused: Some("credential gh is not bound to channel personal".into()),
            item_has_brief: Some(false),
            ..ready()
        };
        let r = assess("personal", "/r", &facts);
        assert!(r.investigate.ready, "{:?}", r.investigate);
        assert_eq!(keys(&r.verify), vec!["checks"]);
        assert_eq!(keys(&r.publish), vec!["checks", "remote"]);
        assert_eq!(r.publish.notes[0].key, "brief");
    }

    #[test]
    fn publication_needs_a_forge_and_a_bound_credential() {
        let r = assess(
            "personal",
            "/r",
            &Facts {
                credential_refused: Some("credential gh is not bound to channel personal".into()),
                ..ready()
            },
        );
        assert!(r.verify.ready);
        assert_eq!(keys(&r.publish), vec!["credential"]);
        assert!(r.publish.missing[0].message.contains("not bound"));

        let r = assess(
            "personal",
            "/r",
            &Facts {
                remote: Some("git.example.com/o/r".into()),
                ..ready()
            },
        );
        assert_eq!(keys(&r.publish), vec!["remote"]);
    }

    #[test]
    fn a_launch_refusal_blocks_every_path() {
        let r = assess(
            "personal",
            "/r",
            &Facts {
                repo_exists: false,
                launch_refused: Some("this node is failed: podman".into()),
                ..ready()
            },
        );
        for p in [&r.investigate, &r.verify, &r.publish] {
            assert_eq!(keys(p)[..2], ["repo", "launch"]);
        }
    }

    #[test]
    fn an_unbuilt_dockerfile_is_said_as_such() {
        let r = assess(
            "personal",
            "/r",
            &Facts {
                image: "harness@sha256:x".into(),
                image_source: "harness image (repository image not built)",
                ..ready()
            },
        );
        assert!(r.verify.missing[0].message.contains("not been built"));

        let r = assess(
            "personal",
            "/r",
            &Facts {
                image: "localhost/harness".into(),
                image_source: HARNESS_IMAGE,
                ..ready()
            },
        );
        assert!(
            r.verify.ready,
            "the harness image is resolved when a check runs"
        );
        assert_eq!(r.verify.notes[0].key, "image");
        assert_eq!(forge_of("gitlab.example.com/o/r"), Some(Forge::Gitlab));
    }
}
