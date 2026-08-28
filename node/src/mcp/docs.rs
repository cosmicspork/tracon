//! Documents: long, named, read deliberately. `doc_search` finds by content,
//! `doc_read` fetches by slug, `doc_write` creates or edits — the last is not
//! named by the shipped policy, so it is asked.

use serde_json::{json, Value};
use tracon_sync::ChangeOp;

use crate::{
    corpus,
    mcp::{CallContext, SessionAccess},
    store::{now_ms, DocumentRow},
};

pub const DOC_READ: &str = "doc_read";
pub const DOC_SEARCH: &str = "doc_search";
pub const DOC_WRITE: &str = "doc_write";

/// The notebook prefixes, kept as the document's kind.
pub const KINDS: &[&str] = &[
    "note",
    "repo",
    "meeting",
    "inbox",
    "proposal",
    "plan",
    "guide",
    "ref",
    "architecture",
];

/// `<kind>-<slug>` → kind, else `other`.
pub fn kind_of(slug: &str) -> &'static str {
    KINDS
        .iter()
        .find(|k| slug == **k || slug.starts_with(&format!("{k}-")))
        .copied()
        .unwrap_or("other")
}

/// The first `# ` heading, else the slug de-prefixed and de-hyphenated.
pub fn title_of(slug: &str, body: &str) -> String {
    body.lines()
        .find_map(|l| l.strip_prefix("# ").map(|t| t.trim().to_string()))
        .filter(|t| !t.is_empty())
        .unwrap_or_else(|| {
            let kind = kind_of(slug);
            let rest = slug.strip_prefix(&format!("{kind}-")).unwrap_or(slug);
            rest.replace('-', " ")
        })
}

pub fn valid_slug(slug: &str) -> bool {
    !slug.is_empty()
        && slug.len() <= 120
        && slug.bytes().all(|b| {
            b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'.' || b == b'_'
        })
        && !slug.starts_with('.')
}

pub fn definitions() -> Vec<Value> {
    vec![
        json!({
            "name": DOC_SEARCH,
            "description": "Find documents by content. Returns slugs with a snippet; call doc_read for the whole document.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": { "type": "string" },
                    "kind": { "type": "string", "enum": KINDS },
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
            "description": "Create or replace a document on this channel. Slugs are `<kind>-<name>` with \
                            kind one of note, repo, meeting, inbox, proposal, plan, guide, ref, \
                            architecture. Pass if_hash (from doc_read) to refuse overwriting an edit \
                            you have not seen. The operator is asked before this runs.",
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
    access: &SessionAccess,
    ctx: &CallContext,
    name: &str,
    args: &Value,
) -> Result<Value, String> {
    match name {
        DOC_SEARCH => {
            let query = args["query"].as_str().unwrap_or("").trim();
            let limit = args["limit"].as_u64().unwrap_or(8).clamp(1, 50) as usize;
            let hits = access
                .store
                .doc_search(Some(&ctx.channel), args["kind"].as_str(), query, limit)
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
            Ok(
                json!({ "slug": doc.slug, "kind": doc.kind, "title": doc.title, "hash": doc.hash, "body": doc.body }),
            )
        }
        DOC_WRITE => {
            let slug = args["slug"].as_str().unwrap_or("").trim();
            let body = args["body"].as_str().unwrap_or("");
            let node_id = access.manager.node_id().to_string();
            let doc = write_document(
                &access.store,
                access.manager.bus(),
                &node_id,
                &ctx.channel,
                slug,
                body,
                args["if_hash"].as_str(),
            )
            .map_err(|e| e.to_string())?;
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
    #[error(transparent)]
    Store(#[from] crate::store::StoreError),
}

/// Create or replace a document at a slug, on this node, as a sync write.
/// `if_hash` is the edit's precondition: the hash the caller last read.
pub fn write_document(
    store: &crate::store::Store,
    bus: &crate::stream::Bus,
    site: &str,
    channel: &str,
    slug: &str,
    body: &str,
    if_hash: Option<&str>,
) -> Result<DocumentRow, WriteError> {
    if !valid_slug(slug) {
        return Err(WriteError::Slug(slug.to_string()));
    }
    let existing = store.doc_get(channel, slug)?;
    if let (Some(want), Some(cur)) = (if_hash, &existing) {
        if want != cur.hash {
            return Err(WriteError::Conflict {
                hash: cur.hash.clone(),
                body: cur.body.clone(),
            });
        }
    }
    let now = now_ms();
    let hash = corpus::hash_body(body);
    let row = DocumentRow {
        id: existing
            .as_ref()
            .map(|d| d.id.clone())
            .unwrap_or_else(corpus::new_id),
        channel: channel.to_string(),
        slug: slug.to_string(),
        kind: kind_of(slug).to_string(),
        title: title_of(slug, body),
        body: body.to_string(),
        hash,
        site: site.to_string(),
        hlc_ms: 0,
        deleted: 0,
        created_ms: existing.as_ref().map(|d| d.created_ms).unwrap_or(now),
        updated_ms: now,
    };
    corpus::write(
        store,
        bus,
        site,
        channel,
        "document",
        ChangeOp::Upsert,
        &row.id,
        row.to_change_row(),
    )?;
    Ok(row)
}

/// Remove a document at a slug (a tombstone that travels).
pub fn delete_document(
    store: &crate::store::Store,
    bus: &crate::stream::Bus,
    site: &str,
    channel: &str,
    slug: &str,
) -> Result<bool, crate::store::StoreError> {
    let Some(existing) = store.doc_get(channel, slug)? else {
        return Ok(false);
    };
    corpus::write(
        store,
        bus,
        site,
        channel,
        "document",
        ChangeOp::Delete,
        &existing.id,
        Value::Null,
    )?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kinds_and_titles_follow_the_notebook_scheme() {
        assert_eq!(kind_of("guide-workspace"), "guide");
        assert_eq!(kind_of("ref"), "ref");
        assert_eq!(kind_of("refx-y"), "other");
        assert_eq!(kind_of("todo"), "other");
        assert_eq!(title_of("plan-hdr-todo", "# HDR todo\n\nbody"), "HDR todo");
        assert_eq!(title_of("plan-hdr-todo", "no heading"), "hdr todo");
        assert!(valid_slug("guide-work_space.v2"));
        assert!(!valid_slug("Guide"));
        assert!(!valid_slug("../x"));
        assert!(!valid_slug(""));
    }
}
