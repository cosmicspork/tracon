//! What a session is told before its first prompt: assembled on the node,
//! never a file in the worktree.
//!
//! Two things are being separated here, and they are separated twice over.
//!
//! **Whose words they are.** Everything this module generates is tracon's own
//! account of the installation: the node, the session's work and phase, the
//! tool names, what the node enforces. It is followed by two sections that are
//! not tracon's — "Operator notes", the standing text the operator set for
//! this channel, and "Pinned documents", the documents the operator has chosen
//! to have every session start with, in full. A session that cannot tell one
//! from the other cannot weigh either: it reads a human's preference as a
//! property of the system, or a system fact as advice it may trade away. So
//! each of those two carries a heading and a sentence saying where it came
//! from, and nothing personal is ever compiled into this file.
//!
//! **What the cap may take.** *Reserved* is what this session is for and what
//! bounds it: the node, the work item and its phase, the brief it points at,
//! the plan or the diff under review, the agreements, the operator's
//! directives, notes, customization, and any document the operator has
//! pinned to the channel. It is assembled first and is never dropped to make
//! room for anything below it — a pinned document goes in whole or not at
//! all, never truncated; the operator's own curation is the limit, not a
//! byte cap. *Discretionary* is everything else the node offers because it
//! might help: the other ready work on the project, in ledger order, with
//! whatever the reserved tier left of the cap.
//!
//! The cap exists because oversized context degrades the session it was meant
//! to help. It used to be enforced by truncating the assembled text, which cut
//! from the end — where the task, the constraints and the directives lived. A
//! session told the conventions but not the job is worse off than one told
//! neither, so the cap now binds the discretionary tier and nothing else.
//!
//! Whatever does not fit is *named*: which piece, how much of it is missing,
//! and the call that fetches the rest. A bare "trimmed" flag cannot be acted
//! on, because the agent cannot tell whether the thing it needs was one of the
//! things it did not get.

use tracon_sync::work::WorkItem;

use crate::{
    policy::{Policy, Verdict},
    store::{MemoryRow, ReviewRow, Store, WorkView},
};

/// Roughly six thousand tokens at four characters each. Binds the
/// discretionary tier; see the module note on why it no longer truncates the
/// whole text.
pub const CAP_CHARS: usize = 24_000;
/// A diff longer than this is cut; the reviewer has the worktree and git.
const DIFF_CHARS: usize = 12_000;
/// Each reserved piece is bounded too, so that nothing inside the reserved
/// tier can starve anything else inside it.
const BODY_CHARS: usize = 8_000;
/// The proposed change's own description, for a review session.
const REQUIREMENTS_CHARS: usize = 4_000;
/// Directives and recalled facts.
const KNOWN_CHARS: usize = 4_000;
/// The operator's standing notes for this channel. Its own allowance, so a
/// long skill list cannot squeeze the operator's own words out and a long
/// note cannot hide which skills exist.
const NOTES_CHARS: usize = 6_000;
/// The skills and agents the channel's launch manifest installed.
const CUSTOMIZATION_CHARS: usize = 6_000;
/// Initial room held back from discretionary context for omission notices.
/// The notice may exceed this so that it never hides one omission behind
/// another generic truncation flag.
const NOTICE_CHARS: usize = 1_500;

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
    /// `plan`, `execute`, or `review`.
    pub phase: &'a str,
    /// The item this session holds.
    pub item: Option<&'a WorkItem>,
    /// The plan document's body, for an execute session.
    pub plan_body: Option<&'a str>,
    /// Ready work on the project, for `work_discover` deps and context.
    pub ready: &'a [WorkView],
    /// For a review session: the review to read.
    pub review: Option<&'a ReviewRow>,
    /// The channel's launch manifest. Its instructions and agents are told to
    /// the session here rather than written into the harness's own
    /// `instructions` key: the node already has one place a session's
    /// standing text goes, and instruction content grants no permission on
    /// either path.
    pub manifest: &'a crate::manifest::LaunchManifest,
}

/// Context this orientation could not carry in full. Named, with the call
/// that fetches the rest, so the session can go and get what it is missing
/// instead of not knowing that it is missing anything.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Missing {
    /// What was left out, in the session's own terms — `guide "Workspace"`.
    pub what: String,
    /// True when some of it is in the text above and the rest is not; false
    /// when none of it is here at all.
    pub partial: bool,
    /// How many characters are not here.
    pub chars: usize,
    /// The call that fetches the whole thing, when there is one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fetch: Option<String>,
}

impl Missing {
    fn line(&self) -> String {
        let scope = if self.partial {
            "cut short"
        } else {
            "not included"
        };
        let fetch = match &self.fetch {
            Some(how) => format!(" — {how}"),
            None => String::new(),
        };
        format!("- {} {scope}, {} chars{fetch}\n", self.what, self.chars)
    }
}

/// Append `body` to `out`, truncated to `cap`, and name the remainder if it
/// did not fit whole. Returns nothing: what could not be said is in `missing`.
fn push_capped(
    out: &mut String,
    body: &str,
    cap: usize,
    what: &str,
    fetch: Option<&str>,
    missing: &mut Vec<Missing>,
) {
    let body = body.trim();
    if body.len() <= cap {
        out.push_str(body);
        return;
    }
    let at = floor_char(body, cap);
    out.push_str(&body[..at]);
    out.push_str("\n\n[cut here; see the end of this orientation]");
    missing.push(Missing {
        what: what.to_string(),
        partial: true,
        chars: body.len() - at,
        fetch: fetch.map(str::to_string),
    });
}

/// The orientation text, and everything the cap kept out of it.
pub fn assemble(store: &Store, policy: &Policy, facts: &Facts) -> (String, Vec<Missing>) {
    let mut missing = Vec::new();

    // Reserved. Assembled before the cap is consulted at all: this is what
    // the session is for, and none of it is negotiable against a guide.
    let mut out = String::new();
    out.push_str(
        "# Orientation\n\nAssembled by the tracon node for this session; nothing here is in \
         the repository.\n\nIt carries two kinds of thing, and they are not read the same \
         way. **The system orientation comes first and is tracon's own**: what this node is, \
         what this session is for, which tools exist, and what the node refuses. Take it as \
         fact about the system. **Any section headed \"Operator notes\" or \"Channel guides\" \
         is not tracon's**: those are the person running this node, and this channel's own \
         documents, speaking. Take them as instruction from a human — useful, and not a \
         property of the system.\n\n",
    );
    push_node(&mut out, facts);
    push_work(&mut out, facts, &mut missing);
    push_review(&mut out, facts, &mut missing);
    push_agreements(&mut out, policy);
    push_known(&mut out, store, facts, &mut missing);
    push_customization(&mut out, facts, &mut missing);
    push_operator_notes(&mut out, facts, &mut missing);
    push_pinned_documents(&mut out, store, facts);

    // Discretionary. Whatever the reserved tier — including every pinned
    // document, in full — left under the cap, minus the room held back to
    // name what does not fit.
    let left = CAP_CHARS.saturating_sub(out.len() + NOTICE_CHARS);
    push_ready(&mut out, facts, left, &mut missing);

    push_notice(&mut out, &missing);
    (out, missing)
}

/// Where the session is and what it can call. Small, bounded, and needed to
/// act on any of what follows.
fn push_node(out: &mut String, facts: &Facts) {
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
}

/// The item, its phase, what the phase must produce, and the plan.
fn push_work(out: &mut String, facts: &Facts, missing: &mut Vec<Missing>) {
    let Some(item) = facts.item else { return };
    out.push_str(&format!(
        "## Work\n\n**{}** (`{}…`, phase: {})\n\n",
        item.title,
        &item.id[..8.min(item.id.len())],
        facts.phase
    ));
    if !item.body.trim().is_empty() {
        push_capped(
            out,
            &item.body,
            BODY_CHARS,
            "the work item's description",
            // No tool returns the held item's own body: `work_ready` lists
            // what no session holds. The only honest route to the rest is the
            // operator, so that is what is named.
            Some("ask for the rest with `ask_operator`"),
            missing,
        );
        out.push_str("\n\n");
    }
    if let Some(slug) = &item.brief_slug {
        // A pointer, not the brief: what the work is for is worth 25 tokens
        // of the reserved space, and the brief itself is a call away.
        out.push_str(&format!(
            "This item has a product brief (`{slug}`): who the work is for, what would make it \
             good, and what is still open. `brief_read` returns it, with every line marked \
             observed, inferred or decided. `brief_note` adds a line to it — `observed` must \
             cite what the customer said or did, `inferred` is your own reasoning, and a \
             decision is the operator's and is refused. Every line you add goes to the \
             operator for approval before it lands.\n\n"
        ));
    }
    if let Some(from) = &item.discovered_from {
        out.push_str(&format!(
            "Discovered from item `{}…`.\n\n",
            &from[..8.min(from.len())]
        ));
    }
    match facts.phase {
        "plan" => {
            let slug = crate::corpus::work::plan_slug(&item.id);
            out.push_str(&format!(
                "This is a **plan session**: read, ask `recall`, and think; write no code. \
                 It ends when you write the plan as document `{slug}` with `doc_write` \
                 (that slug alone needs no approval). Say what will change, where, how it \
                 is verified, and what you are unsure of. An execute session follows and \
                 reads only that document and this item.\n\n"
            ));
        }
        "execute" => {
            out.push_str(
                "This is an **execute session**: do the work in the worktree, commit it, and \
                 call `submit_review` — you hold no forge token, so the node snapshots the \
                 worktree and puts the change in front of the operator. Work you find but \
                 should not do now: `work_discover` records it against this item instead of \
                 losing it. `work_close` closes the item this session holds, and ends the \
                 session with it. Submitting is not closing: a change can be under review, \
                 or published and still awaiting a verdict on whether it was any good. Close \
                 when the item itself is finished, and leave it open otherwise.\n\n",
            );
            if let Some(plan) = facts.plan_body {
                out.push_str("### Plan\n\n");
                push_capped(
                    out,
                    plan,
                    BODY_CHARS,
                    "the plan",
                    Some(&format!(
                        "call `doc_read` for `{}`",
                        crate::corpus::work::plan_slug(&item.id)
                    )),
                    missing,
                );
                out.push_str("\n\n");
            }
        }
        _ => {}
    }
}

/// A review session: requirements and diff, nothing of how the diff came to
/// be. A fresh reader does not rationalise what it watched.
fn push_review(out: &mut String, facts: &Facts, missing: &mut Vec<Missing>) {
    let Some(r) = facts.review else { return };
    out.push_str(&format!(
        "## Review\n\nThis is a **review session**. You did not write this change and have \
         not seen how it was made; judge the diff against the requirements above and the \
         plan, run the tests in your worktree if it helps, and end by calling \
         `review_verdict` (approve, or request_changes with findings). A human decides \
         after you; your verdict informs them.\n\n### Proposed change\n\n**{}**\n\n",
        r.title
    ));
    push_capped(
        out,
        &r.body,
        REQUIREMENTS_CHARS,
        "the proposed change's description",
        Some("call `review_status` for the whole description"),
        missing,
    );
    out.push_str(&format!(
        "\n\n### Diff ({} added, {} removed, base `{}`)\n\n```diff\n",
        r.added, r.removed, r.base_ref
    ));
    let mut diff = r.diff.clone();
    if diff.len() > DIFF_CHARS {
        let at = floor_char(&diff, DIFF_CHARS);
        missing.push(Missing {
            what: "the diff".into(),
            partial: true,
            chars: diff.len() - at,
            fetch: Some("run `git diff` in the worktree for the rest".into()),
        });
        diff.truncate(at);
        diff.push_str("\n[diff cut here; run `git diff` in the worktree for the rest]");
    }
    out.push_str(&diff);
    out.push_str("\n```\n\n");
}

/// What is refused, and why, so a refusal reads as expected.
fn push_agreements(out: &mut String, policy: &Policy) {
    let denies: Vec<_> = policy
        .rules
        .iter()
        .filter(|r| r.verdict == Verdict::Deny)
        .collect();
    if denies.is_empty() {
        return;
    }
    out.push_str(
        "## Working agreements\n\nThese are enforced by the node, not requested of you:\n\n",
    );
    for r in denies {
        out.push_str(&format!("- **{}** — {}\n", r.id, r.reason));
    }
    // The gate answers in the task's own terms rather than as an auth error
    // (`mcp::refusal`). Saying so is one sentence, and it is the difference
    // between an agent that reads the reason and one that goes looking for a
    // way round what it took for a broken tool.
    out.push_str(
        "\nA refused call comes back as `refused by policy (<rule>): <reason>` — the rule's \
         own words, not an error. Take the route the reason names, or ask the operator; \
         there is no way around it to find.\n\n",
    );
}

/// Directives always, confident facts for the project. Bounded, and the
/// count left out is named rather than the tail going quietly missing.
fn push_known(out: &mut String, store: &Store, facts: &Facts, missing: &mut Vec<Missing>) {
    let known: Vec<MemoryRow> = store
        .directives_for(facts.channel, facts.project_id)
        .unwrap_or_default();
    if known.is_empty() {
        return;
    }
    out.push_str("## Known\n\n");
    let mut used = 0;
    let mut dropped = 0;
    let mut dropped_chars = 0;
    for m in &known {
        let tag = if m.kind == "directive" {
            "directive"
        } else {
            "fact"
        };
        let line = format!("- ({tag}) {}\n", m.body.trim());
        // A directive is the operator speaking. Keep it whole or leave it
        // out: a half-sentence can reverse what the operator asked for.
        if used + line.len() > KNOWN_CHARS {
            dropped += 1;
            dropped_chars += line.len();
            continue;
        }
        used += line.len();
        out.push_str(&line);
    }
    if dropped > 0 {
        let noun = if dropped == 1 {
            "directive or fact"
        } else {
            "directives and facts"
        };
        missing.push(Missing {
            what: format!("{dropped} more {noun}"),
            partial: false,
            chars: dropped_chars,
            fetch: Some("call `recall` for them".into()),
        });
    }
    out.push_str("\nCall `recall` for more; `retain` what you learn.\n\n");
}

/// What the channel's launch manifest installed: the skills this session may
/// call and the agents it may spawn. Reserved, and still tracon's own account
/// of the installation — what is *in* a skill is the operator's, but that this
/// session can call it is a fact about the node.
fn push_customization(out: &mut String, facts: &Facts, missing: &mut Vec<Missing>) {
    let text = facts.manifest.customization();
    if text.is_empty() {
        return;
    }
    push_capped(
        out,
        &text,
        CUSTOMIZATION_CHARS,
        "the launch manifest's skills and agents",
        Some("ask the operator; the manifest is not readable from inside the session"),
        missing,
    );
    out.push_str("\n\n");
}

/// The operator's standing notes for this channel: their own words, under
/// their own heading, the first of the reserved tier's human-authored
/// sections so the boundary between what tracon says and what a human says
/// is visible rather than inferred. The channel's pinned documents — other
/// documents the operator chose to include, not necessarily their own words
/// — follow it.
///
/// Reserved, and with an allowance of its own. These are the operator
/// speaking to every session on the channel — the same standing that a
/// directive has — so a long skill list must not be able to crowd them out,
/// nor they it.
fn push_operator_notes(out: &mut String, facts: &Facts, missing: &mut Vec<Missing>) {
    let text = facts.manifest.operator_notes();
    if text.is_empty() {
        return;
    }
    push_capped(
        out,
        &text,
        NOTES_CHARS,
        "the operator's notes",
        Some("ask the operator; the manifest is not readable from inside the session"),
        missing,
    );
    out.push_str("\n\n");
}

/// The heading the channel's pinned documents open under, and the sentence
/// saying whose they are. Not "Conventions": a pinned document is one the
/// operator chose, not one tracon selected, and calling it a convention
/// invites the session to read it as tracon's rule. It carries no budget
/// sentence, unlike every other section here that names one: there is none.
const PINNED_HEADING: &str = "## Pinned documents\n\nDocuments the operator has pinned to this \
                              channel's orientation. They are not tracon's and they are not \
                              selected for this task by the session: the operator chose to have \
                              every session start with them, in full. Where one contradicts the \
                              work item, the plan or the operator's notes, those win.\n\n";

/// The channel's pinned documents, alphabetically by title, each in full.
/// Reserved, not discretionary, and uncapped: an archived document is never
/// included even if its pinned flag is still set from before it was
/// archived, and an HTML document is never included — this is markdown text
/// compiled into a session's orientation, not a bundle a session can render.
fn push_pinned_documents(out: &mut String, store: &Store, facts: &Facts) {
    let mut docs: Vec<_> = store
        .doc_list(Some(facts.channel))
        .unwrap_or_default()
        .into_iter()
        .filter(|d| d.pinned != 0 && d.format == "markdown" && d.archived == 0)
        .filter_map(|d| store.doc_by_id(&d.id).ok().flatten())
        .collect();
    if docs.is_empty() {
        return;
    }
    docs.sort_by(|a, b| a.title.cmp(&b.title));
    out.push_str(PINNED_HEADING);
    for d in docs {
        out.push_str(&format!("### {} (`{}`)\n\n", d.title, d.slug));
        out.push_str(d.body.trim());
        out.push_str("\n\n");
    }
}

/// Background: what else is ready on this project, for `work_discover` deps.
/// It is discretionary too: keep complete entries inside the remaining
/// budget and name the rest as one fetchable piece.
fn push_ready(out: &mut String, facts: &Facts, mut left: usize, missing: &mut Vec<Missing>) {
    if facts.item.is_none() || facts.ready.is_empty() {
        return;
    }
    let heading = "## Ready work on this project\n\n";
    let mut opened = false;
    let mut included = 0;
    let mut omitted = 0;
    let mut omitted_chars = 0;
    for v in facts.ready {
        let line = format!(
            "- `{}…` {}\n",
            &v.item.id[..8.min(v.item.id.len())],
            v.item.title
        );
        let header = if opened { 0 } else { heading.len() };
        if included < 10 && header + line.len() < left {
            if !opened {
                out.push_str(heading);
                left -= header;
                opened = true;
            }
            left -= line.len();
            out.push_str(&line);
            included += 1;
        } else {
            omitted += 1;
            omitted_chars += line.len();
        }
    }
    if opened {
        out.push('\n');
    }
    if omitted > 0 {
        let noun = if omitted == 1 { "item" } else { "items" };
        missing.push(Missing {
            what: format!("ready work on this project ({omitted} {noun})"),
            partial: opened,
            chars: omitted_chars,
            fetch: Some("call `work_ready` for the full list".into()),
        });
    }
}

/// Name everything that did not fit. The session is owed this: it cannot ask
/// for a document it does not know was withheld. Notices may exceed their
/// initial allowance rather than silently reducing named omissions to
/// another generic flag.
fn push_notice(out: &mut String, missing: &[Missing]) {
    if missing.is_empty() {
        return;
    }
    out.push_str(
        "## Not in this orientation\n\nThe node's context cap kept the following out, not \
         policy. Ask for any of it you need:\n\n",
    );
    for m in missing {
        out.push_str(&m.line());
    }
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
    use tracon_sync::{work::Readiness, ChangeOp};

    #[test]
    fn three_layers_and_the_known_in_order_under_the_cap() {
        let store = Store::open_in_memory().unwrap();
        store
            .write_change("n", "personal", "document", ChangeOp::Upsert, "g", json!({
                "channel": "personal", "slug": "guide-commits", "kind": "guide", "title": "Commits",
                "body": "# Commits\n\nConventional commits.", "pinned": true,
                "hash": "h", "created_ms": 1, "updated_ms": 1}))
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
                "body": "not pinned", "hash": "h", "created_ms": 1, "updated_ms": 1}),
            )
            .unwrap();
        store
            .write_change(
                "n",
                "personal",
                "document",
                ChangeOp::Upsert,
                "html-guide",
                json!({
                "channel": "personal", "slug": "guide-html", "kind": "guide", "title": "HTML",
                "body": "<p>HTML must not become orientation</p>", "hash": "html", "format": "html",
                "entry_path": "index.html", "source_name": "index.html", "pinned": true,
                "created_ms": 1, "updated_ms": 1}),
            )
            .unwrap();
        store
            .write_change("n", "personal", "memory", ChangeOp::Upsert, "m", json!({
                "channel": "personal", "scope": "global", "scope_ref": null, "kind": "directive",
                "body": "run just test", "source_session": null, "source_node": null, "confidence": 1.0,
                "state": "active", "created_ms": 1, "updated_ms": 1}))
            .unwrap();
        let facts = Facts {
            node_name: "laptop",
            node_id: "0123456789abcdef",
            backend: "podman",
            harness: "opencode",
            harness_version: "18.0.4",
            channel: "personal",
            project_id: Some("p1"),
            project_name: Some("tracon"),
            tools: &["recall".into(), "retain".into()],
            worktree: "/work",
            phase: "execute",
            item: None,
            plan_body: None,
            ready: &[],
            review: None,
            manifest: &crate::manifest::LaunchManifest::default(),
        };
        let (text, missing) = assemble(&store, &Policy::shipped(), &facts);
        assert!(missing.is_empty(), "{missing:?}");
        let i = |s: &str| {
            text.find(s)
                .unwrap_or_else(|| panic!("missing {s:?} in:\n{text}"))
        };
        assert!(i("## Pinned documents") < i("Conventional commits"));
        assert!(!text.contains("not pinned"));
        assert!(!text.contains("HTML must not become orientation"));
        assert!(i("## This node") < i("## Working agreements"));
        // Pinned documents are reserved, but they come after the agreements
        // and the operator's directives in the reserved tier, not before them.
        assert!(i("## Known") < i("## Pinned documents"));
        assert!(i("no-merge") < i("## Known"));
        // And the session is told whose the pinned documents are, rather than
        // being left to read a channel document as a rule of the system.
        assert!(i("## Pinned documents") < i("They are not tracon's"));
        assert!(text.contains("(directive) run just test"));
        assert!(text.contains("`recall`, `retain`"));
        assert!(text.contains("Project `tracon`"));
    }

    fn item(title: &str, body: &str) -> WorkItem {
        WorkItem {
            id: "item0001deadbeef".into(),
            channel: "personal".into(),
            project_id: None,
            title: title.into(),
            body: body.into(),
            state: "ready".into(),
            priority: 0,
            deps: vec![],
            discovered_from: None,
            discovered_by_session: None,
            phase_plan_slug: None,
            brief_slug: None,
            closed_by_session: None,
            created_ms: 1,
            updated_ms: 1,
        }
    }

    fn pinned_docs(store: &Store, n: usize, chars: usize) {
        for i in 0..n {
            store
                .write_change(
                    "n",
                    "personal",
                    "document",
                    ChangeOp::Upsert,
                    &format!("g{i}"),
                    json!({
                    "channel": "personal", "slug": format!("guide-{i}"), "kind": "guide",
                    "title": format!("Guide {i}"), "body": "y".repeat(chars), "pinned": true,
                    "hash": format!("h{i}"), "created_ms": 1, "updated_ms": 1}),
                )
                .unwrap();
        }
    }

    /// The operator's launch manifest is their standing text for every
    /// session on the channel, so it is reserved beside the directives — and
    /// it is their *own* section, distinct both from what the node installed
    /// and from what the channel's pinned documents are.
    #[test]
    fn the_operators_notes_are_their_own_section_and_come_before_pinned_documents() {
        let store = Store::open_in_memory().unwrap();
        pinned_docs(&store, 2, 200);
        let manifest = crate::manifest::LaunchManifest {
            revision: 3,
            digest: "deadbeefcafe".into(),
            instructions: vec![crate::manifest::TextEntry {
                name: "house-style".into(),
                body: "Always write the test before the fix.".into(),
            }],
            agents: vec![crate::manifest::TextEntry {
                name: "scout".into(),
                body: "Reads before it writes.".into(),
            }],
            ..Default::default()
        };
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
            phase: "execute",
            item: None,
            plan_body: None,
            ready: &[],
            review: None,
            manifest: &manifest,
        };
        let (text, missing) = assemble(&store, &Policy::shipped(), &facts);
        let i = |s: &str| {
            text.find(s)
                .unwrap_or_else(|| panic!("missing {s:?} in:\n{text}"))
        };
        assert!(
            text.contains("Always write the test before the fix."),
            "{text}"
        );
        assert!(missing.is_empty(), "{missing:?}");

        // The operator's words are under their own heading, and the session is
        // told they are not the node's.
        assert!(i("## Operator notes") < i("Always write the test before the fix."));
        assert!(text.contains("not tracon's"), "{text}");
        // What the node installed is a separate section from what the
        // operator wrote, and both precede the channel's pinned documents.
        assert!(i("## Customization") < i("## Operator notes"));
        assert!(i("Reads before it writes.") < i("## Operator notes"));
        assert!(i("## Operator notes") < i("## Pinned documents"));
        // The system orientation comes first and says so.
        assert!(i("## This node") < i("## Operator notes"));
        assert!(i("is tracon's own") < i("## This node"));
    }

    #[test]
    fn an_oversized_directive_does_not_break_the_known_bound() {
        let store = Store::open_in_memory().unwrap();
        for (id, body, created_ms) in [
            ("large", "x".repeat(KNOWN_CHARS * 2), 2),
            ("small", "keep this directive".into(), 1),
        ] {
            store
                .write_change("n", "personal", "memory", ChangeOp::Upsert, id, json!({
                    "channel": "personal", "scope": "global", "scope_ref": null, "kind": "directive",
                    "body": body, "source_session": null, "source_node": null, "confidence": 1.0,
                    "state": "active", "created_ms": created_ms, "updated_ms": created_ms}))
                .unwrap();
        }
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
            phase: "execute",
            item: None,
            plan_body: None,
            ready: &[],
            review: None,
            manifest: &crate::manifest::LaunchManifest::default(),
        };
        let (text, missing) = assemble(&store, &Policy::default(), &facts);
        assert!(!text.contains(&"x".repeat(KNOWN_CHARS)), "{text}");
        assert!(text.contains("(directive) keep this directive"), "{text}");
        assert_eq!(missing.len(), 1, "{missing:?}");
        assert!(!missing[0].partial, "{missing:?}");
        assert!(missing[0].chars > KNOWN_CHARS, "{missing:?}");
    }

    #[test]
    fn ready_work_stays_inside_the_discretionary_budget() {
        let store = Store::open_in_memory().unwrap();
        let current = item("Current item", "");
        let ready_title = "r".repeat(CAP_CHARS * 2);
        let ready = [WorkView {
            item: WorkItem {
                id: "ready001deadbeef".into(),
                title: ready_title.clone(),
                ..item("", "")
            },
            readiness: Readiness::Ready,
            session_id: None,
        }];
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
            phase: "execute",
            item: Some(&current),
            plan_body: None,
            ready: &ready,
            review: None,
            manifest: &crate::manifest::LaunchManifest::default(),
        };
        let (text, missing) = assemble(&store, &Policy::default(), &facts);
        assert!(!text.contains(&ready_title), "ready work bypassed the cap");
        assert!(
            missing.iter().any(|m| m.what.contains("ready work")),
            "{missing:?}"
        );
    }

    /// An archived document is never included even if its pinned flag is
    /// still set from before it was archived — retiring a document retires
    /// it from orientation too, without the operator having to remember to
    /// unpin it first.
    #[test]
    fn a_pinned_but_archived_document_is_excluded() {
        let store = Store::open_in_memory().unwrap();
        store
            .write_change(
                "n",
                "personal",
                "document",
                ChangeOp::Upsert,
                "g",
                json!({
                "channel": "personal", "slug": "guide-retired", "kind": "guide", "title": "Retired",
                "body": "# Retired\n\nold guidance", "pinned": true, "archived": true,
                "hash": "h", "created_ms": 1, "updated_ms": 1}),
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
            phase: "execute",
            item: None,
            plan_body: None,
            ready: &[],
            review: None,
            manifest: &crate::manifest::LaunchManifest::default(),
        };
        let (text, missing) = assemble(&store, &Policy::default(), &facts);
        assert!(!text.contains("old guidance"), "{text}");
        assert!(!text.contains("## Pinned documents"), "{text}");
        assert!(missing.is_empty(), "{missing:?}");
    }

    /// The operator's own curation is the limit on a pinned document, not a
    /// byte cap: one many times the size of the whole orientation cap still
    /// goes in whole, uncut and unnamed as missing.
    #[test]
    fn a_very_large_pinned_document_appears_in_full_regardless_of_the_cap() {
        let store = Store::open_in_memory().unwrap();
        let long = "y".repeat(CAP_CHARS * 3);
        store
            .write_change(
                "n",
                "personal",
                "document",
                ChangeOp::Upsert,
                "g",
                json!({
                "channel": "personal", "slug": "guide-long", "kind": "guide", "title": "Long",
                "body": long, "pinned": true,
                "hash": "h", "created_ms": 1, "updated_ms": 1}),
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
            phase: "execute",
            item: None,
            plan_body: None,
            ready: &[],
            review: None,
            manifest: &crate::manifest::LaunchManifest::default(),
        };
        let (text, missing) = assemble(&store, &Policy::default(), &facts);
        assert!(text.len() > CAP_CHARS * 3, "the pinned document was cut short");
        assert!(!text.contains("[cut here"), "{text}");
        assert!(
            missing.iter().all(|m| !m.what.contains("guide-long")),
            "{missing:?}"
        );
    }

    /// A pinned document only takes effect when it is markdown: an HTML
    /// bundle is a document, but not text a session's orientation can carry.
    #[test]
    fn a_pinned_html_document_is_excluded() {
        let store = Store::open_in_memory().unwrap();
        store
            .write_change(
                "n",
                "personal",
                "document",
                ChangeOp::Upsert,
                "g",
                json!({
                "channel": "personal", "slug": "guide-html", "kind": "guide", "title": "HTML",
                "body": "<p>should not appear</p>", "pinned": true, "format": "html",
                "entry_path": "index.html", "source_name": "index.html",
                "hash": "h", "created_ms": 1, "updated_ms": 1}),
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
            phase: "execute",
            item: None,
            plan_body: None,
            ready: &[],
            review: None,
            manifest: &crate::manifest::LaunchManifest::default(),
        };
        let (text, _) = assemble(&store, &Policy::default(), &facts);
        assert!(!text.contains("should not appear"), "{text}");
        assert!(!text.contains("## Pinned documents"), "{text}");
    }

    /// Pinned documents are reserved: they are never capped or squeezed out
    /// by the discretionary tier, and their own size comes out of what is
    /// left for it, exactly as any other reserved content would.
    #[test]
    fn pinned_documents_are_never_capped_even_when_ready_work_is_squeezed() {
        let store = Store::open_in_memory().unwrap();
        pinned_docs(&store, 3, CAP_CHARS);
        let current = item("Current item", "");
        let ready_title = "r".repeat(500);
        let ready = [WorkView {
            item: WorkItem {
                id: "ready001deadbeef".into(),
                title: ready_title.clone(),
                ..item("", "")
            },
            readiness: Readiness::Ready,
            session_id: None,
        }];
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
            phase: "execute",
            item: Some(&current),
            plan_body: None,
            ready: &ready,
            review: None,
            manifest: &crate::manifest::LaunchManifest::default(),
        };
        let (text, missing) = assemble(&store, &Policy::default(), &facts);
        // All three pinned documents survive in full: none was capped,
        // trimmed, or named as missing.
        for i in 0..3 {
            assert!(text.contains(&format!("Guide {i}")), "{text}");
        }
        assert!(!text.contains("[cut here"), "{text}");
        assert!(missing.iter().all(|m| !m.what.contains("Guide")), "{missing:?}");
        // Ready work, genuinely discretionary, is what gets squeezed instead.
        assert!(!text.contains(&ready_title), "ready work should have been squeezed: too long");
        assert!(
            missing.iter().any(|m| m.what.contains("ready work")),
            "{missing:?}"
        );
    }

    /// An item's brief is named in the reserved tier, and only when there is
    /// one: a session whose item has no brief is told nothing about briefs.
    #[test]
    fn a_brief_is_pointed_at_from_the_task_and_is_silent_when_there_is_none() {
        let store = Store::open_in_memory().unwrap();
        let mut item = item("Overnight alert triage", "");
        let manifest = crate::manifest::LaunchManifest::default();
        fn facts<'a>(
            item: &'a WorkItem,
            manifest: &'a crate::manifest::LaunchManifest,
        ) -> Facts<'a> {
            Facts {
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
                phase: "execute",
                item: Some(item),
                plan_body: None,
                ready: &[],
                review: None,
                manifest,
            }
        }
        let (bare, _) = assemble(&store, &Policy::default(), &facts(&item, &manifest));
        assert!(!bare.contains("brief"), "no brief, nothing said: {bare}");

        item.brief_slug = Some("brief-item0001dead".into());
        let (text, missing) = assemble(&store, &Policy::default(), &facts(&item, &manifest));
        assert!(text.contains("`brief-item0001dead`"), "{text}");
        assert!(text.contains("`brief_read`"), "and how to read it: {text}");
        // Reading a brief is only half of it: a session that learns something
        // about the customer can put it back, and the node holds it to what it
        // can honestly claim.
        assert!(
            text.contains("`brief_note`"),
            "and how to add to it: {text}"
        );
        assert!(
            text.contains("approval"),
            "and that a line is asked: {text}"
        );
        assert!(
            text.find("Overnight alert triage") < text.find("brief-item0001dead"),
            "the pointer sits with the task"
        );
        assert!(missing.is_empty(), "a pointer costs nothing: {missing:?}");
    }

    /// An orientation that names a tool the node does not serve is worse than
    /// silence: the session spends a turn on a call that cannot exist and
    /// learns nothing from the failure. Every tool the work section names has
    /// to be one the MCP server actually registers.
    #[test]
    fn the_work_section_names_only_tools_the_node_serves() {
        let store = Store::open_in_memory().unwrap();
        let item = item("Carry the invoice rewrite", "Round half to even.");
        let manifest = crate::manifest::LaunchManifest::default();
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
            phase: "execute",
            item: Some(&item),
            plan_body: None,
            ready: &[],
            review: None,
            manifest: &manifest,
        };
        let (text, _) = assemble(&store, &Policy::shipped(), &facts);

        for tool in [
            crate::mcp::review::SUBMIT,
            crate::mcp::work::WORK_DISCOVER,
            crate::mcp::work::WORK_CLOSE,
        ] {
            assert!(
                text.contains(&format!("`{tool}`")),
                "{tool} not named:\n{text}"
            );
        }
        // `work_get` is a store call, not a tool. It was named here for a
        // release and could not be made.
        assert!(!text.contains("work_get"), "{text}");
        // And `submit` alone is not the tool's name.
        assert!(!text.contains("`submit`"), "{text}");

        // Closing is not what submitting does, and the roadmap moves work
        // further away from closing merely because a change was published.
        assert!(text.contains("Submitting is not closing"), "{text}");
    }

    /// A denial is answered in the task's own terms by `mcp::refusal`; the
    /// agreements section says so, so a refusal reads as an answer rather
    /// than as a broken tool worth routing around.
    #[test]
    fn the_agreements_say_a_refusal_explains_itself() {
        let store = Store::open_in_memory().unwrap();
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
            phase: "execute",
            item: None,
            plan_body: None,
            ready: &[],
            review: None,
            manifest: &crate::manifest::LaunchManifest::default(),
        };
        let (text, _) = assemble(&store, &Policy::shipped(), &facts);
        assert!(text.contains("## Working agreements"), "{text}");
        assert!(text.contains("refused by policy"), "{text}");
        assert!(
            text.find("## Working agreements") < text.find("refused by policy"),
            "{text}"
        );
    }
}
