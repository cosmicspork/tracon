//! Documents: long, named, read deliberately. `doc_search` finds by content,
//! `doc_read` fetches by slug, `doc_write` creates or edits — the last is not
//! named by the shipped policy, so it is asked.

use serde_json::{json, Value};

use crate::{
    config::DocKind,
    corpus,
    mcp::{CallContext, SessionAccess},
    store::{DocumentRow, DocumentWrite},
};

pub const DOC_READ: &str = "doc_read";
pub const DOC_SEARCH: &str = "doc_search";
pub const DOC_WRITE: &str = "doc_write";

/// A reserved kind: one the node's own behaviour hangs on, so it is built in
/// and no `[[docs.kind]]` entry removes or redefines it.
pub struct Reserved {
    pub kind: &'static str,
    pub label: &'static str,
    pub description: &'static str,
}

/// The reserved kinds. A work item's selected context (`corpus::context`)
/// and shown work (`show_work`) are refused to `doc_write`; a brief is what
/// the work tools read an item's criteria from; `plan-<item>`, written in a
/// plan session, is the plan artifact that ends the phase.
pub const RESERVED: &[Reserved] = &[
    Reserved {
        kind: corpus::brief::KIND,
        label: "Brief",
        description: "a work item's brief: what it is for and how it is judged",
    },
    Reserved {
        kind: corpus::context::KIND,
        label: "Context",
        description: "a work item's selected context, which only the operator changes",
    },
    Reserved {
        kind: PLAN,
        label: "Plan",
        description: "a plan; `plan-<item id>` written in a plan session is that item's plan",
    },
    Reserved {
        kind: SHOWN,
        label: "Shown work",
        description: "work shown to the operator, which only `show_work` writes",
    },
];

/// The kind of a plan, and of the plan artifact (`corpus::work::plan_slug`).
pub const PLAN: &str = "plan";

/// What a slug with no kind's prefix is filed under.
pub const OTHER: &str = "other";

/// Whether `slug` carries `prefix`: it is `prefix` or starts `prefix-`.
fn has_prefix(slug: &str, prefix: &str) -> bool {
    slug.strip_prefix(prefix)
        .is_some_and(|rest| rest.is_empty() || rest.starts_with('-'))
}

/// The reserved kind named `kind`, if it is one.
pub fn reserved(kind: &str) -> Option<&'static Reserved> {
    RESERVED.iter().find(|r| r.kind == kind)
}

/// The reserved kind `slug` is filed under, whatever the configuration.
pub fn reserved_kind_of(slug: &str) -> Option<&'static str> {
    RESERVED
        .iter()
        .find(|r| has_prefix(slug, r.kind))
        .map(|r| r.kind)
}

/// `<kind>-<slug>` → kind, by the reserved kinds and then `kinds`, the
/// node's configured ones (`[[docs.kind]]`); else `other`. Decided when a
/// document is read, not taken from the kind stored with it, so a kind the
/// operator renames or removes applies at once and peers agree.
pub fn kind_of<'a>(kinds: &'a [DocKind], slug: &str) -> &'a str {
    reserved_kind_of(slug)
        .or_else(|| {
            kinds
                .iter()
                .find(|k| has_prefix(slug, &k.prefix))
                .map(|k| k.prefix.as_str())
        })
        .unwrap_or(OTHER)
}

/// Whether `slug` is a working note: its kind is configured with
/// `working_note = true`. What a policy rule's `slug_working_note` matches.
pub fn is_working_note(kinds: &[DocKind], slug: &str) -> bool {
    reserved_kind_of(slug).is_none()
        && kinds
            .iter()
            .any(|k| k.working_note && has_prefix(slug, &k.prefix))
}

/// Every kind a document can be filed under, in the order the Documents
/// screen shows them: the configured ones as written, then the reserved,
/// then `other`. What `GET /api/docs/kinds` answers and `doc_search`'s
/// `kind` takes.
pub fn kind_list(kinds: &[DocKind]) -> Vec<Value> {
    let configured = kinds.iter().map(|k| {
        json!({
            "kind": k.prefix,
            "label": if k.label.is_empty() { &k.prefix } else { &k.label },
            "description": k.description,
            "working_note": k.working_note,
            "reserved": false,
        })
    });
    let reserved = RESERVED.iter().map(|r| {
        json!({
            "kind": r.kind, "label": r.label, "description": r.description,
            "working_note": false, "reserved": true,
        })
    });
    let other = json!({
        "kind": OTHER, "label": "Other",
        "description": "a slug with no kind's prefix",
        "working_note": false, "reserved": true,
    });
    configured
        .chain(reserved)
        .chain(std::iter::once(other))
        .collect()
}

/// What a search's `kind` filter admits: a document whose slug classifies
/// as that kind under the configured list.
#[derive(Clone, Copy)]
pub struct KindFilter<'a> {
    pub kinds: &'a [DocKind],
    pub kind: &'a str,
}

impl KindFilter<'_> {
    pub fn admits(&self, slug: &str) -> bool {
        kind_of(self.kinds, slug) == self.kind
    }
}

/// The first `# ` heading, else the slug de-prefixed and de-hyphenated.
pub fn title_of(kinds: &[DocKind], slug: &str, body: &str) -> String {
    body.lines()
        .find_map(|l| l.strip_prefix("# ").map(|t| t.trim().to_string()))
        .filter(|t| !t.is_empty())
        .unwrap_or_else(|| {
            let kind = kind_of(kinds, slug);
            let rest = slug.strip_prefix(&format!("{kind}-")).unwrap_or(slug);
            rest.replace('-', " ")
        })
}

/// A work item's selected context is the operator's choice of what every
/// attempt at the item starts with (`corpus::context`). A session writing it
/// would be choosing its own context — the substitution the selection exists
/// to prevent — so `doc_write` refuses the slug outright, before the policy
/// gate would put it to the operator as an ordinary document edit.
///
/// Shown work is reserved too: only `show_work` writes it, bound to the
/// commit it was shown at, and an edit through `doc_write` would be an account
/// nobody showed at any commit.
pub fn reserved_for_operator(args: &Value) -> Option<String> {
    let slug = args["slug"].as_str().unwrap_or("").trim();
    match reserved_kind_of(slug)? {
        corpus::context::KIND => Some(format!(
            "`{slug}` is a work item's selected context, which only the operator changes. \
             Say which document belongs in it, and why, with `ask_operator`."
        )),
        SHOWN => Some(format!(
            "`{slug}` is work shown to the operator, which only `show_work` writes. Show the \
             work again with `show_work`."
        )),
        _ => None,
    }
}

/// The kind of the documents `show_work` stores its pages and files in.
pub const SHOWN: &str = "shown";

pub fn valid_slug(slug: &str) -> bool {
    !slug.is_empty()
        && slug.len() <= 120
        && slug.bytes().all(|b| {
            b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'.' || b == b'_'
        })
        && !slug.starts_with('.')
}

/// The tools, their kinds generated from `kinds`, the node's configured
/// list, so what an agent is told it may write and search by is what the
/// node classifies by.
pub fn definitions(kinds: &[DocKind]) -> Vec<Value> {
    let every: Vec<String> = kind_list(kinds)
        .into_iter()
        .filter_map(|k| k["kind"].as_str().map(str::to_string))
        .collect();
    // What `doc_write` may write: every configured kind, and the plan. A
    // brief is the work tools' to write, and context and shown work are
    // refused (`reserved_for_operator`).
    let writable: Vec<String> = kinds
        .iter()
        .map(|k| match k.description.is_empty() {
            true => format!("`{}`", k.prefix),
            false => format!("`{}` ({})", k.prefix, k.description),
        })
        .chain(reserved(PLAN).map(|r| format!("`{}` ({})", r.kind, r.description)))
        .collect();
    let write_description = format!(
        "Create or replace a document on this channel. Slugs are `<kind>-<name>` with kind one \
         of {}; any other prefix is filed as `{OTHER}`. Pass if_hash (from doc_read) to refuse \
         overwriting an edit you have not seen. The operator is asked before this runs, unless \
         the policy writes it unattended (working notes): an asked call returns \
         `awaiting_operator` with an `approval_id` at once, and approval_status reports the \
         outcome.",
        writable.join(", ")
    );
    vec![
        json!({
            "name": DOC_SEARCH,
            "description": "Find documents by content. Returns slugs with a snippet; call doc_read for the whole document.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": { "type": "string" },
                    "kind": { "type": "string", "enum": every },
                    "limit": { "type": "integer", "default": 8 },
                },
                "required": ["query"],
            },
        }),
        json!({
            "name": DOC_READ,
            "description": "Read a document by slug (for example `guide-workspace`).",
            "inputSchema": {
                "type": "object",
                "properties": { "slug": { "type": "string" } },
                "required": ["slug"],
            },
        }),
        json!({
            "name": DOC_WRITE,
            "description": write_description,
            "inputSchema": {
                "type": "object",
                "properties": {
                    "slug": { "type": "string" },
                    "body": { "type": "string" },
                    "if_hash": { "type": "string" },
                },
                "required": ["slug", "body"],
            },
        }),
    ]
}

pub async fn call(
    tools: &super::Tools,
    access: &SessionAccess,
    ctx: &CallContext,
    name: &str,
    args: &Value,
) -> Result<Value, String> {
    match name {
        DOC_SEARCH => {
            let query = args["query"].as_str().unwrap_or("").trim();
            let limit = args["limit"].as_u64().unwrap_or(8).clamp(1, 50) as usize;
            let near = crate::embed::neighbours(
                &tools.cfg,
                &access.store,
                &tools.http,
                access.manager.probe_token(),
                Some(&ctx.channel),
                query,
                limit,
            )
            .await;
            let live = tools.cfg.live();
            let kind = args["kind"].as_str().map(|kind| KindFilter {
                kinds: &live.docs.kind,
                kind,
            });
            let hits = access
                .store
                .doc_search_hybrid(Some(&ctx.channel), kind, query, limit, &near.hits)
                .map_err(|e| e.to_string())?;
            Ok(json!({ "hits": hits }))
        }
        DOC_READ => {
            let slug = args["slug"].as_str().unwrap_or("").trim();
            let doc = access
                .store
                .doc_get(&ctx.channel, slug)
                .map_err(|e| e.to_string())?
                .ok_or_else(|| format!("no document {slug} on channel {}", ctx.channel))?;
            let kind = kind_of(&tools.cfg.live().docs.kind, &doc.slug).to_string();
            Ok(json!({
                "slug": doc.slug, "kind": kind, "title": doc.title, "hash": doc.hash,
                "format": doc.format, "entry_path": doc.entry_path, "source_name": doc.source_name,
                "archived": doc.archived != 0, "body": doc.body,
            }))
        }
        DOC_WRITE => {
            let slug = args["slug"].as_str().unwrap_or("").trim();
            let body = args["body"].as_str().unwrap_or("");
            let node_id = access.manager.node_id().to_string();
            let kinds = tools.cfg.live().docs.kind.clone();
            let doc = write_document(
                &access.store,
                access.manager.bus(),
                &kinds,
                &node_id,
                &ctx.channel,
                slug,
                body,
                args["if_hash"].as_str(),
                false,
                None,
                None,
            )
            .map_err(|e| e.to_string())?;
            // The plan artifact: record it on the item and end the plan
            // session once this turn is over.
            let session = ctx
                .session_id()
                .and_then(|id| access.store.get_session(id).ok().flatten());
            if let Some((session_id, item)) = session
                .filter(|s| s.phase == "plan")
                .and_then(|s| s.work_item_id.clone().map(|item| (s.id, item)))
                .filter(|(_, item)| corpus::work::plan_slug(item) == slug)
            {
                corpus::work::set_plan(&access.store, access.manager.bus(), &node_id, &item, slug)
                    .map_err(|e| e.to_string())?;
                access.manager.record_event(
                    &session_id,
                    crate::session::state::event_kind::PLAN_ARTIFACT,
                    json!({ "slug": slug, "hash": doc.hash, "work_item_id": item }),
                );
                access.manager.phase_done(&session_id).await;
            }
            Ok(json!({ "slug": doc.slug, "hash": doc.hash }))
        }
        other => Err(format!("no tool named {other}")),
    }
}

#[derive(Debug, thiserror::Error)]
pub enum WriteError {
    #[error("slug {0:?} is not lowercase letters, digits, dots, dashes, and underscores")]
    Slug(String),
    #[error("the document changed since it was read; its hash is now {hash}")]
    Conflict { hash: String, body: String },
    #[error("HTML documents must be replaced through import")]
    HtmlDocument,
    #[error(transparent)]
    Store(#[from] crate::store::StoreError),
}

/// Create or replace a document at a slug, on this node, as a sync write.
/// `kinds` is the node's configured list: the kind stored with the row is a
/// cache of what it classified as when written, never what it is read as.
/// `if_hash` is the edit's precondition: the hash the caller last read.
/// `create_only` is the HTTP `If-None-Match: *` precondition. `archived` and
/// `pinned` of `None` leave the document as it was.
#[allow(clippy::too_many_arguments)]
pub fn write_document(
    store: &crate::store::Store,
    bus: &crate::stream::Bus,
    kinds: &[DocKind],
    site: &str,
    channel: &str,
    slug: &str,
    body: &str,
    if_hash: Option<&str>,
    create_only: bool,
    archived: Option<bool>,
    pinned: Option<bool>,
) -> Result<DocumentRow, WriteError> {
    if !valid_slug(slug) {
        return Err(WriteError::Slug(slug.to_string()));
    }
    if store
        .doc_get(channel, slug)?
        .is_some_and(|document| document.format == "html")
    {
        return Err(WriteError::HtmlDocument);
    }
    let hash = corpus::hash_body(body);
    match store.write_document_change(
        site,
        channel,
        slug,
        kind_of(kinds, slug),
        &title_of(kinds, slug, body),
        body,
        &hash,
        if_hash,
        create_only,
        &corpus::new_id(),
        archived,
        pinned,
    )? {
        DocumentWrite::Written { row, change } => {
            let row = *row;
            bus.publish(crate::stream::Frame::Changes {
                channel: channel.to_string(),
                changes: vec![change],
            });
            Ok(row)
        }
        DocumentWrite::Conflict { hash, body } => Err(WriteError::Conflict { hash, body }),
        DocumentWrite::HtmlDocument => Err(WriteError::HtmlDocument),
    }
}

/// Remove a document at a slug (a tombstone that travels).
pub fn delete_document(
    store: &crate::store::Store,
    bus: &crate::stream::Bus,
    site: &str,
    channel: &str,
    slug: &str,
) -> Result<bool, crate::store::StoreError> {
    let Some(changes) = store.delete_document_change(site, channel, slug)? else {
        return Ok(false);
    };
    bus.publish(crate::stream::Frame::Changes {
        channel: channel.to_string(),
        changes,
    });
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Barrier};

    /// `pinned`, like `archived`, `kind`, and `title`, is operator-only: an
    /// agent calling `doc_write` must never be able to set it.
    #[test]
    fn doc_write_schema_has_no_operator_only_fields() {
        let defs = definitions(&crate::config::default_doc_kinds());
        let write = defs
            .iter()
            .find(|d| d["name"] == DOC_WRITE)
            .expect("doc_write is defined");
        let props = write["inputSchema"]["properties"]
            .as_object()
            .expect("doc_write has properties");
        assert_eq!(
            props
                .keys()
                .map(String::as_str)
                .collect::<std::collections::BTreeSet<_>>(),
            ["slug", "body", "if_hash"].into_iter().collect(),
            "{props:?}"
        );
        assert!(!props.contains_key("pinned"), "{props:?}");
        assert!(!props.contains_key("archived"), "{props:?}");
    }

    fn runbook() -> DocKind {
        DocKind {
            prefix: "runbook".into(),
            label: "Runbook".into(),
            description: "how to run something".into(),
            working_note: false,
        }
    }

    #[test]
    fn kinds_and_titles_follow_the_prefix_scheme() {
        let kinds = crate::config::default_doc_kinds();
        assert_eq!(kind_of(&kinds, "guide-workspace"), "guide");
        assert_eq!(kind_of(&kinds, "ref"), "ref");
        assert_eq!(kind_of(&kinds, "refx-y"), "other");
        assert_eq!(kind_of(&kinds, "todo"), "other");
        assert_eq!(
            title_of(&kinds, "plan-hdr-todo", "# HDR todo\n\nbody"),
            "HDR todo"
        );
        assert_eq!(title_of(&kinds, "plan-hdr-todo", "no heading"), "hdr todo");
        assert!(valid_slug("guide-work_space.v2"));
        assert!(!valid_slug("Guide"));
        assert!(!valid_slug("../x"));
        assert!(!valid_slug(""));
    }

    #[test]
    fn a_configured_kind_classifies_and_a_removed_one_reads_as_other() {
        let mut kinds = crate::config::default_doc_kinds();
        assert_eq!(kind_of(&kinds, "runbook-deploy"), OTHER);
        kinds.push(runbook());
        kinds.retain(|k| k.prefix != "meeting");
        assert_eq!(kind_of(&kinds, "runbook-deploy"), "runbook");
        assert_eq!(kind_of(&kinds, "meeting-weekly"), OTHER);
        // The reserved kinds hold whatever the list says.
        for (slug, kind) in [
            ("context-0192f3a17c4e", "context"),
            ("shown-x", "shown"),
            ("brief-x", "brief"),
            ("plan-x", "plan"),
        ] {
            assert_eq!(kind_of(&[], slug), kind, "{slug}");
        }
        assert!(is_working_note(&kinds, "note-x"));
        assert!(!is_working_note(&kinds, "runbook-x"));
        assert!(!is_working_note(&kinds, "meeting-x"));
    }

    #[test]
    fn the_tools_offer_the_configured_kinds_and_nothing_else() {
        let mut kinds = crate::config::default_doc_kinds();
        kinds.push(runbook());
        kinds.retain(|k| k.prefix != "meeting");
        let defs = definitions(&kinds);
        let def = |name: &str| defs.iter().find(|d| d["name"] == name).unwrap().clone();
        let search = def(DOC_SEARCH);
        let offered: Vec<&str> = search["inputSchema"]["properties"]["kind"]["enum"]
            .as_array()
            .unwrap()
            .iter()
            .map(|k| k.as_str().unwrap())
            .collect();
        let mut want: Vec<&str> = kinds.iter().map(|k| k.prefix.as_str()).collect();
        want.extend(RESERVED.iter().map(|r| r.kind));
        want.push(OTHER);
        assert_eq!(offered, want);
        let write = def(DOC_WRITE)["description"].as_str().unwrap().to_string();
        assert!(
            write.contains("`runbook` (how to run something)"),
            "{write}"
        );
        assert!(write.contains("`plan`"), "{write}");
        assert!(!write.contains("`meeting`"), "{write}");
        // Refused to doc_write, so not offered by it.
        assert!(
            !write.contains("`context`") && !write.contains("`shown`"),
            "{write}"
        );
    }

    #[test]
    fn matching_hash_is_checked_in_the_same_transaction_as_the_write() {
        let store = Arc::new(crate::store::Store::open_in_memory().unwrap());
        let bus = Arc::new(crate::stream::Bus::new());
        let original = write_document(
            &store,
            &bus,
            &[],
            "A",
            "personal",
            "guide-race",
            "# Original",
            None,
            true,
            None,
            None,
        )
        .unwrap();
        assert!(matches!(
            write_document(
                &store,
                &bus,
                &[],
                "A",
                "personal",
                "guide-race",
                "# Blind overwrite",
                None,
                true,
                None,
                None,
            ),
            Err(WriteError::Conflict { .. })
        ));
        let barrier = Arc::new(Barrier::new(3));
        let mut joins = Vec::new();
        for body in ["# First", "# Second"] {
            let store = store.clone();
            let bus = bus.clone();
            let barrier = barrier.clone();
            let hash = original.hash.clone();
            joins.push(std::thread::spawn(move || {
                barrier.wait();
                write_document(
                    &store,
                    &bus,
                    &[],
                    "A",
                    "personal",
                    "guide-race",
                    body,
                    Some(&hash),
                    false,
                    None,
                    None,
                )
            }));
        }
        barrier.wait();
        let results: Vec<_> = joins.into_iter().map(|j| j.join().unwrap()).collect();
        assert_eq!(results.iter().filter(|r| r.is_ok()).count(), 1);
        assert_eq!(
            results
                .iter()
                .filter(|r| matches!(r, Err(WriteError::Conflict { .. })))
                .count(),
            1
        );
    }
}
