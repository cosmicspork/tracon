//! Documents: long, named, read deliberately. `doc_search` finds by content,
//! `doc_read` fetches by slug, `doc_write` creates or edits in either format,
//! and `doc_delete` removes — the last two are not named by the shipped policy,
//! so they are asked.

use serde_json::{json, Value};

use crate::{
    config::DocKind,
    corpus,
    mcp::{CallContext, SessionAccess},
    store::{DocumentDelete, DocumentRow, DocumentWrite},
};

pub const DOC_READ: &str = "doc_read";
pub const DOC_SEARCH: &str = "doc_search";
pub const DOC_WRITE: &str = "doc_write";
pub const DOC_DELETE: &str = "doc_delete";

/// What a document is written as. HTML from `doc_write` is a single-file
/// bundle, stored, validated and served exactly as an operator's import is:
/// only from the preview origin, never on the operator's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Markdown,
    Html,
}

impl Format {
    pub const NAMES: [&'static str; 2] = ["markdown", "html"];

    /// An omitted format is Markdown, which is what `doc_write` always wrote.
    pub fn parse(format: Option<&str>) -> Result<Self, WriteError> {
        match format.map(str::trim) {
            None | Some("markdown") => Ok(Self::Markdown),
            Some("html") => Ok(Self::Html),
            Some(other) => Err(WriteError::Format(other.to_string())),
        }
    }

    /// The format a call's arguments name. A JSON `null` is an omitted one.
    pub fn of_args(args: &Value) -> Result<Self, WriteError> {
        match &args["format"] {
            Value::Null => Self::parse(None),
            Value::String(f) => Self::parse(Some(f)),
            other => Err(WriteError::Format(other.to_string())),
        }
    }
}

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

/// The document a `doc_write` would replace, if one stands at its slug: the
/// current value the operator weighs the proposed body against.
pub fn replaced(
    store: &crate::store::Store,
    channel: &str,
    args: &Value,
) -> Result<Option<DocumentRow>, crate::store::StoreError> {
    let slug = args["slug"].as_str().unwrap_or("").trim();
    if !valid_slug(slug) {
        return Ok(None);
    }
    store.doc_get(channel, slug)
}

/// What the approval page compares a held `doc_write` against: the document
/// as it stands (`null` for a new slug) and whether the `if_hash` the agent
/// sent no longer matches it, which the write would meet as a conflict. The
/// corpus keeps no history, so the base is the current body, not the one the
/// agent read; `stale` says when those differ.
pub fn approval_context(document: Option<&DocumentRow>, args: &Value) -> Value {
    let stale = args["if_hash"]
        .as_str()
        .is_some_and(|want| document.map(|d| d.hash.as_str()) != Some(want));
    json!({
        "document": document.map(|d| json!({
            "body": d.body,
            "hash": d.hash,
            "format": d.format,
            "title": d.title,
            "updated_ms": d.updated_ms,
        })),
        "stale": stale,
    })
}

/// The approval title's tail: whether the write creates the document or
/// edits it, and for an edit the title it has now. Never the body.
pub fn approval_title(summary: &str, document: Option<&DocumentRow>) -> String {
    match document {
        Some(d) => format!("{summary} · edit · \"{}\"", d.title),
        None => format!("{summary} · new"),
    }
}

/// The kind of the documents `show_work` stores its pages and files in.
pub const SHOWN: &str = "shown";

/// What `doc_delete` refuses outright: every slug `doc_write` refuses, and a
/// brief, which the work tools own and an item's criteria are read from.
fn reserved_from_delete(args: &Value) -> Option<String> {
    let slug = args["slug"].as_str().unwrap_or("").trim();
    if reserved_kind_of(slug) == Some(corpus::brief::KIND) {
        return Some(format!(
            "`{slug}` is a work item's brief, which the work tools keep. Say why it should go \
             with `ask_operator`."
        ));
    }
    reserved_for_operator(args)
}

/// Everything a document call can be refused for that its arguments alone
/// decide: a reserved slug, a slug that is not one, a format that is not
/// one, or an HTML body over the per-file limit. Checked before the call is
/// held, so the operator is never asked to allow a write that cannot run,
/// and again when an allowed call runs, against the arguments as allowed.
pub fn refusal_before_asking(name: &str, args: &Value) -> Option<String> {
    let reserved = match name {
        DOC_WRITE => reserved_for_operator(args),
        DOC_DELETE => reserved_from_delete(args),
        _ => return None,
    };
    if reserved.is_some() {
        return reserved;
    }
    let slug = args["slug"].as_str().unwrap_or("").trim();
    if !valid_slug(slug) {
        return Some(WriteError::Slug(slug.to_string()).to_string());
    }
    if name == DOC_WRITE {
        let format = match Format::of_args(args) {
            Ok(format) => format,
            Err(error) => return Some(error.to_string()),
        };
        let body = args["body"].as_str().unwrap_or("");
        if format == Format::Html && body.len() > corpus::html::MAX_FILE_BYTES {
            return Some(
                WriteError::Html(corpus::html::HtmlBundleError::FileTooLarge(html_entry(
                    slug, None,
                )))
                .to_string(),
            );
        }
    }
    None
}

/// The path of the single file an HTML `doc_write` stores: the existing
/// document's entry when it is already HTML, so a replacement keeps it, else
/// `<slug>.html`.
fn html_entry(slug: &str, existing: Option<&DocumentRow>) -> String {
    existing
        .filter(|doc| doc.format == "html")
        .and_then(|doc| doc.entry_path.clone())
        .unwrap_or_else(|| format!("{slug}.html"))
}

/// The source name an HTML `doc_write` files its bundle under, by the same
/// rule as [`html_entry`].
fn html_source_name(slug: &str, existing: Option<&DocumentRow>) -> String {
    existing
        .filter(|doc| doc.format == "html")
        .and_then(|doc| doc.source_name.clone())
        .unwrap_or_else(|| format!("{slug}.html"))
}

/// What the operator should know about a `doc_write` beyond its slug: that it
/// changes the document's format, and how many files of an HTML bundle it
/// drops. `None` for an ordinary edit.
pub fn write_consequence(
    store: &crate::store::Store,
    channel: &str,
    args: &Value,
) -> Option<String> {
    let slug = args["slug"].as_str()?.trim();
    let format = Format::of_args(args).ok()?;
    let existing = store.doc_get(channel, slug).ok().flatten();
    let was_html = existing.as_ref().is_some_and(|doc| doc.format == "html");
    let kept = match format {
        Format::Html => Some(html_entry(slug, existing.as_ref())),
        Format::Markdown => None,
    };
    let dropped = match existing.as_ref().filter(|_| was_html) {
        Some(doc) => store
            .html_bundle_metadata(doc)
            .ok()?
            .into_iter()
            .filter(|file| Some(&file.path) != kept.as_ref())
            .count(),
        None => 0,
    };
    let files = |n: usize| format!("{n} file{}", if n == 1 { "" } else { "s" });
    let drops = (dropped > 0).then(|| format!("drops {} of its HTML bundle", files(dropped)));
    let converts = match (format, existing.as_ref()) {
        (Format::Markdown, Some(_)) if was_html => Some("converts HTML to Markdown"),
        (Format::Html, Some(_)) if !was_html => Some("converts Markdown to HTML"),
        (Format::Html, None) => Some("as HTML"),
        _ => None,
    };
    match (converts, drops) {
        (Some(c), Some(d)) => Some(format!("{c}; {d}")),
        (Some(c), None) => Some(c.to_string()),
        (None, Some(d)) => Some(d),
        (None, None) => None,
    }
}

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
         of {}; any other prefix is filed as `{OTHER}`. `format` is `markdown` (the default) or \
         `html`; writing the other format converts the document in place, keeping its archived \
         and pinned state. An HTML body is one self-contained page, stored as a single-file \
         bundle and shown only in the sandboxed preview; replacing a multi-file bundle drops \
         its other files. Pass if_hash (from doc_read) to refuse overwriting an edit you have \
         not seen. The operator is asked before this runs, unless the policy writes it \
         unattended (working notes): an asked call returns `awaiting_operator` with an \
         `approval_id` at once, and approval_status reports the outcome.",
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
                    "format": { "type": "string", "enum": Format::NAMES, "default": "markdown" },
                    "if_hash": { "type": "string" },
                },
                "required": ["slug", "body"],
            },
        }),
        json!({
            "name": DOC_DELETE,
            "description": "Delete a document on this channel, in either format; an HTML \
                document's bundle goes with it. Pass if_hash (from doc_read) to refuse deleting \
                an edit you have not seen. A work item's brief, its selected context and shown \
                work are refused. The operator is always asked: the call returns \
                `awaiting_operator` with an `approval_id` at once, and approval_status reports \
                the outcome.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "slug": { "type": "string" },
                    "if_hash": { "type": "string" },
                },
                "required": ["slug"],
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
            let format = Format::of_args(args).map_err(|e| e.to_string())?;
            let doc = write_document_as(
                &access.store,
                access.manager.bus(),
                &kinds,
                &node_id,
                &ctx.channel,
                slug,
                format,
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
            Ok(json!({ "slug": doc.slug, "hash": doc.hash, "format": doc.format }))
        }
        DOC_DELETE => {
            let slug = args["slug"].as_str().unwrap_or("").trim();
            let removed = delete_document(
                &access.store,
                access.manager.bus(),
                access.manager.node_id(),
                &ctx.channel,
                slug,
                args["if_hash"].as_str(),
            )
            .map_err(|e| e.to_string())?;
            if !removed {
                return Err(format!("no document {slug} on channel {}", ctx.channel));
            }
            Ok(json!({ "slug": slug, "deleted": true }))
        }
        other => Err(format!("no tool named {other}")),
    }
}

#[derive(Debug, thiserror::Error)]
pub enum WriteError {
    #[error("slug {0:?} is not lowercase letters, digits, dots, dashes, and underscores")]
    Slug(String),
    #[error("format {0} is not `markdown` or `html`")]
    Format(String),
    #[error("the document changed since it was read; its hash is now {hash}")]
    Conflict { hash: String, body: String },
    #[error(transparent)]
    Html(#[from] corpus::html::HtmlBundleError),
    #[error(transparent)]
    Store(#[from] crate::store::StoreError),
}

impl From<crate::store::HtmlWriteError> for WriteError {
    fn from(error: crate::store::HtmlWriteError) -> Self {
        use crate::store::HtmlWriteError as E;
        match error {
            E::Bundle(error) => Self::Html(error),
            E::Conflict { hash, body } => Self::Conflict { hash, body },
            E::InvalidSourceName => {
                Self::Store(crate::store::StoreError::Invalid(error.to_string()))
            }
            E::Sqlite(error) => Self::Store(error.into()),
            E::Sync(error) => Self::Store(crate::store::corpus::sync_err(error)),
        }
    }
}

/// Create or replace a Markdown document at a slug, on this node, as a sync
/// write: [`write_document_as`] with [`Format::Markdown`].
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
    write_document_as(
        store,
        bus,
        kinds,
        site,
        channel,
        slug,
        Format::Markdown,
        body,
        if_hash,
        create_only,
        archived,
        pinned,
    )
}

/// Create or replace a document at a slug in `format`, on this node, as a
/// sync write. Writing the other format converts the document in place.
/// `kinds` is the node's configured list: the kind stored with the row is a
/// cache of what it classified as when written, never what it is read as.
/// `if_hash` is the edit's precondition: the hash the caller last read,
/// whatever format it was then. `create_only` is the HTTP `If-None-Match: *`
/// precondition. `archived` and `pinned` of `None` leave the document as it
/// was.
#[allow(clippy::too_many_arguments)]
pub fn write_document_as(
    store: &crate::store::Store,
    bus: &crate::stream::Bus,
    kinds: &[DocKind],
    site: &str,
    channel: &str,
    slug: &str,
    format: Format,
    body: &str,
    if_hash: Option<&str>,
    create_only: bool,
    archived: Option<bool>,
    pinned: Option<bool>,
) -> Result<DocumentRow, WriteError> {
    if !valid_slug(slug) {
        return Err(WriteError::Slug(slug.to_string()));
    }
    let (row, changes) = match format {
        Format::Markdown => {
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
                DocumentWrite::Written { row, changes } => (*row, changes),
                DocumentWrite::Conflict { hash, body } => {
                    return Err(WriteError::Conflict { hash, body })
                }
            }
        }
        // Through the operator's own import writer: the same path checks,
        // limits and generation hash, and so the same preview-only serving.
        Format::Html => {
            let existing = store.doc_get(channel, slug)?;
            let entry = html_entry(slug, existing.as_ref());
            store.write_html_document_change(
                kinds,
                site,
                channel,
                slug,
                &html_source_name(slug, existing.as_ref()),
                &entry,
                vec![corpus::html::HtmlFile {
                    path: entry.clone(),
                    bytes: body.as_bytes().to_vec(),
                }],
                if_hash,
                create_only,
                archived,
                pinned,
            )?
        }
    };
    // One frame, in the order written: a peer applies a generation's files
    // before the document that names it, and its tombstones after.
    bus.publish(crate::stream::Frame::Changes {
        channel: channel.to_string(),
        changes,
    });
    Ok(row)
}

/// Remove a document at a slug (a tombstone that travels), with its HTML
/// bundle. `if_hash` refuses removing a version the caller has not read.
/// `Ok(false)` when there is no such document.
pub fn delete_document(
    store: &crate::store::Store,
    bus: &crate::stream::Bus,
    site: &str,
    channel: &str,
    slug: &str,
    if_hash: Option<&str>,
) -> Result<bool, WriteError> {
    match store.delete_document_change(site, channel, slug, if_hash)? {
        DocumentDelete::Deleted(changes) => {
            bus.publish(crate::stream::Frame::Changes {
                channel: channel.to_string(),
                changes,
            });
            Ok(true)
        }
        DocumentDelete::Missing => Ok(false),
        DocumentDelete::Conflict { hash, body } => Err(WriteError::Conflict { hash, body }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Barrier};

    /// An `if_hash` for a document that no longer stands is as stale as one
    /// for a document that changed: the write would meet either as a
    /// conflict. Without an `if_hash` nothing is stale.
    #[test]
    fn an_if_hash_with_no_document_behind_it_is_stale() {
        let ctx = approval_context(None, &json!({ "slug": "plan-foo", "if_hash": "h" }));
        assert_eq!(ctx, json!({ "document": null, "stale": true }));
        let ctx = approval_context(None, &json!({ "slug": "plan-foo" }));
        assert_eq!(ctx, json!({ "document": null, "stale": false }));
    }

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
            ["slug", "body", "format", "if_hash"].into_iter().collect(),
            "{props:?}"
        );
        assert!(!props.contains_key("pinned"), "{props:?}");
        assert!(!props.contains_key("archived"), "{props:?}");
    }

    #[test]
    fn an_html_body_over_the_file_limit_is_refused_before_asking() {
        let limit = corpus::html::MAX_FILE_BYTES;
        let args = |body: String, format: &str| json!({ "slug": "ref-big", "body": body, "format": format });
        let refused = refusal_before_asking(DOC_WRITE, &args("x".repeat(limit + 1), "html"));
        assert!(
            refused.as_deref().is_some_and(|r| r.contains("byte limit")),
            "{refused:?}"
        );
        assert_eq!(
            refusal_before_asking(DOC_WRITE, &args("x".repeat(limit), "html")),
            None
        );
        // Markdown has no such limit of its own.
        assert_eq!(
            refusal_before_asking(DOC_WRITE, &args("x".repeat(limit + 1), "markdown")),
            None
        );
        assert_eq!(
            refusal_before_asking(DOC_READ, &json!({ "slug": "Bad" })),
            None
        );
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
    fn the_kind_filter_classifies_when_it_reads_not_by_the_stored_kind() {
        let store = crate::store::Store::open_in_memory().unwrap();
        let bus = crate::stream::Bus::new();
        let defaults = crate::config::default_doc_kinds();
        // Written while the node knew no runbook kind: stored as `other`.
        let written = write_document(
            &store,
            &bus,
            &defaults,
            "A",
            "personal",
            "runbook-deploy",
            "# Deploy\n\nRoll the hub behind the proxy.",
            None,
            true,
            None,
            None,
        )
        .unwrap();
        assert_eq!(written.kind, OTHER);
        let mut kinds = defaults.clone();
        kinds.push(runbook());
        let found = |kind: &str| {
            store
                .doc_search(
                    None,
                    Some(KindFilter {
                        kinds: &kinds,
                        kind,
                    }),
                    "hub proxy",
                    5,
                )
                .unwrap()
                .into_iter()
                .filter_map(|hit| hit.slug)
                .collect::<Vec<_>>()
        };
        assert_eq!(found("runbook"), ["runbook-deploy"]);
        assert!(found(OTHER).is_empty(), "{:?}", found(OTHER));
        // Under the list it was written with, it is still `other`.
        let before = store
            .doc_search(
                None,
                Some(KindFilter {
                    kinds: &defaults,
                    kind: OTHER,
                }),
                "hub proxy",
                5,
            )
            .unwrap();
        assert_eq!(before.len(), 1);
    }

    #[test]
    fn the_kind_list_is_the_configured_kinds_then_the_reserved_then_other() {
        let mut kinds = crate::config::default_doc_kinds();
        kinds.push(DocKind {
            label: String::new(),
            ..runbook()
        });
        let list = kind_list(&kinds);
        let order: Vec<&str> = list.iter().map(|k| k["kind"].as_str().unwrap()).collect();
        assert_eq!(
            order,
            [
                "guide",
                "ref",
                "architecture",
                "proposal",
                "repo",
                "note",
                "meeting",
                "inbox",
                "runbook",
                "brief",
                "context",
                "plan",
                "shown",
                "other",
            ]
        );
        for entry in &list {
            let keys: Vec<&str> = entry
                .as_object()
                .unwrap()
                .keys()
                .map(String::as_str)
                .collect();
            assert_eq!(
                keys,
                ["description", "kind", "label", "reserved", "working_note"],
                "{entry}"
            );
        }
        assert_eq!(list[1]["label"], "Reference");
        assert_eq!(list[4]["working_note"], true);
        assert_eq!(list[4]["reserved"], false);
        // A configured kind with no label is called by its prefix.
        assert_eq!(list[8]["label"], "runbook");
        assert_eq!(list[11]["label"], "Plan");
        assert_eq!(list[11]["reserved"], true);
        assert_eq!(list[13]["label"], "Other");
        assert_eq!(list[13]["reserved"], true);
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
