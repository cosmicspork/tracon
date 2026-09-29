//! Publishing crosses two explicit boundaries. A candidate is read without a
//! credential from its runtime snapshot, then imported into a fresh
//! publisher-controlled bare repository. Only that publisher repository sees
//! broker-provided forge authentication.
//!
//! It also crosses a boundary in time. A push and an opened merge request are
//! side effects this node cannot take back, and a node can die between them.
//! So every attempt carries a record (`crate::store::publication`) written
//! before each side effect, and a resumed attempt *looks before it acts*: it
//! asks the forge what the branch holds and whether the change is already
//! open, and resumes from what it observes. Pushing twice, opening a second
//! merge request, and reporting a failure that already succeeded are all the
//! same bug — believing a local record over the forge.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::process::Command;

use crate::git_remote::{self, Credential};
use crate::{broker::SharedBroker, config::Config};

/// The home `$HOME` points at for publication's Git commands: node-owned,
/// empty, and never the operator's.
const PUBLISH_HOME: &str = "publish-home";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Provider {
    Github,
    Gitlab,
}

impl Provider {
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "github" | "gh" | "pr" => Some(Self::Github),
            "gitlab" | "glab" | "mr" => Some(Self::Gitlab),
            _ => None,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Github => "github",
            Self::Gitlab => "gitlab",
        }
    }

    pub fn credential(&self) -> &'static str {
        match self {
            Self::Github => "gh",
            Self::Gitlab => "glab",
        }
    }

    pub fn command(&self, cfg: &Config) -> String {
        match self {
            Self::Github => cfg.publish.gh.clone(),
            Self::Gitlab => cfg.publish.glab.clone(),
        }
    }

    pub fn noun(&self) -> &'static str {
        match self {
            Self::Github => "pull request",
            Self::Gitlab => "merge request",
        }
    }

    /// The same forge, as the side of the node that already knows how to
    /// authenticate Git against it names it.
    fn forge(&self) -> crate::forge::Forge {
        match self {
            Self::Github => crate::forge::Forge::Github,
            Self::Gitlab => crate::forge::Forge::Gitlab,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Target {
    pub provider: String,
    pub project: String,
    pub base: String,
    pub branch: String,
    /// External worktree provenance only. Publication still imports its
    /// immutable candidate into a distinct publisher repository.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree: Option<String>,
    /// The change on the forge this review updates. `None` opens a new one.
    /// Fixed at first submit: a resubmission cannot retarget a review.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub change: Option<ChangeRef>,
}

/// An existing pull request (by number) or merge request (by iid).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChangeRef {
    pub number: u64,
    #[serde(default)]
    pub url: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Description {
    pub title: String,
    pub body: String,
}

/// What a revision asks the forge to show besides its commits. The review's
/// own title and body are the operator's summary; they reach the forge only
/// as a new change's description when no other description is given.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Outputs {
    /// A new change opens with it; an existing change's title and
    /// description are replaced by it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<Description>,
    /// Posted on the change once its commits are on the forge.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub comment: Option<String>,
    /// Open the new change as a draft. Meaningless for an existing one.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub draft: bool,
}

impl Outputs {
    /// The outputs a publication acts on: a new change always has a
    /// description, and without one it is the approved title and body — which
    /// is all a review published before outputs existed ever asked for.
    pub fn resolve(mut self, target: &Target, title: &str, body: &str) -> Self {
        if target.change.is_none() && self.description.is_none() {
            self.description = Some(Description {
                title: title.to_string(),
                body: body.to_string(),
            });
        }
        if target.change.is_some() {
            self.draft = false;
        }
        self
    }
}

/// One revision's publication intent, pinned when it is submitted.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Intent {
    #[serde(default)]
    pub forge: Outputs,
    /// What the existing change's branch held when this revision was
    /// submitted. A push may replace exactly this and nothing else.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lease: Option<String>,
    /// The branch's history was rewritten, so the push is not a fast-forward
    /// of `lease` and is forced — but only over `lease`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub rewrite: bool,
}

/// What the forge says about an existing change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangeState {
    pub url: String,
    pub open: bool,
    pub source_branch: String,
    pub target_branch: String,
    /// The change's source branch lives in another repository (a fork).
    pub cross_repository: bool,
    pub head_sha: String,
}

impl ChangeState {
    /// Why this change cannot be updated by publishing `target`, if it can't.
    pub fn refusal(&self, provider: Provider, target: &Target, number: u64) -> Option<String> {
        let noun = provider.noun();
        if !self.open {
            Some(format!("{noun} {number} is not open"))
        } else if self.cross_repository {
            Some(format!(
                "{noun} {number} comes from another repository; only a branch of {} can be updated",
                target.project
            ))
        } else if self.source_branch != target.branch {
            Some(format!(
                "{noun} {number} is for branch {}, not {}",
                self.source_branch, target.branch
            ))
        } else if self.target_branch != target.base {
            Some(format!(
                "{noun} {number} merges into {}, not {}",
                self.target_branch, target.base
            ))
        } else {
            None
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum PublishError {
    #[error("{0}")]
    Broker(String),
    #[error("{provider} is not a provider this node publishes to")]
    UnknownProvider { provider: String },
    #[error("could not run {cli}: {source}")]
    Spawn { cli: String, source: std::io::Error },
    #[error("{cli} refused: {stderr}")]
    Refused { cli: String, stderr: String },
    #[error("the branch moved after approval (reviewed {reviewed:.8}, now {now:.8}); re-review before publishing")]
    BranchMoved { reviewed: String, now: String },
    #[error("candidate identity changed while transferring to the publisher")]
    IdentityChanged,
    #[error("the reviewed tree ({reviewed:.8}) is not what {head:.8} holds now ({found:.8}); re-review before publishing")]
    TreeChanged {
        head: String,
        reviewed: String,
        found: String,
    },
    #[error(
        "no reviewed tree is recorded for this candidate, so what would be pushed cannot be \
         checked against what was reviewed; resubmit it for review before publishing"
    )]
    NoReviewedTree,
    #[error(
        "{branch} on the forge holds {found} after the push, not the reviewed commit {expected:.8}"
    )]
    RefMismatch {
        branch: String,
        expected: String,
        found: String,
    },
    #[error(
        "{branch} on the forge holds {found:.8}, not {lease:.8} that this revision was submitted \
         against; somebody else pushed to it, so resubmit on top of what it holds now"
    )]
    LeaseLost {
        branch: String,
        lease: String,
        found: String,
    },
    #[error("{0}")]
    Unknown(String),
    #[error("invalid publication target: {0}")]
    Target(String),
}

impl PublishError {
    /// Whether the outcome is genuinely unknown rather than settled. The
    /// caller must not turn this into either a success or a failure, and must
    /// not undo the publish claim: something may exist on the forge.
    pub fn is_unknown(&self) -> bool {
        matches!(self, Self::Unknown(_))
    }
}

/// The node's record of one publication, told what is about to happen before
/// it happens. Implemented over the store; `publish` never writes the database
/// itself, so the boundary this module is about stays where it is.
pub trait Journal: Send + Sync {
    /// The branch is on the forge and the forge was observed to hold it.
    fn pushed(&self, sha: &str) -> Result<(), String>;
    /// About to ask the forge to open the change. Recorded first: a crash
    /// inside the call must leave "a change may be open", not "nothing ran".
    fn opening(&self) -> Result<(), String>;
    fn opened(&self, url: &str) -> Result<(), String>;
    /// Best effort, and deliberately infallible: this is what is written when
    /// something has already gone wrong.
    fn uncertain(&self, note: &str);
    fn failed(&self, note: &str);
}

/// For the unit tests below and any caller with nothing to record.
pub struct NoJournal;

impl Journal for NoJournal {
    fn pushed(&self, _sha: &str) -> Result<(), String> {
        Ok(())
    }
    fn opening(&self) -> Result<(), String> {
        Ok(())
    }
    fn opened(&self, _url: &str) -> Result<(), String> {
        Ok(())
    }
    fn uncertain(&self, _note: &str) {}
    fn failed(&self, _note: &str) {}
}

/// One publication of one immutable candidate.
pub struct Publication<'a> {
    pub channel: &'a str,
    pub node_id: &'a str,
    /// The publication record's id, which is also the publisher directory's
    /// name and the marker written into the opened change. It has to be
    /// derived from the review, revision, target and commit so that a retry
    /// looks for what the interrupted attempt would have left.
    pub id: &'a str,
    /// A node-owned snapshot made immediately before approval, never the
    /// mutable agent workspace.
    pub candidate: &'a str,
    pub target: &'a Target,
    pub head_sha: &'a str,
    /// The tree hash the node recorded when it captured the candidate for
    /// review. It is the one value here that does not come from the directory
    /// being published, so it is what turns "this commit still resolves to a
    /// tree" into "this commit still holds the bytes that were reviewed".
    /// Required: a candidate with no recorded tree cannot be published.
    pub reviewed_tree: &'a str,
    /// Resolved (`Outputs::resolve`): a new change always has a description.
    pub outputs: &'a Outputs,
    /// For an existing change: what its branch held at submit, and whether
    /// the push replaces that history rather than fast-forwarding it.
    pub lease: Option<&'a str>,
    pub rewrite: bool,
    /// A previous attempt for this same publication reached, or may have
    /// reached, the forge. Its side effects are observed rather than repeated.
    pub resume: bool,
    /// A previous attempt recorded a push the forge confirmed.
    pub pushed: bool,
    pub before_push: Option<&'a (dyn Fn() -> Result<(), String> + Send + Sync)>,
    pub journal: &'a dyn Journal,
}

/// Publish an immutable candidate. Git reads it with an empty credential
/// environment, writes a bundle, verifies commit and tree identity after
/// importing it, then pushes only from a new publisher repository with
/// broker authentication — and confirms afterwards that the forge holds what
/// was pushed.
pub async fn publish(
    broker: &SharedBroker,
    cfg: &Config,
    publication: &Publication<'_>,
) -> Result<String, PublishError> {
    match attempt(broker, cfg, publication).await {
        Ok(url) => Ok(url),
        Err(error) => {
            if error.is_unknown() {
                publication.journal.uncertain(&error.to_string());
            } else {
                publication.journal.failed(&error.to_string());
            }
            Err(error)
        }
    }
}

async fn attempt(
    broker: &SharedBroker,
    cfg: &Config,
    p: &Publication<'_>,
) -> Result<String, PublishError> {
    let provider = Provider::parse(&p.target.provider).ok_or(PublishError::UnknownProvider {
        provider: p.target.provider.clone(),
    })?;
    let env = broker
        .read()
        .unwrap()
        .env_for(provider.credential(), p.channel, p.node_id)
        .map_err(|e| PublishError::Broker(e.to_string()))?;
    if p.reviewed_tree.is_empty() {
        return Err(PublishError::NoReviewedTree);
    }

    let candidate_path = Path::new(p.candidate);
    let now = source_git(&cfg.publish.git, candidate_path, ["rev-parse", "HEAD"]).await?;
    if now != p.head_sha {
        return Err(PublishError::BranchMoved {
            reviewed: p.head_sha.to_string(),
            now,
        });
    }
    let source_type = source_git(
        &cfg.publish.git,
        candidate_path,
        ["cat-file", "-t", p.head_sha],
    )
    .await?;
    if source_type != "commit" {
        return Err(PublishError::IdentityChanged);
    }
    let source_tree = source_git(
        &cfg.publish.git,
        candidate_path,
        ["rev-parse", &format!("{}^{{tree}}", p.head_sha)],
    )
    .await?;
    // Every other identity here is read out of the directory being published,
    // so on its own it only proves that directory is self-consistent. This is
    // the reviewed tree, recorded elsewhere at capture time, read back with
    // replacement objects and grafts off.
    if p.reviewed_tree != source_tree {
        return Err(PublishError::TreeChanged {
            head: p.head_sha.to_string(),
            reviewed: p.reviewed_tree.to_string(),
            found: source_tree,
        });
    }
    if let Some(recheck) = p.before_push {
        recheck().map_err(PublishError::Broker)?;
    }

    let publisher = publisher_dir(p.id)?;
    let bundle = publisher.with_extension("bundle");
    let _ = std::fs::remove_dir_all(&publisher);
    let _ = std::fs::remove_file(&bundle);
    if let Some(parent) = publisher.parent() {
        std::fs::create_dir_all(parent).map_err(|source| PublishError::Spawn {
            cli: "mkdir".into(),
            source,
        })?;
    }
    init_publisher(&cfg.publish.git, &publisher).await?;
    // `git bundle create` needs the positive revision to resolve to a named
    // ref: a bare commit SHA has no ref to advertise in the bundle header, so
    // git refuses it as "empty" even though the objects are all there. Point
    // a throwaway ref at it for the bundle, then remove the ref again; the
    // candidate is a private snapshot, so nothing else observes it.
    let bundle_ref = "refs/tracon/publish-candidate";
    source_git(
        &cfg.publish.git,
        candidate_path,
        ["update-ref", bundle_ref, p.head_sha],
    )
    .await?;
    let bundled = source_git(
        &cfg.publish.git,
        candidate_path,
        [
            "bundle",
            "create",
            bundle.to_string_lossy().as_ref(),
            bundle_ref,
        ],
    )
    .await;
    let _ = source_git(
        &cfg.publish.git,
        candidate_path,
        ["update-ref", "-d", bundle_ref],
    )
    .await;
    bundled?;
    publisher_git(
        &cfg.publish.git,
        &publisher,
        [
            "fetch",
            "--no-tags",
            bundle.to_string_lossy().as_ref(),
            &format!("{}:refs/heads/candidate", p.head_sha),
        ],
    )
    .await?;
    let imported = publisher_git(
        &cfg.publish.git,
        &publisher,
        ["rev-parse", "refs/heads/candidate"],
    )
    .await?;
    let imported_tree = publisher_git(
        &cfg.publish.git,
        &publisher,
        ["rev-parse", "refs/heads/candidate^{tree}"],
    )
    .await?;
    // What is pushed below is `head_sha` out of this publisher repository, so
    // these three comparisons are what make the pushed commit the reviewed
    // one: the imported commit id, its tree, and the tree the review recorded.
    if imported != p.head_sha || imported_tree != source_tree || p.reviewed_tree != imported_tree {
        return Err(PublishError::IdentityChanged);
    }

    let remote = remote_url(provider, &env, &p.target.project)?;
    publisher_git(
        &cfg.publish.git,
        &publisher,
        ["remote", "add", "origin", &remote],
    )
    .await?;

    // From here on every Git command speaks to the forge, and it does so with
    // the credential the broker bound to this channel and node — never a
    // helper the host might offer.
    let token = provider.forge().token(&env).map(String::as_str);
    let credential = git_remote::brokered(provider.forge().git_user(), token);
    let refname = format!("refs/heads/{}", p.target.branch);

    // An existing change is only ever updated in place: still open, still
    // this branch of this repository into this base.
    let verified = match &p.target.change {
        Some(change) => {
            let state =
                change_state(provider, cfg, &publisher, &env, p.target, change.number).await?;
            if let Some(refusal) = state.refusal(provider, p.target, change.number) {
                return Err(PublishError::Target(refusal));
            }
            Some(state)
        }
        None => None,
    };

    // Look before acting: a resumed attempt asks what the forge holds rather
    // than repeating a push whose outcome it never learned. An update always
    // looks, because what it may replace is exactly what it was reviewed on.
    let observed = if p.resume || p.target.change.is_some() {
        Some(remote_sha(cfg, &publisher, &credential, &refname).await?)
    } else {
        None
    };
    if let (Some(Some(found)), Some(lease)) = (&observed, p.lease) {
        if found != p.head_sha && found != lease {
            return Err(PublishError::LeaseLost {
                branch: p.target.branch.clone(),
                lease: lease.to_string(),
                found: found.clone(),
            });
        }
    }
    match observed {
        Some(Some(ref sha)) if sha == p.head_sha => {
            // The interrupted attempt's push did land. Nothing to repeat.
            p.journal.pushed(sha).map_err(PublishError::Broker)?;
        }
        Some(found) if p.pushed => {
            // A push this node recorded as confirmed is no longer what the
            // branch holds: someone moved or deleted it. Say so; do not
            // quietly push over whatever is there now.
            return Err(PublishError::RefMismatch {
                branch: p.target.branch.clone(),
                expected: p.head_sha.to_string(),
                found: found.unwrap_or_else(|| "nothing".into()),
            });
        }
        _ => {
            let refspec = format!("{}:refs/heads/{}", p.head_sha, p.target.branch);
            // A rewrite is forced only over the commit it was reviewed
            // against; anything else there makes the forge refuse it.
            let lease = match (p.rewrite, p.lease) {
                (true, Some(lease)) => Some(format!("--force-with-lease={refname}:{lease}")),
                _ => None,
            };
            let mut args = vec!["push"];
            args.extend(lease.as_deref());
            args.extend(["origin", &refspec]);
            publisher_git_with_credential(&cfg.publish.git, &publisher, &credential, args).await?;
            // A push that reports success is not evidence the forge kept it:
            // a ref-update hook can reject it after the fact, and a proxy can
            // answer for a repository that is not the one named.
            match remote_sha(cfg, &publisher, &credential, &refname).await? {
                Some(sha) if sha == p.head_sha => {
                    p.journal.pushed(&sha).map_err(PublishError::Broker)?
                }
                found => {
                    return Err(PublishError::RefMismatch {
                        branch: p.target.branch.clone(),
                        expected: p.head_sha.to_string(),
                        found: found.unwrap_or_else(|| "nothing".into()),
                    })
                }
            }
        }
    }

    if let Some(recheck) = p.before_push {
        recheck().map_err(PublishError::Broker)?;
    }
    p.journal.opening().map_err(PublishError::Broker)?;
    let (url, number) = match &p.target.change {
        Some(change) => {
            // Replacing a description is idempotent, so a resumed attempt
            // simply says it again.
            if let Some(description) = &p.outputs.description {
                edit_description(
                    provider,
                    cfg,
                    &publisher,
                    &env,
                    p.target,
                    change.number,
                    description,
                )
                .await?;
            }
            let url = verified.map(|state| state.url).unwrap_or_default();
            (url, change.number)
        }
        None => {
            // The same question for the second side effect: if an interrupted
            // attempt already opened the change, record that one rather than
            // opening another.
            let found = if p.resume {
                existing_change(provider, cfg, &publisher, &env, p).await?
            } else {
                None
            };
            let url = match found {
                Some(url) => url,
                None => {
                    let description = p.outputs.description.clone().unwrap_or_default();
                    let body = format!("{}\n\n{}", description.body, marker_comment(p.id));
                    let args = open_change_args(
                        provider,
                        p.target,
                        &description.title,
                        body,
                        p.outputs.draft,
                    );
                    let argv: Vec<&str> = args.iter().map(String::as_str).collect();
                    run_cli(provider.command(cfg), &publisher, &env, &argv).await?
                }
            };
            let number = change_number(&url);
            (url, number.unwrap_or(0))
        }
    };
    if let Some(comment) = &p.outputs.comment {
        if number == 0 {
            return Err(PublishError::Unknown(format!(
                "{url} was opened, but its number could not be read from that address to post \
                 the comment; post it by hand or verify before retrying"
            )));
        }
        post_comment_once(provider, cfg, &publisher, &env, p, number, comment).await?;
    }
    p.journal.opened(&url).map_err(PublishError::Broker)?;
    Ok(url)
}

/// The number a forge gives a change, from its web address: the last path
/// segment of `…/pull/7` or `…/-/merge_requests/7`.
fn change_number(url: &str) -> Option<u64> {
    url.trim()
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .and_then(|n| n.parse().ok())
}

/// A REST call to the forge through its own CLI, so the brokered environment
/// is the only credential it can use.
async fn forge_api(
    provider: Provider,
    cfg: &Config,
    dir: &Path,
    env: &BTreeMap<String, String>,
    method: &str,
    path: &str,
    fields: &[(&str, &str)],
) -> Result<Value, PublishError> {
    let mut args: Vec<String> = vec!["api".into(), "-X".into(), method.into(), path.into()];
    for (key, value) in fields {
        // `-f` is a raw field: a value is never read as `@file`.
        args.push("-f".into());
        args.push(format!("{key}={value}"));
    }
    let argv: Vec<&str> = args.iter().map(String::as_str).collect();
    let out = run_cli(provider.command(cfg), dir, env, &argv).await?;
    serde_json::from_str(&out).map_err(|error| PublishError::Refused {
        cli: provider.command(cfg),
        stderr: format!("{method} {path} answered something that is not JSON ({error})"),
    })
}

fn gitlab_project(project: &str) -> String {
    project.replace('/', "%2F")
}

fn change_path(provider: Provider, target: &Target, number: u64) -> String {
    match provider {
        Provider::Github => format!("repos/{}/pulls/{number}", target.project),
        Provider::Gitlab => format!(
            "projects/{}/merge_requests/{number}",
            gitlab_project(&target.project)
        ),
    }
}

fn comments_path(provider: Provider, target: &Target, number: u64) -> String {
    match provider {
        Provider::Github => format!("repos/{}/issues/{number}/comments", target.project),
        Provider::Gitlab => format!("{}/notes", change_path(provider, target, number)),
    }
}

/// Where a forge read made outside a publication runs: node-owned and empty,
/// like the home publication's commands get.
pub fn inspection_dir() -> Result<PathBuf, PublishError> {
    let dir = git_remote::home(PUBLISH_HOME);
    std::fs::create_dir_all(&dir).map_err(|source| PublishError::Spawn {
        cli: "mkdir".into(),
        source,
    })?;
    Ok(dir)
}

/// What the forge holds for an existing change.
pub async fn change_state(
    provider: Provider,
    cfg: &Config,
    dir: &Path,
    env: &BTreeMap<String, String>,
    target: &Target,
    number: u64,
) -> Result<ChangeState, PublishError> {
    let v = forge_api(
        provider,
        cfg,
        dir,
        env,
        "GET",
        &change_path(provider, target, number),
        &[],
    )
    .await?;
    let text = |pointer: &str| {
        v.pointer(pointer)
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string()
    };
    Ok(match provider {
        Provider::Github => ChangeState {
            url: text("/html_url"),
            open: text("/state") == "open",
            source_branch: text("/head/ref"),
            target_branch: text("/base/ref"),
            cross_repository: !text("/head/repo/full_name").eq_ignore_ascii_case(&target.project),
            head_sha: text("/head/sha"),
        },
        Provider::Gitlab => ChangeState {
            url: text("/web_url"),
            open: text("/state") == "opened",
            source_branch: text("/source_branch"),
            target_branch: text("/target_branch"),
            cross_repository: v.get("source_project_id") != v.get("target_project_id"),
            head_sha: text("/sha"),
        },
    })
}

/// The open change for `target.branch`, if the forge has one: `(number, url)`.
pub async fn open_change_for_branch(
    provider: Provider,
    cfg: &Config,
    dir: &Path,
    env: &BTreeMap<String, String>,
    target: &Target,
) -> Result<Option<(u64, String)>, PublishError> {
    let path = match provider {
        Provider::Github => {
            let owner = target.project.split('/').next().unwrap_or_default();
            format!(
                "repos/{}/pulls?state=open&head={owner}:{}",
                target.project, target.branch
            )
        }
        Provider::Gitlab => format!(
            "projects/{}/merge_requests?state=opened&source_branch={}",
            gitlab_project(&target.project),
            target.branch
        ),
    };
    let listed = forge_api(provider, cfg, dir, env, "GET", &path, &[]).await?;
    let (number_key, url_key) = match provider {
        Provider::Github => ("number", "html_url"),
        Provider::Gitlab => ("iid", "web_url"),
    };
    Ok(listed.as_array().into_iter().flatten().find_map(|change| {
        Some((
            change.get(number_key)?.as_u64()?,
            change.get(url_key)?.as_str()?.to_string(),
        ))
    }))
}

async fn edit_description(
    provider: Provider,
    cfg: &Config,
    dir: &Path,
    env: &BTreeMap<String, String>,
    target: &Target,
    number: u64,
    description: &Description,
) -> Result<(), PublishError> {
    let (method, body_key) = match provider {
        Provider::Github => ("PATCH", "body"),
        Provider::Gitlab => ("PUT", "description"),
    };
    forge_api(
        provider,
        cfg,
        dir,
        env,
        method,
        &change_path(provider, target, number),
        &[("title", &description.title), (body_key, &description.body)],
    )
    .await?;
    Ok(())
}

/// Post the comment unless a resumed attempt finds this publication already
/// posted it: a comment, unlike a description, is not idempotent.
async fn post_comment_once(
    provider: Provider,
    cfg: &Config,
    dir: &Path,
    env: &BTreeMap<String, String>,
    p: &Publication<'_>,
    number: u64,
    comment: &str,
) -> Result<(), PublishError> {
    let path = comments_path(provider, p.target, number);
    let marker = marker_comment(p.id);
    if p.resume {
        let listed = forge_api(
            provider,
            cfg,
            dir,
            env,
            "GET",
            &format!("{path}?per_page=100"),
            &[],
        )
        .await
        .map_err(|error| {
            PublishError::Unknown(format!(
                "the forge could not be asked whether this publication already commented \
                     ({error}); verify it before retrying"
            ))
        })?;
        let posted = listed.as_array().into_iter().flatten().any(|c| {
            c.get("body")
                .and_then(Value::as_str)
                .is_some_and(|body| body.contains(&marker))
        });
        if posted {
            return Ok(());
        }
    }
    let body = format!("{comment}\n\n{marker}");
    forge_api(provider, cfg, dir, env, "POST", &path, &[("body", &body)]).await?;
    Ok(())
}

/// The CLI arguments that open the change. The squash choice is left to the
/// project's own merge settings: glab has no negated squash flag, and the
/// squash-merge subject is what semantic-release reads.
fn open_change_args(
    provider: Provider,
    target: &Target,
    title: &str,
    body: String,
    draft: bool,
) -> Vec<String> {
    let mut args = open_change_args_ready(provider, target, title, body);
    if draft {
        args.push("--draft".into());
    }
    args
}

fn open_change_args_ready(
    provider: Provider,
    target: &Target,
    title: &str,
    body: String,
) -> Vec<String> {
    match provider {
        Provider::Github => vec![
            "pr".into(),
            "create".into(),
            "--repo".into(),
            target.project.clone(),
            "--base".into(),
            target.base.clone(),
            "--head".into(),
            target.branch.clone(),
            "--title".into(),
            title.to_string(),
            "--body".into(),
            body,
        ],
        Provider::Gitlab => vec![
            "mr".into(),
            "create".into(),
            "--repo".into(),
            target.project.clone(),
            "--target-branch".into(),
            target.base.clone(),
            "--source-branch".into(),
            target.branch.clone(),
            "--title".into(),
            title.to_string(),
            "--description".into(),
            body,
            "--yes".into(),
        ],
    }
}

/// The marker that identifies a change this publication opened. A merge
/// request is not addressable by anything the node chose — the forge assigns
/// the number — so the publication's own id goes in the body, where a resumed
/// attempt can find it again.
pub fn marker_comment(id: &str) -> String {
    format!("<!-- tracon-publication:{id} -->")
}

/// What the forge's branch points at, or `None` when the branch is absent. An
/// unreachable forge is `Unknown`: the push may or may not have landed, and
/// only the forge can settle that.
async fn remote_sha(
    cfg: &Config,
    publisher: &Path,
    credential: &Credential<'_>,
    refname: &str,
) -> Result<Option<String>, PublishError> {
    let listed = publisher_git_with_credential(
        &cfg.publish.git,
        publisher,
        credential,
        ["ls-remote", "origin", refname],
    )
    .await
    .map_err(|error| {
        PublishError::Unknown(format!(
            "the forge could not be asked what {refname} holds ({error}); \
             the push may or may not have landed — verify the branch before retrying"
        ))
    })?;
    Ok(listed
        .lines()
        .find_map(|line| line.split_whitespace().next())
        .map(str::to_string))
}

/// The change this publication already opened, if it did. Matching is on the
/// marker in the body, not on the source branch alone: a change somebody else
/// opened from the same branch is not this publication's.
async fn existing_change(
    provider: Provider,
    cfg: &Config,
    publisher: &Path,
    env: &BTreeMap<String, String>,
    p: &Publication<'_>,
) -> Result<Option<String>, PublishError> {
    let args: Vec<String> = match provider {
        Provider::Github => vec![
            "pr".into(),
            "list".into(),
            "--repo".into(),
            p.target.project.clone(),
            "--head".into(),
            p.target.branch.clone(),
            "--state".into(),
            "all".into(),
            "--limit".into(),
            "50".into(),
            "--json".into(),
            "url,body".into(),
        ],
        Provider::Gitlab => vec![
            "mr".into(),
            "list".into(),
            "--repo".into(),
            p.target.project.clone(),
            "--source-branch".into(),
            p.target.branch.clone(),
            "--all".into(),
            "--output".into(),
            "json".into(),
        ],
    };
    let argv: Vec<&str> = args.iter().map(String::as_str).collect();
    let listed = run_cli(provider.command(cfg), publisher, env, &argv)
        .await
        .map_err(|error| {
            PublishError::Unknown(format!(
                "the forge could not be asked whether this publication already opened a \
                 {} ({error}); verify it before retrying",
                provider.noun()
            ))
        })?;
    let parsed: Value = serde_json::from_str(&listed).map_err(|error| {
        PublishError::Unknown(format!(
            "the forge's list of open changes could not be read ({error}); \
             verify whether a {} is already open before retrying",
            provider.noun()
        ))
    })?;
    let marker = marker_comment(p.id);
    Ok(parsed
        .as_array()
        .into_iter()
        .flatten()
        .find(|change| {
            ["body", "description"]
                .iter()
                .filter_map(|key| change.get(*key).and_then(Value::as_str))
                .any(|body| body.contains(&marker))
        })
        .and_then(|change| {
            ["url", "web_url"]
                .iter()
                .find_map(|key| change.get(*key).and_then(Value::as_str))
        })
        .map(str::to_string))
}

fn publisher_dir(id: &str) -> Result<PathBuf, PublishError> {
    if id.is_empty()
        || !id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
    {
        return Err(PublishError::Target(
            "publication id must be an opaque safe id".into(),
        ));
    }
    Ok(Config::state_dir().join("publishers").join(id))
}

fn remote_url(
    provider: Provider,
    env: &BTreeMap<String, String>,
    project: &str,
) -> Result<String, PublishError> {
    if project.is_empty()
        || project.starts_with('/')
        || project.split('/').any(|part| {
            part.is_empty()
                || part == "."
                || part == ".."
                || !part
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
        })
    {
        return Err(PublishError::Target("project path is not canonical".into()));
    }
    let raw_host = match provider {
        Provider::Github => env
            .get("GITHUB_HOST")
            .map(String::as_str)
            .unwrap_or("github.com"),
        Provider::Gitlab => env
            .get("GITLAB_HOST")
            .map(String::as_str)
            .unwrap_or("gitlab.com"),
    };
    let host = raw_host
        .trim()
        .trim_start_matches("https://")
        .trim_start_matches("http://")
        .trim_end_matches('/');
    if host.is_empty()
        || !host
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b':'))
    {
        return Err(PublishError::Target("forge host is not canonical".into()));
    }
    Ok(format!("https://{host}/{project}.git"))
}

async fn source_git<'a>(
    git: &str,
    dir: &Path,
    args: impl IntoIterator<Item = &'a str>,
) -> Result<String, PublishError> {
    let mut command = git_remote::git_in(git, dir, PUBLISH_HOME, &Credential::Anonymous);
    command.args(args);
    output(git, command).await
}

async fn init_publisher(git: &str, dir: &Path) -> Result<String, PublishError> {
    let mut command = git_remote::git(git, PUBLISH_HOME, &Credential::Anonymous);
    command.args(["init", "--bare"]).arg(dir);
    output(git, command).await
}

async fn publisher_git<'a>(
    git: &str,
    dir: &Path,
    args: impl IntoIterator<Item = &'a str>,
) -> Result<String, PublishError> {
    publisher_git_with_credential(git, dir, &Credential::Anonymous, args).await
}

async fn publisher_git_with_credential<'a>(
    git: &str,
    dir: &Path,
    credential: &Credential<'_>,
    args: impl IntoIterator<Item = &'a str>,
) -> Result<String, PublishError> {
    let mut command = git_remote::git_bare(git, dir, PUBLISH_HOME, credential);
    command.args(args);
    output(git, command).await
}

async fn run_cli(
    cli: String,
    dir: &Path,
    env: &BTreeMap<String, String>,
    args: &[&str],
) -> Result<String, PublishError> {
    let mut command = Command::new(&cli);
    command
        .args(args)
        .current_dir(dir)
        .env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env("HOME", git_remote::home(PUBLISH_HOME))
        .envs(env);
    output(&cli, command).await
}

async fn output(cli: &str, mut command: Command) -> Result<String, PublishError> {
    let out = command
        .output()
        .await
        .map_err(|source| PublishError::Spawn {
            cli: cli.to_string(),
            source,
        })?;
    if out.status.success() {
        let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
        Ok(if stdout.is_empty() {
            String::from_utf8_lossy(&out.stderr).trim().to_string()
        } else {
            stdout
        })
    } else {
        Err(PublishError::Refused {
            cli: cli.to_string(),
            stderr: String::from_utf8_lossy(&out.stderr).trim().to_string(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn publication<'a>(
        target: &'a Target,
        outputs: &'a Outputs,
        journal: &'a NoJournal,
    ) -> Publication<'a> {
        Publication {
            channel: "work",
            node_id: "n1",
            id: "pub1",
            candidate: "/nonexistent-candidate",
            target,
            head_sha: "deadbeef",
            reviewed_tree: "cafebabe",
            outputs,
            lease: None,
            rewrite: false,
            resume: false,
            pushed: false,
            before_push: None,
            journal,
        }
    }

    #[test]
    fn gitlab_change_leaves_squash_to_the_project() {
        let target = Target {
            provider: "gitlab".into(),
            project: "group/app".into(),
            base: "master".into(),
            branch: "fix/x".into(),
            worktree: None,
            change: None,
        };
        let args = open_change_args(Provider::Gitlab, &target, "fix: x", "b".into(), false);
        assert_eq!(
            args,
            [
                "mr",
                "create",
                "--repo",
                "group/app",
                "--target-branch",
                "master",
                "--source-branch",
                "fix/x",
                "--title",
                "fix: x",
                "--description",
                "b",
                "--yes",
            ]
        );
        assert!(!args.iter().any(|a| a.contains("squash")));
    }

    #[test]
    fn providers_map_to_their_cli_and_credential() {
        assert_eq!(Provider::parse("gitlab"), Some(Provider::Gitlab));
        assert_eq!(Provider::parse("mr"), Some(Provider::Gitlab));
        assert_eq!(Provider::parse("github"), Some(Provider::Github));
        assert_eq!(Provider::parse("pr"), Some(Provider::Github));
        assert_eq!(Provider::parse("bitbucket"), None);
        assert_eq!(Provider::Gitlab.credential(), "glab");
        assert_eq!(Provider::Github.credential(), "gh");
    }

    #[tokio::test]
    async fn publishing_without_a_bound_credential_is_refused_before_anything_runs() {
        let broker = crate::broker::Broker::default().shared();
        let target = Target {
            provider: "gitlab".into(),
            project: "custom-development/integrations".into(),
            base: "main".into(),
            branch: "feat/x".into(),
            worktree: None,
            change: None,
        };
        let err = publish(
            &broker,
            &Config::default(),
            &publication(&target, &Outputs::default(), &NoJournal),
        )
        .await
        .unwrap_err();
        assert!(matches!(err, PublishError::Broker(_)), "{err}");
    }

    #[tokio::test]
    async fn an_unknown_provider_is_refused() {
        let target = Target {
            provider: "bitbucket".into(),
            project: "x/y".into(),
            base: "main".into(),
            branch: "feat/x".into(),
            worktree: None,
            change: None,
        };
        let err = publish(
            &crate::broker::Broker::default().shared(),
            &Config::default(),
            &publication(&target, &Outputs::default(), &NoJournal),
        )
        .await
        .unwrap_err();
        assert!(matches!(err, PublishError::UnknownProvider { .. }));
    }

    /// The reviewed tree is what makes the pushed bytes the reviewed bytes.
    /// Without one there is nothing to compare against, so publication stops
    /// rather than proceeding on the candidate directory's own word.
    #[tokio::test]
    async fn a_candidate_with_no_recorded_tree_is_not_published() {
        let broker = toml::from_str::<crate::broker::Broker>(
            "[credentials.gh]\nchannels = [\"work\"]\n[credentials.gh.env]\nGH_TOKEN = \"t\"\n",
        )
        .unwrap()
        .shared();
        let target = Target {
            provider: "github".into(),
            project: "owner/name".into(),
            base: "main".into(),
            branch: "feat/x".into(),
            worktree: None,
            change: None,
        };
        let journal = NoJournal;
        let outputs = Outputs::default();
        let mut p = publication(&target, &outputs, &journal);
        p.reviewed_tree = "";
        let err = publish(&broker, &Config::default(), &p).await.unwrap_err();
        assert!(matches!(err, PublishError::NoReviewedTree), "{err}");
    }

    #[test]
    fn publication_remote_is_reconstructed_not_read_from_candidate_config() {
        let env = BTreeMap::from([("GITHUB_HOST".into(), "github.example".into())]);
        assert_eq!(
            remote_url(Provider::Github, &env, "group/project").unwrap(),
            "https://github.example/group/project.git"
        );
        assert!(remote_url(Provider::Github, &env, "../project").is_err());
    }

    #[test]
    fn an_unknown_outcome_is_neither_success_nor_failure() {
        assert!(PublishError::Unknown("x".into()).is_unknown());
        assert!(!PublishError::IdentityChanged.is_unknown());
    }
}
