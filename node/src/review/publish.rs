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
    /// Fixed at first submit. Once there is a change its `base` is fixed too,
    /// because moving an opened change is the forge's business; until then a
    /// resubmission may name another base.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub change: Option<ChangeRef>,
}

impl Target {
    /// The target after publication opened the change at `url`: what a
    /// resubmission of the same review updates rather than opening another.
    pub fn opened(mut self, url: &str) -> Self {
        if self.change.is_none() {
            self.change = change_number(url)
                .filter(|number| *number > 0)
                .map(|number| ChangeRef {
                    number,
                    url: url.to_string(),
                });
        }
        self
    }
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

/// What a revision ships under the operator's name besides its tree: what
/// the forge shows, and the message and branch the tree is pushed with. The
/// review's own title and body are the operator's summary; they reach the
/// forge only as a new change's description when no other description is
/// given, and as the squashed commit's message when no message is.
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
    /// The message the squashed commit carries. Meaningless when the
    /// revision's commits are pushed as written.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
    /// The operator's rename of the branch a new change is pushed to. An
    /// existing change's branch is the change's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
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
            self.branch = None;
        }
        self
    }
}

/// One revision's publication intent, pinned when it is submitted.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Intent {
    #[serde(default)]
    pub forge: Outputs,
    /// What the branch held on the forge when this revision was submitted,
    /// for an existing change or a declared rewrite. A push may replace
    /// exactly this and nothing else.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lease: Option<String>,
    /// The branch's history was rewritten, so the push is not a fast-forward
    /// of `lease` and is forced — but only over `lease`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub rewrite: bool,
    /// The commit the reviewed tree is squashed onto: `lease` when the push
    /// adds to what the branch holds, else where the branch leaves its base.
    /// Absent pushes the agent's commits as written.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub squash_onto: Option<String>,
    /// The agent's commits beyond the base, oldest first, as listed beside
    /// the diff.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub commits: Vec<super::prose::CommitLine>,
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
    #[error(
        "{onto:.8}, which this revision is squashed onto, is not on the forge's {branch} any \
         more; resubmit on top of what it holds now"
    )]
    SquashBaseGone { branch: String, onto: String },
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
    /// About to speak to the forge. Everything before this ran on this node
    /// alone, so an attempt that stops earlier was definitely not attempted
    /// as far as the forge is concerned.
    fn contacting(&self) -> Result<(), String>;
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
    fn contacting(&self) -> Result<(), String> {
        Ok(())
    }
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
    /// name. It has to be derived from the review, revision, target and
    /// commit so that a retry resumes the record the interrupted attempt left.
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
    /// For an existing change or a declared rewrite: what the branch held at
    /// submit, and whether the push replaces that history rather than
    /// fast-forwarding it.
    pub lease: Option<&'a str>,
    pub rewrite: bool,
    /// Push one commit holding the reviewed tree instead of `head_sha`.
    pub squash: Option<Squash<'a>>,
    /// A previous attempt for this same publication reached, or may have
    /// reached, the forge. Its side effects are observed rather than repeated.
    pub resume: bool,
    /// A previous attempt recorded a push the forge confirmed.
    pub pushed: bool,
    pub before_push: Option<&'a (dyn Fn() -> Result<(), String> + Send + Sync)>,
    pub journal: &'a dyn Journal,
}

/// One commit carrying exactly the reviewed tree, made in the publisher
/// repository. Its parent, message, author and committer are all fixed before
/// it is made — the author and dates are the candidate head's own — so a
/// resumed attempt makes the identical commit and recognises it on the forge.
#[derive(Debug, Clone, Copy)]
pub struct Squash<'a> {
    pub onto: &'a str,
    pub message: &'a str,
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
    p.journal.contacting().map_err(PublishError::Broker)?;
    let token = provider.forge().token(&env).map(String::as_str);
    let credential = git_remote::brokered(provider.forge().git_user(), token);
    let refname = format!("refs/heads/{}", p.target.branch);

    // What is pushed: the reviewed commit, or one commit holding its tree.
    let pushing = match &p.squash {
        None => p.head_sha.to_string(),
        Some(squash) => squash_commit(cfg, &publisher, &credential, p, squash, &refname).await?,
    };
    let pushing = pushing.as_str();

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
    // than repeating a push whose outcome it never learned. An update or a
    // leased push always looks, because what it may replace is exactly what
    // it was reviewed on.
    let observed = if p.resume || p.target.change.is_some() || p.lease.is_some() {
        Some(remote_sha(cfg, &publisher, &credential, &refname).await?)
    } else {
        None
    };
    if let (Some(Some(found)), Some(lease)) = (&observed, p.lease) {
        if found != pushing && found != lease {
            return Err(PublishError::LeaseLost {
                branch: p.target.branch.clone(),
                lease: lease.to_string(),
                found: found.clone(),
            });
        }
    }
    match observed {
        Some(Some(ref sha)) if sha == pushing => {
            // The interrupted attempt's push did land. Nothing to repeat.
            p.journal.pushed(sha).map_err(PublishError::Broker)?;
        }
        Some(found) if p.pushed => {
            // A push this node recorded as confirmed is no longer what the
            // branch holds: someone moved or deleted it. Say so; do not
            // quietly push over whatever is there now.
            return Err(PublishError::RefMismatch {
                branch: p.target.branch.clone(),
                expected: pushing.to_string(),
                found: found.unwrap_or_else(|| "nothing".into()),
            });
        }
        _ => {
            let refspec = format!("{}:refs/heads/{}", pushing, p.target.branch);
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
                Some(sha) if sha == pushing => {
                    p.journal.pushed(&sha).map_err(PublishError::Broker)?
                }
                found => {
                    return Err(PublishError::RefMismatch {
                        branch: p.target.branch.clone(),
                        expected: pushing.to_string(),
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
                existing_change(provider, cfg, &publisher, &env, p, pushing).await?
            } else {
                None
            };
            let url = match found {
                Some(url) => url,
                None => {
                    // Exactly the approved text: nothing is added that the
                    // operator did not see.
                    let description = p.outputs.description.clone().unwrap_or_default();
                    let args = open_change_args(
                        provider,
                        p.target,
                        &description.title,
                        description.body,
                        p.outputs.draft,
                    );
                    let argv: Vec<&str> = args.iter().map(String::as_str).collect();
                    run_cli(provider, cfg, &publisher, &env, &argv).await?
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
    let out = run_cli(provider, cfg, dir, env, &argv).await?;
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

/// What `target.branch` holds on the forge, or `None` when there is no such
/// branch.
pub async fn branch_head(
    provider: Provider,
    cfg: &Config,
    dir: &Path,
    env: &BTreeMap<String, String>,
    target: &Target,
) -> Result<Option<String>, PublishError> {
    let (path, pointer) = match provider {
        Provider::Github => (
            format!("repos/{}/git/ref/heads/{}", target.project, target.branch),
            "/object/sha",
        ),
        Provider::Gitlab => (
            format!(
                "projects/{}/repository/branches/{}",
                gitlab_project(&target.project),
                target.branch.replace('/', "%2F")
            ),
            "/commit/id",
        ),
    };
    match forge_api(provider, cfg, dir, env, "GET", &path, &[]).await {
        Ok(v) => match v.pointer(pointer).and_then(Value::as_str) {
            Some(sha) if !sha.is_empty() => Ok(Some(sha.to_string())),
            _ => Err(PublishError::Refused {
                cli: provider.command(cfg),
                stderr: format!("GET {path} named no commit"),
            }),
        },
        Err(PublishError::Refused { stderr, .. }) if stderr.contains("404") => Ok(None),
        Err(error) => Err(error),
    }
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
/// posted it: a comment, unlike a description, is not idempotent. The comment
/// is posted exactly as approved, so on resume it is recognised by its text —
/// or by the marker earlier versions appended.
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
    let marker = legacy_marker(p.id);
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
                .is_some_and(|body| body.contains(&marker) || same_text(body, comment))
        });
        if posted {
            return Ok(());
        }
    }
    forge_api(provider, cfg, dir, env, "POST", &path, &[("body", comment)]).await?;
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

/// The marker earlier versions appended to an opened change's body and to its
/// comment. Nothing writes it any more — approval publishes exactly the text
/// the operator saw — but a publication those versions interrupted is still
/// recognised by it when resumed.
pub fn legacy_marker(id: &str) -> String {
    format!("<!-- tracon-publication:{id} -->")
}

/// Whether text read back from a forge is the text that was sent. Forges
/// normalise line endings and trailing whitespace, and nothing else.
fn same_text(found: &str, sent: &str) -> bool {
    let norm = |s: &str| s.replace("\r\n", "\n").trim().to_string();
    norm(found) == norm(sent)
}

/// What the forge's branch points at, or `None` when the branch is absent. An
/// unreachable forge is `Unknown`: the push may or may not have landed, and
/// only the forge can settle that.
/// Make the squashed commit in the publisher repository and return it. The
/// commit it sits on is in the candidate's history for a new branch; for one
/// that adds to what the forge's branch holds it may be an earlier squash the
/// agent never had, so it is fetched from the branch.
async fn squash_commit(
    cfg: &Config,
    publisher: &Path,
    credential: &Credential<'_>,
    p: &Publication<'_>,
    squash: &Squash<'_>,
    refname: &str,
) -> Result<String, PublishError> {
    let git = &cfg.publish.git;
    let commit = format!("{}^{{commit}}", squash.onto);
    let held = |dir: &Path| {
        let commit = commit.clone();
        let dir = dir.to_path_buf();
        async move {
            publisher_git(git, &dir, ["cat-file", "-e", commit.as_str()])
                .await
                .is_ok()
        }
    };
    if !held(publisher).await {
        let fetched = publisher_git_with_credential(
            git,
            publisher,
            credential,
            [
                "fetch",
                "--no-tags",
                "origin",
                &format!("+{refname}:refs/tracon/forge"),
            ],
        )
        .await;
        if fetched.is_err() || !held(publisher).await {
            return Err(PublishError::SquashBaseGone {
                branch: p.target.branch.clone(),
                onto: squash.onto.to_string(),
            });
        }
    }
    // A revision that changes nothing the branch holds pushes nothing: an
    // empty commit is not a revision.
    let onto_tree = publisher_git(
        git,
        publisher,
        ["rev-parse", &format!("{}^{{tree}}", squash.onto)],
    )
    .await?;
    if onto_tree == p.reviewed_tree {
        return Ok(squash.onto.to_string());
    }
    let who = publisher_git(
        git,
        publisher,
        [
            "log",
            "-1",
            "--date=raw",
            "--format=%an%x00%ae%x00%ad%x00%cn%x00%ce%x00%cd",
            "refs/heads/candidate",
        ],
    )
    .await?;
    let who: Vec<&str> = who.split('\0').collect();
    let [author, author_email, author_date, committer, committer_email, committer_date] = who[..]
    else {
        return Err(PublishError::IdentityChanged);
    };
    let mut command = git_remote::git_bare(git, publisher, PUBLISH_HOME, &Credential::Anonymous);
    command
        .env("GIT_AUTHOR_NAME", author)
        .env("GIT_AUTHOR_EMAIL", author_email)
        .env("GIT_AUTHOR_DATE", author_date)
        .env("GIT_COMMITTER_NAME", committer)
        .env("GIT_COMMITTER_EMAIL", committer_email)
        .env("GIT_COMMITTER_DATE", committer_date)
        .args(["commit-tree", p.reviewed_tree, "-p", squash.onto, "-m"])
        .arg(squash.message);
    let made = output(git, command).await?;
    // The tree is the reviewed one by construction; read it back anyway, the
    // way every other identity on this path is.
    let tree = publisher_git(git, publisher, ["rev-parse", &format!("{made}^{{tree}}")]).await?;
    if tree != p.reviewed_tree {
        return Err(PublishError::IdentityChanged);
    }
    Ok(made)
}

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

/// The change this publication already opened, if it did.
///
/// A change carries nothing the node chose — the forge assigns the number —
/// so it is recognised by what it was opened with: this branch of this
/// repository, into this base, at the reviewed commit, with exactly the
/// approved title and description. A change from the branch at another commit
/// is not this publication's. One at this commit whose text differs, or more
/// than one that matches, is not guessed between: the outcome is `Unknown`
/// and the operator verifies it. A change an earlier version opened is
/// recognised by the marker it wrote into the body.
async fn existing_change(
    provider: Provider,
    cfg: &Config,
    publisher: &Path,
    env: &BTreeMap<String, String>,
    p: &Publication<'_>,
    pushed: &str,
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
            "url,title,body,headRefOid,baseRefName,isCrossRepository".into(),
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
    let listed = run_cli(provider, cfg, publisher, env, &argv)
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
    let changes: Vec<Listed> = parsed
        .as_array()
        .into_iter()
        .flatten()
        .map(|change| Listed::read(provider, change))
        .collect();
    match recognise(&changes, p, pushed) {
        Recognised::Ours(url) => Ok(Some(url)),
        Recognised::None => Ok(None),
        Recognised::Ambiguous(why) => Err(PublishError::Unknown(format!(
            "{why}; verify which {} this publication opened before retrying",
            provider.noun()
        ))),
    }
}

/// One change as a forge's list reports it.
#[derive(Debug, Default)]
struct Listed {
    url: String,
    title: String,
    body: String,
    head_sha: String,
    base: String,
    cross_repository: bool,
}

impl Listed {
    fn read(provider: Provider, change: &Value) -> Self {
        let text = |key: &str| {
            change
                .get(key)
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string()
        };
        match provider {
            Provider::Github => Listed {
                url: text("url"),
                title: text("title"),
                body: text("body"),
                head_sha: text("headRefOid"),
                base: text("baseRefName"),
                cross_repository: change
                    .get("isCrossRepository")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
            },
            Provider::Gitlab => Listed {
                url: text("web_url"),
                title: text("title"),
                body: text("description"),
                head_sha: text("sha"),
                base: text("target_branch"),
                cross_repository: change.get("source_project_id")
                    != change.get("target_project_id"),
            },
        }
    }
}

#[derive(Debug, PartialEq)]
enum Recognised {
    Ours(String),
    None,
    Ambiguous(String),
}

fn recognise(changes: &[Listed], p: &Publication<'_>, pushed: &str) -> Recognised {
    let marker = legacy_marker(p.id);
    if let Some(change) = changes.iter().find(|c| c.body.contains(&marker)) {
        return Recognised::Ours(change.url.clone());
    }
    let at_head: Vec<&Listed> = changes
        .iter()
        .filter(|c| !c.cross_repository && c.base == p.target.base && c.head_sha == pushed)
        .collect();
    if at_head.is_empty() {
        return Recognised::None;
    }
    let description = p.outputs.description.clone().unwrap_or_default();
    let ours: Vec<&&Listed> = at_head
        .iter()
        .filter(|c| {
            c.title.trim() == description.title.trim() && same_text(&c.body, &description.body)
        })
        .collect();
    match ours.as_slice() {
        [one] => Recognised::Ours(one.url.clone()),
        [] => Recognised::Ambiguous(format!(
            "{} is open at the reviewed commit, but not with the approved title and description",
            at_head[0].url
        )),
        many => Recognised::Ambiguous(format!(
            "{} changes match the reviewed commit and the approved text",
            many.len()
        )),
    }
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

/// Bounded by `[publish] forge_timeout_secs`. A call that runs out of time
/// may still have done what it was asked on the forge, so it is `Unknown`
/// rather than a refusal.
async fn run_cli(
    provider: Provider,
    cfg: &Config,
    dir: &Path,
    env: &BTreeMap<String, String>,
    args: &[&str],
) -> Result<String, PublishError> {
    let cli = provider.command(cfg);
    let mut command = Command::new(&cli);
    command
        .args(args)
        .current_dir(dir)
        .env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env("HOME", git_remote::home(PUBLISH_HOME))
        .envs(env)
        .kill_on_drop(true);
    let limit = std::time::Duration::from_secs(cfg.publish.forge_timeout_secs);
    tokio::time::timeout(limit, output(&cli, command))
        .await
        .unwrap_or_else(|_| {
            Err(PublishError::Unknown(format!(
                "{cli} {} did not answer within {}s",
                args.first().copied().unwrap_or_default(),
                limit.as_secs()
            )))
        })
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
            squash: None,
            resume: false,
            pushed: false,
            before_push: None,
            journal,
        }
    }

    fn listed(url: &str, title: &str, body: &str, head: &str) -> Listed {
        Listed {
            url: url.into(),
            title: title.into(),
            body: body.into(),
            head_sha: head.into(),
            base: "main".into(),
            cross_repository: false,
        }
    }

    fn approved() -> (Target, Outputs) {
        let target = Target {
            provider: "github".into(),
            project: "owner/name".into(),
            base: "main".into(),
            branch: "feat/x".into(),
            worktree: None,
            change: None,
        };
        let outputs = Outputs {
            description: Some(Description {
                title: "feat: x".into(),
                body: "why\nand how".into(),
            }),
            ..Outputs::default()
        };
        (target, outputs)
    }

    #[test]
    fn a_resumed_publication_recognises_its_change_by_commit_and_approved_text() {
        let (target, outputs) = approved();
        let journal = NoJournal;
        let p = publication(&target, &outputs, &journal);
        let changes = [
            // An earlier change from the branch, at another commit.
            listed("u/1", "feat: x", "why\nand how", "0ld"),
            // The forge hands the body back with its own line endings.
            listed("u/2", "feat: x", "why\r\nand how\n", "deadbeef"),
        ];
        assert_eq!(
            recognise(&changes, &p, p.head_sha),
            Recognised::Ours("u/2".into())
        );
    }

    #[test]
    fn a_change_only_at_another_commit_is_not_this_publications() {
        let (target, outputs) = approved();
        let journal = NoJournal;
        let p = publication(&target, &outputs, &journal);
        let mut elsewhere = listed("u/1", "feat: x", "why\nand how", "deadbeef");
        elsewhere.base = "release".into();
        let mut fork = listed("u/2", "feat: x", "why\nand how", "deadbeef");
        fork.cross_repository = true;
        let changes = [
            listed("u/3", "feat: x", "why\nand how", "0ld"),
            elsewhere,
            fork,
        ];
        assert_eq!(recognise(&changes, &p, p.head_sha), Recognised::None);
    }

    #[test]
    fn a_change_at_the_commit_with_other_text_or_two_matches_is_not_guessed_between() {
        let (target, outputs) = approved();
        let journal = NoJournal;
        let p = publication(&target, &outputs, &journal);
        let edited = [listed("u/1", "feat: x", "someone edited this", "deadbeef")];
        assert!(matches!(
            recognise(&edited, &p, p.head_sha),
            Recognised::Ambiguous(why) if why.contains("u/1")
        ));
        let twice = [
            listed("u/1", "feat: x", "why\nand how", "deadbeef"),
            listed("u/2", "feat: x", "why\nand how", "deadbeef"),
        ];
        assert!(matches!(
            recognise(&twice, &p, p.head_sha),
            Recognised::Ambiguous(_)
        ));
    }

    #[test]
    fn a_change_an_earlier_version_marked_is_still_recognised() {
        let (target, outputs) = approved();
        let journal = NoJournal;
        let p = publication(&target, &outputs, &journal);
        let body = format!("why\nand how\n\n{}", legacy_marker("pub1"));
        let changes = [listed("u/9", "feat: x", &body, "deadbeef")];
        assert_eq!(
            recognise(&changes, &p, p.head_sha),
            Recognised::Ours("u/9".into())
        );
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
