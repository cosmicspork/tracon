//! Text in a tool's definition that only one audience needs.
//!
//! The node keeps one definition per tool, written for every caller, and
//! narrows it here for the one reading it. A session the node runs never holds
//! a worktree path or names its project, and its orientation already says once
//! how an asked call answers and how a wait ends; a harness the operator runs
//! has no orientation, holds no item and is not in a review session. Each side
//! is told its own half and not the other's, so neither spends a request on
//! instructions it cannot act on.
//!
//! The sentences are matched exactly against the definitions, and a test fails
//! when one no longer appears in any of them.

use serde_json::Value;

use super::Audience;

/// How every tool the operator decides says so, in its own description: a
/// session's orientation says it once instead (`corpus::orientation`).
pub const ASKED: &str = "An asked call returns `awaiting_operator` with an `approval_id` at \
                         once, and approval_status reports the outcome.";

/// The opening of a parameter only a harness the operator runs passes. A
/// session is not offered the parameter at all.
pub const EXTERNAL_PARAM: &str = "Only for a harness you run yourself";

/// Sentences a session is not told: what a harness outside the boundary does.
const EXTERNAL_ONLY: &[&str] = &[
    "A harness you run yourself holds no item, so its items are linked to none and are marked \
     with your lane; pass `worktree` to bind the item to that repository's project, or leave it \
     out and the item belongs to the whole channel, with no project.",
    "A harness you run yourself passes the item's `id`.",
    "A harness you run yourself sees the whole channel, or one project by `project` or by a \
     `worktree` of its repository.",
    "A harness you run yourself must pass `review_id`, and paths are read from that review's \
     worktree.",
    "A harness you run yourself passes `worktree`: the absolute path of a worktree whose \
     repository is under the node's [external] repo_roots; its branch is what gets pushed.",
];

/// Sentences a harness the operator runs is not told: what happens inside a
/// session the node started.
const SESSION_ONLY: &[&str] = &[
    "In a session the node started it is linked to this session's item as discovered-from and \
     bound to this session's project, so it survives the session.",
    "A session sees its own project unless it names another.",
    "In a review session it goes on the review you are reviewing, marked with that review's \
     candidate.",
];

/// `tool` as `audience` reads it.
pub fn shape(tool: &mut Value, audience: Audience) {
    let session = matches!(audience, Audience::Session(_));
    if let Some(text) = tool["description"].as_str() {
        let mut text = text.to_string();
        if session {
            let pointer = super::wait::pointer();
            for sentence in EXTERNAL_ONLY
                .iter()
                .copied()
                .chain([ASKED, pointer.as_str()])
            {
                text = text.replace(sentence, "");
            }
        } else {
            for sentence in SESSION_ONLY {
                text = text.replace(sentence, "");
            }
        }
        tool["description"] = Value::String(tidy(&text));
    }
    if !session {
        return;
    }
    let Some(schema) = tool.get_mut("inputSchema").and_then(Value::as_object_mut) else {
        return;
    };
    let mut dropped = Vec::new();
    if let Some(properties) = schema.get_mut("properties").and_then(Value::as_object_mut) {
        properties.retain(|name, property| {
            let external = property["description"]
                .as_str()
                .is_some_and(|d| d.starts_with(EXTERNAL_PARAM));
            if external {
                dropped.push(name.clone());
            }
            !external
        });
    }
    if let Some(required) = schema.get_mut("required").and_then(Value::as_array_mut) {
        required.retain(|key| !dropped.iter().any(|d| key == d));
    }
}

/// The spaces a removed sentence leaves, closed up.
fn tidy(text: &str) -> String {
    text.split(' ')
        .filter(|word| !word.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::{
        approvals, consulta, docs, egress, github, gitlab, jira, memory, operator, review,
        services, setup, show, work,
    };
    use crate::session::Phase;

    fn every_definition() -> Vec<Value> {
        let mut all = Vec::new();
        all.extend(github::definitions());
        all.extend(gitlab::definitions());
        all.extend(jira::definitions());
        all.extend(consulta::definitions(&["qa".to_string()]));
        all.extend(review::definitions());
        all.extend(memory::definitions());
        all.extend(docs::definitions(&crate::config::default_doc_kinds()));
        all.extend(operator::definitions());
        all.extend(egress::definitions());
        all.extend(show::definitions());
        all.extend(setup::definitions());
        all.extend(work::definitions());
        all.extend(approvals::definitions());
        all.extend(services::definitions_for(&["browser"], "browser"));
        all
    }

    /// A sentence edited in its tool but not here would quietly stop being
    /// narrowed: every one listed must still be found.
    #[test]
    fn every_audience_specific_sentence_is_still_in_a_definition() {
        let all = every_definition();
        let text: Vec<&str> = all
            .iter()
            .filter_map(|t| t["description"].as_str())
            .collect();
        for sentence in EXTERNAL_ONLY.iter().chain(SESSION_ONLY).chain([&ASKED]) {
            assert!(
                text.iter().any(|d| d.contains(sentence)),
                "no definition says {sentence:?}"
            );
        }
    }

    /// A tool the shipped bundle leaves to the operator says so, or a harness
    /// plans around a result it will not get at once (#740). A session is told
    /// once in its orientation instead; the tools the dispatcher answers before
    /// the policy gate, and those that run on scoped authority, are not asked
    /// this way.
    #[test]
    fn every_tool_the_shipped_bundle_asks_says_so() {
        use crate::policy::{Policy, Request, Verdict};
        let policy = Policy::shipped();
        let kinds = crate::config::default_doc_kinds();
        let none = serde_json::json!({});
        let before_the_gate = [
            operator::ASK,
            operator::NOTIFY,
            operator::REPORT,
            operator::REPORT_STATUS,
            operator::QUESTION_STATUS,
            show::SHOW,
            approvals::STATUS,
        ];
        let mut silent = Vec::new();
        for tool in every_definition() {
            let name = tool["name"].as_str().unwrap();
            if !Audience::External.offers(name)
                || before_the_gate.contains(&name)
                || crate::mcp::consequential_name(name)
            {
                continue;
            }
            let decision = policy.decide(&Request {
                channel: "work",
                action: name,
                kind: Some(crate::mcp::TOOL_KIND),
                resource: None,
                command: None,
                arguments: Some(&none),
                doc_kinds: Some(&kinds),
            });
            let said = tool["description"].as_str().unwrap();
            if decision.verdict == Verdict::Ask && !said.contains(ASKED) {
                silent.push(name.to_string());
            }
        }
        assert!(silent.is_empty(), "asked without saying so: {silent:?}");
    }

    #[test]
    fn a_session_is_told_nothing_meant_for_a_harness_outside_the_boundary() {
        for mut tool in every_definition() {
            shape(&mut tool, Audience::Session(Phase::Execute));
            let said = tool.to_string();
            assert!(!said.contains("harness you run yourself"), "{said}");
            assert!(!said.contains(ASKED), "{said}");
            assert!(!said.contains("Blocks at most"), "{said}");
            assert!(!said.contains("  "), "{said}");
            assert!(
                tool["inputSchema"]["properties"]["worktree"].is_null(),
                "{said}"
            );
        }
    }

    #[test]
    fn a_harness_outside_the_boundary_keeps_its_half_and_loses_the_sessions() {
        for mut tool in every_definition() {
            let before = tool["inputSchema"]["properties"]["worktree"].clone();
            shape(&mut tool, Audience::External);
            let said = tool.to_string();
            assert!(!said.contains("In a session the node started"), "{said}");
            assert!(!said.contains("In a review session it goes"), "{said}");
            assert_eq!(tool["inputSchema"]["properties"]["worktree"], before);
        }
        let mut submit = review::definitions()
            .into_iter()
            .find(|t| t["name"] == review::SUBMIT)
            .unwrap();
        shape(&mut submit, Audience::External);
        let said = submit["description"].as_str().unwrap();
        assert!(
            said.contains("A harness you run yourself passes `worktree`"),
            "{said}"
        );
    }
}
