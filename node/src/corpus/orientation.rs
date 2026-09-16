//! What a session is told before its first prompt: assembled on the node,
//! never a file in the worktree. Two tiers, and the order between them is the
//! point.
//!
//! **Reserved** is what this session is for and what bounds it: the node it
//! runs on, the work item and its phase, the plan or the diff under review,
//! the agreements the node enforces, and the operator's standing directives
//! and customization. It is assembled first and is never dropped to make room
//! for anything below it.
//!
//! **Discretionary** is everything the node offers because it might help:
//! shared conventions (documents of kind `guide` on the channel) and the other
//! ready work on the project. It gets what is left of the cap, shortest first.
//!
//! The cap exists because oversized context degrades the session it was meant
//! to help. It used to be enforced by truncating the assembled text, which cut
//! from the end — where the task, the constraints and the directives lived. A
//! session told the conventions but not the job is worse off than one told
//! neither, so the cap now binds the discretionary tier and nothing else.
//!
//! Whatever does not fit is *named*: which guide, how much of it is missing,
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
/// A single guide document may take at most this much of what is left.
const GUIDE_CHARS: usize = 8_000;
/// A diff longer than this is cut; the reviewer has the worktree and git.
const DIFF_CHARS: usize = 12_000;
/// Each reserved piece is bounded too, so that nothing inside the reserved
/// tier can starve anything else inside it.
const BODY_CHARS: usize = 8_000;
/// The proposed change's own description, for a review session.
const REQUIREMENTS_CHARS: usize = 4_000;
/// Directives and recalled facts.
const KNOWN_CHARS: usize = 4_000;
/// The channel's launch manifest, rendered.
const CUSTOMIZATION_CHARS: usize = 6_000;
/// Held back from the cap so that naming what was left out cannot itself be
/// the thing that does not fit.
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
    out.push_str("# Orientation\n\nAssembled by the tracon node for this session; nothing here is in the repository.\n\n");
    push_node(&mut out, facts);
    push_work(&mut out, facts, &mut missing);
    push_review(&mut out, facts, &mut missing);
    push_agreements(&mut out, policy);
    push_known(&mut out, store, facts, &mut missing);
    push_customization(&mut out, facts, &mut missing);

    // Discretionary. Whatever the reserved tier left under the cap, minus the
    // room held back to name what does not fit.
    let left = CAP_CHARS.saturating_sub(out.len() + NOTICE_CHARS);
    push_conventions(&mut out, store, facts, left, &mut missing);
    push_ready(&mut out, facts);

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
            Some("call `work_get` for the whole item"),
            missing,
        );
        out.push_str("\n\n");
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
                "This is an **execute session**: do the work in the worktree, then `submit` \
                 for review. Work you find but should not do now: `work_discover`. When the \
                 item is done and submitted, `work_close` ends this session.\n\n",
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
    out.push('\n');
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
    for m in &known {
        let tag = if m.kind == "directive" {
            "directive"
        } else {
            "fact"
        };
        let line = format!("- ({tag}) {}\n", m.body.trim());
        // A directive is the operator speaking. Whole ones, never a
        // half-sentence that reverses what it was asking for.
        if used + line.len() > KNOWN_CHARS && used > 0 {
            dropped += 1;
            continue;
        }
        used += line.len();
        out.push_str(&line);
    }
    if dropped > 0 {
        missing.push(Missing {
            what: format!("{dropped} more directives and facts"),
            partial: true,
            chars: 0,
            fetch: Some("call `recall` for them".into()),
        });
    }
    out.push_str("\nCall `recall` for more; `retain` what you learn.\n\n");
}

/// What the operator customized this channel's launches with. Reserved: it is
/// the operator's standing text for every session here, not something the
/// node offers because it might help.
fn push_customization(out: &mut String, facts: &Facts, missing: &mut Vec<Missing>) {
    let text = facts.manifest.orientation();
    if text.is_empty() {
        return;
    }
    push_capped(
        out,
        &text,
        CUSTOMIZATION_CHARS,
        "the launch manifest's instructions",
        Some("ask the operator; the manifest is not readable from inside the session"),
        missing,
    );
    out.push_str("\n\n");
}

/// Shared conventions, shortest first so a long one cannot crowd out the
/// rest, and only as far as the cap allows. What does not fit is named.
fn push_conventions(
    out: &mut String,
    store: &Store,
    facts: &Facts,
    mut left: usize,
    missing: &mut Vec<Missing>,
) {
    let mut guides: Vec<_> = store
        .doc_list(Some(facts.channel))
        .unwrap_or_default()
        .into_iter()
        .filter(|d| d.kind == "guide" && d.format == "markdown")
        .filter_map(|d| store.doc_by_id(&d.id).ok().flatten())
        .collect();
    if guides.is_empty() {
        return;
    }
    guides.sort_by_key(|d| d.body.len());
    let mut opened = false;
    for g in guides {
        let body = g.body.trim();
        let what = format!("guide \"{}\" (`{}`)", g.title, g.slug);
        let fetch = format!("call `doc_read` for `{}`", g.slug);
        let heading = format!("### {} (`{}`)\n\n", g.title, g.slug);
        let header = if opened {
            0
        } else {
            "## Conventions\n\n".len()
        };
        // Room for the heading and a usable amount of the document. Below
        // that there is nothing to say that naming it does not say better.
        let room = left.saturating_sub(header + heading.len() + 200);
        if room == 0 {
            missing.push(Missing {
                what,
                partial: false,
                chars: body.len(),
                fetch: Some(fetch),
            });
            continue;
        }
        if !opened {
            out.push_str("## Conventions\n\n");
            left -= header;
            opened = true;
        }
        out.push_str(&heading);
        let before = out.len();
        push_capped(
            out,
            body,
            room.min(GUIDE_CHARS),
            &what,
            Some(&fetch),
            missing,
        );
        out.push_str("\n\n");
        left = left.saturating_sub(out.len() - before + heading.len());
    }
}

/// Background: what else is ready on this project, for `work_discover` deps.
fn push_ready(out: &mut String, facts: &Facts) {
    if facts.item.is_none() || facts.ready.is_empty() {
        return;
    }
    out.push_str("## Ready work on this project\n\n");
    for v in facts.ready.iter().take(10) {
        out.push_str(&format!(
            "- `{}…` {}\n",
            &v.item.id[..8.min(v.item.id.len())],
            v.item.title
        ));
    }
    out.push('\n');
}

/// Name what did not fit. The session is owed this: it cannot ask for a
/// document it does not know was withheld.
fn push_notice(out: &mut String, missing: &[Missing]) {
    if missing.is_empty() {
        return;
    }
    out.push_str(
        "## Not in this orientation\n\nThe node's context cap kept the following out, not \
         policy. Ask for any of it you need:\n\n",
    );
    let mut shown = 0;
    let mut room = NOTICE_CHARS.saturating_sub(200);
    for m in missing {
        let line = m.line();
        if line.len() > room {
            break;
        }
        room -= line.len();
        out.push_str(&line);
        shown += 1;
    }
    if shown < missing.len() {
        out.push_str(&format!("- and {} more\n", missing.len() - shown));
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
            .write_change(
                "n",
                "personal",
                "document",
                ChangeOp::Upsert,
                "html-guide",
                json!({
                "channel": "personal", "slug": "guide-html", "kind": "guide", "title": "HTML",
                "body": "<p>HTML must not become orientation</p>", "hash": "html", "format": "html",
                "entry_path": "index.html", "source_name": "index.html",
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
        assert!(i("## Conventions") < i("Conventional commits"));
        assert!(!text.contains("not a guide"));
        assert!(!text.contains("HTML must not become orientation"));
        assert!(i("## This node") < i("## Working agreements"));
        // Conventions are discretionary, so they come after the agreements
        // and the operator's directives, not before them.
        assert!(i("## Known") < i("## Conventions"));
        assert!(i("no-merge") < i("## Known"));
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
            closed_by_session: None,
            created_ms: 1,
            updated_ms: 1,
        }
    }

    fn guides(store: &Store, n: usize, chars: usize) {
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
                    "title": format!("Guide {i}"), "body": "y".repeat(chars),
                    "hash": format!("h{i}"), "created_ms": 1, "updated_ms": 1}),
                )
                .unwrap();
        }
    }

    /// The defect this reserved tier exists for. Four guides, each twice the
    /// per-guide cap, used to fill the whole orientation and push the task,
    /// the plan, the agreements and the operator's directives off the end —
    /// leaving the session a wall of conventions and a bare "trimmed" flag.
    #[test]
    fn long_guides_never_crowd_out_the_task_its_constraints_or_the_directives() {
        let store = Store::open_in_memory().unwrap();
        guides(&store, 4, GUIDE_CHARS * 2);
        for (i, body) in ["never touch the production database", "deploy on Thursdays"]
            .iter()
            .enumerate()
        {
            store
                .write_change("n", "personal", "memory", ChangeOp::Upsert, &format!("m{i}"), json!({
                    "channel": "personal", "scope": "global", "scope_ref": null, "kind": "directive",
                    "body": body, "source_session": null, "source_node": null, "confidence": 1.0,
                    "state": "active", "created_ms": 1, "updated_ms": 1}))
                .unwrap();
        }
        let item = item("Fix the billing rounding error", "Round half to even.");
        let facts = Facts {
            node_name: "laptop",
            node_id: "0123456789abcdef",
            backend: "podman",
            harness: "opencode",
            harness_version: "18.0.4",
            channel: "personal",
            project_id: None,
            project_name: None,
            tools: &["recall".into()],
            worktree: "/work",
            phase: "execute",
            item: Some(&item),
            plan_body: Some("Essential constraint: do not change the public API."),
            ready: &[],
            review: None,
            manifest: &crate::manifest::LaunchManifest::default(),
        };
        let (text, missing) = assemble(&store, &Policy::shipped(), &facts);

        // The task, the plan's constraint, the node's agreements and both
        // operator directives all survive four guides that together are five
        // times the cap.
        assert!(text.contains("Fix the billing rounding error"), "{text}");
        assert!(text.contains("Round half to even."));
        assert!(text.contains("do not change the public API"));
        assert!(text.contains("## Working agreements"));
        assert!(text.contains("no-merge"));
        assert!(text.contains("(directive) never touch the production database"));
        assert!(text.contains("(directive) deploy on Thursdays"));
        assert!(text.contains("Your worktree is `/work`"));

        // And the omission is named, guide by guide, not flagged.
        assert!(text.contains("## Not in this orientation"));
        assert!(missing.len() >= 3, "{missing:?}");
        for m in &missing {
            assert!(m.what.contains("guide-"), "{m:?}");
            assert!(m.chars > 0, "{m:?}");
            assert!(m.fetch.as_deref().unwrap().contains("doc_read"), "{m:?}");
            // A generic flag is not sufficient: the slug has to be in the
            // text the session actually reads, not only in the event.
            assert!(text.contains(&m.what), "{} not named in:\n{text}", m.what);
        }
        // Whole guides that never fit are named as absent, not as cut short.
        assert!(missing.iter().any(|m| !m.partial), "{missing:?}");
    }

    /// The cap binds the guides, not the work. An execute session with a
    /// large item and a large plan keeps both, and simply gets fewer guides.
    #[test]
    fn the_cap_is_spent_on_the_work_before_the_conventions() {
        let store = Store::open_in_memory().unwrap();
        guides(&store, 6, 3_000);
        let item = item("Carry the invoice rewrite", &"w".repeat(BODY_CHARS));
        let plan = "p".repeat(BODY_CHARS);
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
            plan_body: Some(&plan),
            ready: &[],
            review: None,
            manifest: &crate::manifest::LaunchManifest::default(),
        };
        let (text, missing) = assemble(&store, &Policy::shipped(), &facts);
        assert!(text.contains(&"w".repeat(BODY_CHARS)), "item body was cut");
        assert!(text.contains(&"p".repeat(BODY_CHARS)), "plan was cut");
        assert!(!missing.is_empty(), "the squeezed guides should be named");
        // Some guides still fit; the cap took it out of the conventions.
        assert!(text.contains("## Conventions"));
        assert!(missing.len() < 6, "{missing:?}");
    }

    /// A review session keeps the diff and the requirements it is there to
    /// judge, whatever the channel's guides weigh.
    #[test]
    fn a_review_session_keeps_its_diff_over_the_conventions() {
        let store = Store::open_in_memory().unwrap();
        guides(&store, 3, GUIDE_CHARS);
        let review = ReviewRow {
            id: "r1".into(),
            session_id: "s1".into(),
            node_id: "n".into(),
            channel: "personal".into(),
            kind: "code".into(),
            title: "Rewrite the invoice totals".into(),
            body: "Totals must round half to even.".into(),
            edited_title: None,
            edited_body: None,
            provider: "github".into(),
            target: "t".into(),
            diff: format!("--- a/x\n+++ b/x\n{}", "+line\n".repeat(400)),
            files: "[]".into(),
            head_sha: "abc".into(),
            base_ref: "main".into(),
            added: 400,
            removed: 0,
            state: "new".into(),
            verdict_reason: None,
            publish_result: None,
            claimed_ms: None,
            created_ms: 1,
            created_mono_ms: 1,
            resolved_mono_ms: None,
            updated_ms: 1,
            checks_json: None,
            review_session_id: None,
            ai_verdict_json: None,
            revision_patch: None,
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
            phase: "review",
            item: None,
            plan_body: None,
            ready: &[],
            review: Some(&review),
            manifest: &crate::manifest::LaunchManifest::default(),
        };
        let (text, missing) = assemble(&store, &Policy::shipped(), &facts);
        assert!(text.contains("Totals must round half to even."));
        assert!(text.contains("Rewrite the invoice totals"));
        assert!(text.contains("```diff"));
        assert!(text.matches("+line").count() > 300, "the diff was starved");
        // The diff gets its full allowance first; the guides get the rest,
        // and are named when there is not enough of it to go round.
        assert!(text.find("## Review").unwrap() < text.find("## Conventions").unwrap());
        assert!(!missing.is_empty(), "the squeezed guides should be named");
    }

    /// The operator's launch manifest is their standing text for every
    /// session on the channel, so it is reserved beside the directives.
    #[test]
    fn the_operators_customization_outranks_the_channels_guides() {
        let store = Store::open_in_memory().unwrap();
        guides(&store, 4, GUIDE_CHARS * 2);
        let manifest = crate::manifest::LaunchManifest {
            revision: 3,
            digest: "deadbeefcafe".into(),
            instructions: vec![crate::manifest::TextEntry {
                name: "house-style".into(),
                body: "Always write the test before the fix.".into(),
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
        assert!(
            text.contains("Always write the test before the fix."),
            "{text}"
        );
        assert!(!missing.is_empty());
        assert!(text.find("## Customization") < text.find("## Conventions"));
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
            phase: "execute",
            item: None,
            plan_body: None,
            ready: &[],
            review: None,
            manifest: &crate::manifest::LaunchManifest::default(),
        };
        let (text, missing) = assemble(&store, &Policy::default(), &facts);
        assert_eq!(missing.len(), 1, "{missing:?}");
        assert!(missing[0].partial);
        assert!(missing[0].what.contains("guide-long"), "{missing:?}");
        assert!(text.contains("[cut here"));
        assert!(text.contains("call `doc_read` for `guide-long`"));
        assert!(text.len() < CAP_CHARS);
    }
}
