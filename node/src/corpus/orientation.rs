//! What a session is told before its first prompt: three layers assembled on
//! the node, never a file in the worktree. Shared conventions come from the
//! corpus (documents of kind `guide` on the channel), node facts from what
//! the node knows about itself, and the channel's policy from the bundle —
//! plus the directives and confident facts recalled for the project. Capped,
//! because oversized context degrades the session it was meant to help.

use crate::{
    policy::{Policy, Verdict},
    store::{MemoryRow, Store},
};

/// Roughly six thousand tokens at four characters each.
pub const CAP_CHARS: usize = 24_000;
/// A single guide document may take at most this much of the cap.
const GUIDE_CHARS: usize = 8_000;

pub struct Facts<'a> {
    pub node_name: &'a str,
    pub node_id: &'a str,
    pub backend: &'a str,
    pub harness: &'a str,
    pub harness_version: &'a str,
    pub channel: &'a str,
    pub project_id: Option<&'a str>,
    pub project_name: Option<&'a str>,
    pub tools: &'a [String],
    pub worktree: &'a str,
}

/// The orientation text, and whether the cap trimmed it.
pub fn assemble(store: &Store, policy: &Policy, facts: &Facts) -> (String, bool) {
    let mut out = String::new();
    let mut trimmed = false;

    out.push_str("# Orientation\n\nAssembled by the tracon node for this session; nothing here is in the repository.\n\n");

    // 1. Shared conventions: guides on the channel, shortest first so a long
    //    one cannot crowd out the rest.
    let mut guides: Vec<_> = store
        .doc_list(Some(facts.channel))
        .unwrap_or_default()
        .into_iter()
        .filter(|d| d.kind == "guide")
        .filter_map(|d| store.doc_by_id(&d.id).ok().flatten())
        .collect();
    guides.sort_by_key(|d| d.body.len());
    if !guides.is_empty() {
        out.push_str("## Conventions\n\n");
        for g in guides {
            let mut body = g.body.trim().to_string();
            if body.len() > GUIDE_CHARS {
                body.truncate(floor_char(&body, GUIDE_CHARS));
                body.push_str("\n\n[trimmed; call doc_read for the whole document]");
                trimmed = true;
            }
            out.push_str(&format!("### {} (`{}`)\n\n{}\n\n", g.title, g.slug, body));
        }
    }

    // 2. This node.
    out.push_str("## This node\n\n");
    out.push_str(&format!(
        "- Node `{}` ({}…), runtime {}, harness {} {}.\n",
        facts.node_name,
        &facts.node_id[..8.min(facts.node_id.len())],
        facts.backend,
        facts.harness,
        facts.harness_version
    ));
    out.push_str(&format!("- Channel `{}`.", facts.channel));
    match (facts.project_name, facts.project_id) {
        (Some(n), Some(id)) => out.push_str(&format!(
            " Project `{}` ({}…).\n",
            n,
            &id[..8.min(id.len())]
        )),
        _ => out.push('\n'),
    }
    out.push_str(&format!(
        "- Your worktree is `{}`; the main checkout is not yours.\n",
        facts.worktree
    ));
    if facts.tools.is_empty() {
        out.push_str("- No node tools are offered on this channel.\n\n");
    } else {
        out.push_str(&format!(
            "- Node tools (MCP server `tracon`): {}.\n\n",
            facts
                .tools
                .iter()
                .map(|t| format!("`{t}`"))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }

    // 3. Policy: what is refused, and why, so a refusal reads as expected.
    let denies: Vec<_> = policy
        .rules
        .iter()
        .filter(|r| r.verdict == Verdict::Deny)
        .collect();
    if !denies.is_empty() {
        out.push_str(
            "## Working agreements\n\nThese are enforced by the node, not requested of you:\n\n",
        );
        for r in denies {
            out.push_str(&format!("- **{}** — {}\n", r.id, r.reason));
        }
        out.push('\n');
    }

    // 3b. What is known: directives always, confident facts for the project.
    let known: Vec<MemoryRow> = store
        .directives_for(facts.channel, facts.project_id)
        .unwrap_or_default();
    if !known.is_empty() {
        out.push_str("## Known\n\n");
        for m in known {
            let tag = if m.kind == "directive" {
                "directive"
            } else {
                "fact"
            };
            out.push_str(&format!("- ({tag}) {}\n", m.body.trim()));
        }
        out.push_str("\nCall `recall` for more; `retain` what you learn.\n");
    }

    if out.len() > CAP_CHARS {
        out.truncate(floor_char(&out, CAP_CHARS));
        out.push_str("\n\n[orientation trimmed to its cap]\n");
        trimmed = true;
    }
    (out, trimmed)
}

fn floor_char(s: &str, at: usize) -> usize {
    let mut i = at;
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tracon_sync::ChangeOp;

    #[test]
    fn three_layers_and_the_known_in_order_under_the_cap() {
        let store = Store::open_in_memory().unwrap();
        store
            .write_change("n", "personal", "document", ChangeOp::Upsert, "g", json!({
                "channel": "personal", "slug": "guide-workspace", "kind": "guide", "title": "Workspace",
                "body": "# Workspace\n\nConventional commits.", "hash": "h", "created_ms": 1, "updated_ms": 1}))
            .unwrap();
        store
            .write_change(
                "n",
                "personal",
                "document",
                ChangeOp::Upsert,
                "r",
                json!({
                "channel": "personal", "slug": "ref-x", "kind": "ref", "title": "X",
                "body": "not a guide", "hash": "h", "created_ms": 1, "updated_ms": 1}),
            )
            .unwrap();
        store
            .write_change("n", "personal", "memory", ChangeOp::Upsert, "m", json!({
                "channel": "personal", "scope": "global", "scope_ref": null, "kind": "directive",
                "body": "run just test", "source_session": null, "source_node": null, "confidence": 1.0,
                "state": "active", "created_ms": 1, "updated_ms": 1}))
            .unwrap();
        let facts = Facts {
            node_name: "bazzite",
            node_id: "0123456789abcdef",
            backend: "podman",
            harness: "omp",
            harness_version: "18.0.4",
            channel: "personal",
            project_id: Some("p1"),
            project_name: Some("tracon"),
            tools: &["recall".into(), "retain".into()],
            worktree: "/work",
        };
        let (text, trimmed) = assemble(&store, &Policy::shipped(), &facts);
        assert!(!trimmed);
        let i = |s: &str| {
            text.find(s)
                .unwrap_or_else(|| panic!("missing {s:?} in:\n{text}"))
        };
        assert!(i("## Conventions") < i("Conventional commits"));
        assert!(!text.contains("not a guide"));
        assert!(i("## This node") < i("## Working agreements"));
        assert!(i("no-merge") < i("## Known"));
        assert!(text.contains("(directive) run just test"));
        assert!(text.contains("`recall`, `retain`"));
        assert!(text.contains("Project `tracon`"));
    }

    #[test]
    fn a_long_guide_is_trimmed_not_dropped() {
        let store = Store::open_in_memory().unwrap();
        let long = "x".repeat(GUIDE_CHARS * 2);
        store
            .write_change(
                "n",
                "personal",
                "document",
                ChangeOp::Upsert,
                "g",
                json!({
                "channel": "personal", "slug": "guide-long", "kind": "guide", "title": "Long",
                "body": long, "hash": "h", "created_ms": 1, "updated_ms": 1}),
            )
            .unwrap();
        let facts = Facts {
            node_name: "n",
            node_id: "id",
            backend: "local",
            harness: "fake",
            harness_version: "1",
            channel: "personal",
            project_id: None,
            project_name: None,
            tools: &[],
            worktree: "/work",
        };
        let (text, trimmed) = assemble(&store, &Policy::default(), &facts);
        assert!(trimmed);
        assert!(text.contains("[trimmed; call doc_read"));
        assert!(text.len() < CAP_CHARS);
    }
}
