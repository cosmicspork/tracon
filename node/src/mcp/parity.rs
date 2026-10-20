//! What each forge operation is called on GitHub and on GitLab.
//!
//! The two tool sets live apart (`github.rs`, `gitlab.rs`) and keep the
//! names agents already know from `gh` and `glab`, so nothing but this table
//! ties them together. Every tool either forge offers is in it, mapped to its
//! counterpart or to the reason it has none, and a test fails when a tool is
//! added to one forge without that decision.

use super::{github, gitlab};

/// One side of an operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    /// The tool that does it.
    Tool(&'static str),
    /// Nothing does it, and why.
    None(&'static str),
}

/// One operation and how each forge does it.
#[derive(Debug, Clone, Copy)]
pub struct Operation {
    pub what: &'static str,
    pub github: Side,
    pub gitlab: Side,
    /// A CI write: asked, followed once it ran, and listed in the README's
    /// paragraph on CI so an agent reading it knows what it can start.
    pub ci_write: bool,
}

const fn both(what: &'static str, github: &'static str, gitlab: &'static str) -> Operation {
    Operation {
        what,
        github: Side::Tool(github),
        gitlab: Side::Tool(gitlab),
        ci_write: false,
    }
}

pub const OPERATIONS: &[Operation] = &[
    both("a change's state", github::PR_STATUS, gitlab::MR_STATUS),
    both(
        "comment on a change",
        github::PR_COMMENT,
        gitlab::MR_COMMENT,
    ),
    Operation {
        what: "comment on an issue",
        github: Side::Tool(github::ISSUE_COMMENT),
        gitlab: Side::None(
            "the GitLab tools reach merge requests and pipelines, not GitLab's issue tracker; \
             an issue there is commented on by a person",
        ),
        ci_write: false,
    },
    both(
        "a change's review threads",
        github::PR_THREADS,
        gitlab::MR_DISCUSSIONS,
    ),
    both("reply to a thread", github::PR_REPLY, gitlab::MR_REPLY),
    both(
        "the open change for a branch",
        github::PR_FOR_BRANCH,
        gitlab::MR_FOR_BRANCH,
    ),
    both("merge a change", github::PR_MERGE, gitlab::MR_MERGE),
    both(
        "a run or pipeline and its jobs",
        github::RUN_STATUS,
        gitlab::PIPELINE_STATUS,
    ),
    // GitHub's run_status takes `sha` where GitLab has a tool of its own.
    both(
        "the runs at an exact commit",
        github::RUN_STATUS,
        gitlab::PIPELINE_LIST_BY_SHA,
    ),
    both(
        "wait for a run or pipeline to move",
        github::RUN_WAIT,
        gitlab::PIPELINE_WAIT,
    ),
    both(
        "the end of a job's log",
        github::RUN_LOGS,
        gitlab::JOB_TRACE,
    ),
    Operation {
        ci_write: true,
        ..both(
            "retry the failed jobs",
            github::RUN_RERUN,
            gitlab::PIPELINE_RETRY,
        )
    },
    Operation {
        ci_write: true,
        ..both(
            "start a run on a branch",
            github::RUN_DISPATCH,
            gitlab::PIPELINE_RUN,
        )
    },
    Operation {
        what: "play a manual job",
        github: Side::None(
            "GitHub Actions has no manual job: a job held for an environment's required \
             reviewers is approved by a person on GitHub, and a workflow started by hand \
             is run_dispatch",
        ),
        gitlab: Side::Tool(gitlab::JOB_PLAY),
        ci_write: true,
    },
    Operation {
        what: "deploy from an exact commit",
        github: Side::None(
            "a GitHub deployment is a workflow run gated by environment protection, not an \
             existing manual job to verify and play at a SHA; a deploy workflow the operator \
             lists is run_dispatch",
        ),
        gitlab: Side::Tool(gitlab::DEPLOY),
        ci_write: true,
    },
];

#[cfg(test)]
mod tests {
    use super::*;

    fn tools(side: fn(&Operation) -> Side) -> Vec<&'static str> {
        OPERATIONS
            .iter()
            .filter_map(|op| match side(op) {
                Side::Tool(name) => Some(name),
                Side::None(_) => None,
            })
            .collect()
    }

    /// A tool added to one forge without a decision about the other fails
    /// here: put it in `OPERATIONS`, with its counterpart or the reason
    /// there is none.
    #[test]
    fn every_forge_tool_has_a_decision_about_the_other_forge() {
        let github_named = tools(|op| op.github);
        let gitlab_named = tools(|op| op.gitlab);
        for name in github::TOOLS {
            assert!(
                github_named.contains(name),
                "{name} is not in mcp::parity::OPERATIONS"
            );
        }
        for name in gitlab::TOOLS {
            assert!(
                gitlab_named.contains(name),
                "{name} is not in mcp::parity::OPERATIONS"
            );
        }
        for name in &github_named {
            assert!(github::TOOLS.contains(name), "{name} is not a GitHub tool");
        }
        for name in &gitlab_named {
            assert!(gitlab::TOOLS.contains(name), "{name} is not a GitLab tool");
        }
        for op in OPERATIONS {
            for side in [op.github, op.gitlab] {
                if let Side::None(why) = side {
                    assert!(!why.trim().is_empty(), "{}: say why", op.what);
                }
            }
            assert!(
                matches!(op.github, Side::Tool(_)) || matches!(op.gitlab, Side::Tool(_)),
                "{}: neither forge does it",
                op.what
            );
        }
    }

    /// The README's CI paragraph is what an agent reads to learn what it can
    /// start; one that leaves a write out has had an agent conclude it could
    /// not (#462).
    #[test]
    fn the_readme_names_every_ci_write() {
        let readme = include_str!("../../../README.md");
        for op in OPERATIONS.iter().filter(|op| op.ci_write) {
            for side in [op.github, op.gitlab] {
                if let Side::Tool(name) = side {
                    assert!(
                        readme.contains(&format!("`{name}`")),
                        "README.md does not name {name}"
                    );
                }
            }
        }
    }
}
