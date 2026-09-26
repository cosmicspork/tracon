//! The context bundle: the documents the operator selected for one work item,
//! and the record of what each attempt at that item actually received.
//!
//! Three things are kept apart here, because each answers a different
//! question and losing any one of them makes the other two unverifiable.
//!
//! *The selection* is what the operator chose: the item's brief, the research
//! behind it, the decisions already taken, the constraints it must keep, and
//! any other document that matters to this item and not to the whole channel.
//! It is a Markdown document, `context-<item prefix>`, on the item's channel —
//! one line per document, `- [doc:slug] why it is here`, under a heading that
//! says what role it plays. It is readable and editable with no node running,
//! it travels with the channel like any other document, and it is the
//! operator's: a session may read it but `doc_write` refuses to change it,
//! because a session choosing its own context is exactly the substitution
//! this exists to prevent.
//!
//! *The delivery* is what a launch makes of the selection: each document
//! resolved against what this node holds, placed in the orientation in full
//! within a bound of its own, and anything that could not be placed — not on
//! this node, archived, an HTML bundle, or past the bound — named with the
//! call that fetches it. Channel-wide pinned documents and memory recall are
//! not consulted; they are not what was selected.
//!
//! *The receipt* is the delivery written down for one session: which document
//! at which hash, delivered whole, cut short or left out and why. Receipts are
//! never rewritten. Their digest identifies what was received, independent of
//! which session received it, and a new digest for an item mints the item's
//! next context revision — so "revision 3" means the same bytes wherever it is
//! read. Each receipt also says what changed since the previous attempt at the
//! item: documents added, removed, moved between roles, edited since, or
//! delivered differently. A revised attempt that was not told its context
//! changed, or an operator who cannot see that it did, is the failure this
//! record is for.

use serde::{Deserialize, Serialize};

use crate::corpus::orientation::Missing;
use crate::mcp::docs;
use crate::store::{now_ms, Store, StoreError};
use crate::stream::Bus;
use tracon_sync::work::WorkItem;

/// The document kind a selection is stored under.
pub const KIND: &str = "context";
/// Roughly twelve thousand tokens. The selected documents' own allowance in
/// the orientation: generous, because the operator chose each one for this
/// item, but bounded, because one oversized research dump must not become
/// the session. What does not fit is named, never silently dropped.
pub const BUNDLE_CHARS: usize = 48_000;
/// A document cut shorter than this is not worth the partial: it is left out
/// whole and named instead.
const MIN_PARTIAL: usize = 1_000;
/// A selection is a short list; this many lines is a document that lost its
/// shape.
const MAX_ENTRIES: usize = 64;
const MAX_NOTE: usize = 500;

/// What a selected document is for. The order is the order a session reads
/// them in, and the order the operator's allowance is spent in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    Brief,
    Research,
    Decisions,
    Constraints,
    Documents,
}

pub const ROLES: &[Role] = &[
    Role::Brief,
    Role::Research,
    Role::Decisions,
    Role::Constraints,
    Role::Documents,
];

impl Role {
    pub fn heading(self) -> &'static str {
        match self {
            Role::Brief => "Brief",
            Role::Research => "Research",
            Role::Decisions => "Decisions",
            Role::Constraints => "Constraints",
            Role::Documents => "Documents",
        }
    }

    pub fn key(self) -> &'static str {
        match self {
            Role::Brief => "brief",
            Role::Research => "research",
            Role::Decisions => "decisions",
            Role::Constraints => "constraints",
            Role::Documents => "documents",
        }
    }

    /// A heading or a key, singular or plural, however it was cased.
    pub fn parse(text: &str) -> Option<Role> {
        let t = text.trim().to_ascii_lowercase();
        let t = t.trim_end_matches('s');
        ROLES
            .iter()
            .copied()
            .find(|r| r.key().trim_end_matches('s') == t)
    }
}

/// One selected document.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pick {
    pub role: Role,
    pub slug: String,
    /// Why the operator selected it, when they said.
    #[serde(default)]
    pub note: String,
}

/// A selection as this module reads it. `preamble` and `extra` are whatever
/// was written that is not a line of the selection — kept, rendered back,
/// never dropped because it did not fit the shape.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Selection {
    pub title: String,
    #[serde(default)]
    pub preamble: String,
    pub picks: Vec<Pick>,
    #[serde(default)]
    pub extra: String,
}

/// The selection document for an item: `context-<id prefix>`, beside
/// `brief-<prefix>` and `plan-<prefix>`.
pub fn slug_for(item_id: &str) -> String {
    format!("context-{}", &item_id[..12.min(item_id.len())])
}

impl Selection {
    pub fn empty(item: &WorkItem) -> Selection {
        Selection {
            title: format!("Context: {}", item.title.trim()),
            preamble: format!(
                "Selected by the operator for work item `{}`. Every attempt at the item starts \
                 with these documents.",
                item.id
            ),
            picks: Vec::new(),
            extra: String::new(),
        }
    }

    /// Read a selection out of its Markdown. A `- [doc:slug]` line under a
    /// role heading is a pick; everything else is kept where it can be put
    /// back.
    pub fn parse(body: &str) -> Selection {
        let mut title = String::new();
        let mut preamble = Vec::new();
        let mut extra = Vec::new();
        let mut picks = Vec::new();
        // `None` before the first `## `; `Some(None)` under a heading this
        // module does not know.
        let mut role: Option<Option<Role>> = None;
        for line in body.lines() {
            if title.is_empty() && role.is_none() {
                if let Some(t) = line.strip_prefix("# ") {
                    title = t.trim().to_string();
                    continue;
                }
            }
            if let Some(h) = line.strip_prefix("## ") {
                let known = Role::parse(h);
                role = Some(known);
                if known.is_none() {
                    extra.push(line.to_string());
                }
                continue;
            }
            match role {
                None => preamble.push(line.to_string()),
                Some(None) => extra.push(line.to_string()),
                Some(Some(r)) => match parse_pick(line) {
                    Some((slug, note)) => picks.push(Pick {
                        role: r,
                        slug,
                        note,
                    }),
                    // Prose under a role heading is not a pick. It is not
                    // lost either: it goes with whatever else did not fit.
                    None if !line.trim().is_empty() => extra.push(line.to_string()),
                    None => {}
                },
            }
        }
        Selection {
            title,
            preamble: preamble.join("\n").trim().to_string(),
            picks,
            extra: extra.join("\n").trim().to_string(),
        }
    }

    /// The canonical Markdown. Roles in reading order, picks in the order
    /// they were chosen within a role.
    pub fn render(&self) -> String {
        let mut out = format!("# {}\n\n", self.title.trim());
        if !self.preamble.is_empty() {
            out.push_str(self.preamble.trim());
            out.push_str("\n\n");
        }
        for role in ROLES {
            let picks: Vec<_> = self.picks.iter().filter(|p| p.role == *role).collect();
            if picks.is_empty() {
                continue;
            }
            out.push_str(&format!("## {}\n\n", role.heading()));
            for p in picks {
                if p.note.is_empty() {
                    out.push_str(&format!("- [doc:{}]\n", p.slug));
                } else {
                    out.push_str(&format!("- [doc:{}] {}\n", p.slug, p.note));
                }
            }
            out.push('\n');
        }
        if !self.extra.is_empty() {
            out.push_str(self.extra.trim());
            out.push('\n');
        }
        out.trim_end().to_string() + "\n"
    }

    /// Picks in delivery order: by role, then as chosen.
    pub fn ordered(&self) -> Vec<&Pick> {
        ROLES
            .iter()
            .flat_map(|r| self.picks.iter().filter(move |p| p.role == *r))
            .collect()
    }
}

/// `- [doc:slug] note` → `(slug, note)`.
fn parse_pick(line: &str) -> Option<(String, String)> {
    let rest = line
        .trim()
        .strip_prefix("- ")
        .or_else(|| line.trim().strip_prefix("* "))?;
    let rest = rest.trim_start().strip_prefix("[doc:")?;
    let end = rest.find(']')?;
    let slug = rest[..end].trim().to_string();
    if !docs::valid_slug(&slug) {
        return None;
    }
    let note = rest[end + 1..].trim();
    let note = note
        .strip_prefix('—')
        .or_else(|| note.strip_prefix("- "))
        .unwrap_or(note)
        .trim();
    Some((slug, note.to_string()))
}

/// Why a selected document did not reach a session whole.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Reason {
    /// This node holds no document at that slug: never synced here, or
    /// deleted.
    Absent,
    /// Archived since it was selected.
    Archived,
    /// An HTML bundle: not text an orientation can carry.
    Html,
    /// Past the selected context's allowance.
    Cap,
}

impl Reason {
    pub fn says(self) -> &'static str {
        match self {
            Reason::Absent => "not on this node",
            Reason::Archived => "archived",
            Reason::Html => "an HTML document",
            Reason::Cap => "past the selected context's allowance",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Delivery {
    Full,
    Partial,
    Omitted,
}

/// One selected document, as one attempt received it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Received {
    pub role: Role,
    pub slug: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub note: String,
    /// The document's title and hash when this node held it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hash: Option<String>,
    /// The document's whole length, and how much of it the session got.
    pub chars: usize,
    pub delivered_chars: usize,
    pub delivery: Delivery,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<Reason>,
}

/// A selection resolved for one launch: the text the orientation carries,
/// what each document came to, and what the session is owed a name for.
#[derive(Debug, Clone, Default)]
pub struct Delivered {
    /// The selection document's hash, when there is one.
    pub selection_hash: Option<String>,
    pub received: Vec<Received>,
    /// The documents themselves, as the orientation places them.
    pub text: String,
    pub missing: Vec<Missing>,
}

/// Resolve an item's selection against this node. `None` when the item has
/// no selection document — which is the ordinary case, and not a gap.
pub fn deliver(store: &Store, item: &WorkItem) -> Result<Option<Delivered>, StoreError> {
    let Some(doc) = store.doc_get(&item.channel, &slug_for(&item.id))? else {
        return Ok(None);
    };
    if doc.format != "markdown" {
        return Ok(None);
    }
    let selection = Selection::parse(&doc.body);
    let mut out = Delivered {
        selection_hash: Some(doc.hash.clone()),
        ..Default::default()
    };
    let mut left = BUNDLE_CHARS;
    let mut role = None;
    for pick in selection.ordered() {
        let found = store.doc_get(&item.channel, &pick.slug)?;
        let mut r = Received {
            role: pick.role,
            slug: pick.slug.clone(),
            note: pick.note.clone(),
            title: found.as_ref().map(|d| d.title.clone()),
            hash: found.as_ref().map(|d| d.hash.clone()),
            chars: found.as_ref().map(|d| d.body.trim().len()).unwrap_or(0),
            delivered_chars: 0,
            delivery: Delivery::Omitted,
            reason: None,
        };
        let body = match &found {
            None => {
                r.reason = Some(Reason::Absent);
                None
            }
            Some(d) if d.archived != 0 => {
                r.reason = Some(Reason::Archived);
                None
            }
            Some(d) if d.format != "markdown" => {
                r.reason = Some(Reason::Html);
                None
            }
            Some(d) => Some(d.body.trim()),
        };
        if let Some(body) = body {
            let take = if body.len() <= left {
                r.delivery = Delivery::Full;
                body.len()
            } else if left >= MIN_PARTIAL {
                r.delivery = Delivery::Partial;
                r.reason = Some(Reason::Cap);
                floor_char(body, left)
            } else {
                r.reason = Some(Reason::Cap);
                0
            };
            if take > 0 {
                if role != Some(pick.role) {
                    out.text
                        .push_str(&format!("### {}\n\n", pick.role.heading()));
                    role = Some(pick.role);
                }
                let title = found.as_ref().map(|d| d.title.as_str()).unwrap_or("");
                out.text
                    .push_str(&format!("#### {} (`{}`)\n\n", title, pick.slug));
                if !pick.note.is_empty() {
                    out.text
                        .push_str(&format!("_Selected because: {}_\n\n", pick.note));
                }
                out.text.push_str(&body[..take]);
                if r.delivery == Delivery::Partial {
                    out.text
                        .push_str("\n\n[cut here; see the end of this orientation]");
                }
                out.text.push_str("\n\n");
                left -= take;
                r.delivered_chars = take;
            }
        }
        if r.delivery != Delivery::Full {
            let reason = r.reason.unwrap_or(Reason::Absent);
            out.missing.push(Missing {
                what: format!(
                    "selected {} `{}` ({})",
                    pick.role.key(),
                    pick.slug,
                    reason.says()
                ),
                partial: r.delivery == Delivery::Partial,
                chars: r.chars - r.delivered_chars,
                fetch: match reason {
                    Reason::Absent => {
                        Some("ask the operator; this node does not hold it".to_string())
                    }
                    _ => Some(format!("call `doc_read` for `{}`", pick.slug)),
                },
            });
        }
        out.received.push(r);
    }
    Ok(Some(out))
}

/// One difference between two attempts' context.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Change {
    /// `added`, `removed`, `moved`, `edited` or `delivery`.
    pub kind: String,
    pub slug: String,
    pub role: Role,
    /// The change, in a sentence an operator or a session can act on.
    pub says: String,
}

/// What changed in an item's context from one attempt to the next. Keyed by
/// slug: a document is the unit the operator selects.
pub fn changes(previous: &[Received], now: &[Received]) -> Vec<Change> {
    let mut out = Vec::new();
    for r in now {
        let Some(p) = previous.iter().find(|p| p.slug == r.slug) else {
            out.push(Change {
                kind: "added".into(),
                slug: r.slug.clone(),
                role: r.role,
                says: format!("`{}` added as {}", r.slug, r.role.key()),
            });
            continue;
        };
        if p.role != r.role {
            out.push(Change {
                kind: "moved".into(),
                slug: r.slug.clone(),
                role: r.role,
                says: format!(
                    "`{}` moved from {} to {}",
                    r.slug,
                    p.role.key(),
                    r.role.key()
                ),
            });
        }
        if p.hash.is_some() && r.hash.is_some() && p.hash != r.hash {
            out.push(Change {
                kind: "edited".into(),
                slug: r.slug.clone(),
                role: r.role,
                says: format!("`{}` edited since the previous attempt", r.slug),
            });
        }
        if p.delivery != r.delivery || p.reason != r.reason {
            out.push(Change {
                kind: "delivery".into(),
                slug: r.slug.clone(),
                role: r.role,
                says: format!(
                    "`{}` was {}, now {}",
                    r.slug,
                    delivery_words(p),
                    delivery_words(r)
                ),
            });
        }
    }
    for p in previous {
        if !now.iter().any(|r| r.slug == p.slug) {
            out.push(Change {
                kind: "removed".into(),
                slug: p.slug.clone(),
                role: p.role,
                says: format!("`{}` no longer selected", p.slug),
            });
        }
    }
    out
}

fn delivery_words(r: &Received) -> String {
    match (r.delivery, r.reason) {
        (Delivery::Full, _) => "delivered in full".into(),
        (Delivery::Partial, _) => "cut short".into(),
        (Delivery::Omitted, Some(reason)) => format!("left out ({})", reason.says()),
        (Delivery::Omitted, None) => "left out".into(),
    }
}

/// What one attempt received, as recorded. Written once, never rewritten.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Receipt {
    pub session_id: String,
    pub work_item_id: String,
    pub channel: String,
    /// The item's context revision: the same number for the same digest.
    pub revision: i64,
    /// Over what was received, not who received it or when.
    pub digest: String,
    /// The selection document's hash at launch; `None` once it is gone.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selection_hash: Option<String>,
    pub received: Vec<Received>,
    /// The attempt this one is compared with, and what differs from it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous_session: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous_revision: Option<i64>,
    #[serde(default)]
    pub changes: Vec<Change>,
    pub created_ms: i64,
}

impl Receipt {
    /// Documents that did not reach the session whole.
    pub fn omissions(&self) -> impl Iterator<Item = &Received> {
        self.received
            .iter()
            .filter(|r| r.delivery != Delivery::Full)
    }
}

/// The digest of what was received: every document's role, slug, note, hash
/// and delivery, in delivery order.
pub fn digest(received: &[Received]) -> String {
    use sha2::{Digest, Sha256};
    let canonical = serde_json::to_string(
        &received
            .iter()
            .map(|r| {
                serde_json::json!([
                    r.role.key(),
                    r.slug,
                    r.note,
                    r.hash,
                    r.delivery,
                    r.delivered_chars
                ])
            })
            .collect::<Vec<_>>(),
    )
    .unwrap_or_default();
    hex::encode(Sha256::digest(canonical.as_bytes()))
}

/// Deliver an item's context for a session and record the receipt. Returns
/// `None` only when there is nothing to say: no selection, and no earlier
/// attempt that had one. An item whose selection was removed after an
/// attempt used it still gets a receipt, so the removal is visible.
pub fn prepare(
    store: &Store,
    session_id: &str,
    item: &WorkItem,
) -> Result<Option<(Delivered, Receipt)>, StoreError> {
    let previous = store.context_receipt_latest(&item.id)?;
    let delivered = match (deliver(store, item)?, &previous) {
        (Some(d), _) => d,
        (None, Some(_)) => Delivered::default(),
        (None, None) => return Ok(None),
    };
    let changes = previous
        .as_ref()
        .map(|p| changes(&p.received, &delivered.received))
        .unwrap_or_default();
    let receipt = Receipt {
        session_id: session_id.to_string(),
        work_item_id: item.id.clone(),
        channel: item.channel.clone(),
        revision: 0,
        digest: digest(&delivered.received),
        selection_hash: delivered.selection_hash.clone(),
        received: delivered.received.clone(),
        previous_session: previous.as_ref().map(|p| p.session_id.clone()),
        previous_revision: previous.as_ref().map(|p| p.revision),
        changes,
        created_ms: now_ms(),
    };
    let receipt = store.context_receipt_record(&receipt)?;
    Ok(Some((delivered, receipt)))
}

/// The orientation's account of the context: which revision, and what is
/// different from the previous attempt. Empty changes on a later attempt are
/// said, too — "unchanged" is information a revised attempt can use.
pub fn revision_line(receipt: &Receipt) -> String {
    let mut out = format!("Context revision {} for this item.", receipt.revision);
    match receipt.previous_revision {
        None => out.push_str(" This is the first attempt to receive it."),
        Some(_) if receipt.changes.is_empty() => {
            out.push_str(" Unchanged since the previous attempt.")
        }
        Some(prev) => {
            out.push_str(&format!(
                " Since the previous attempt (revision {prev}): {}.",
                receipt
                    .changes
                    .iter()
                    .map(|c| c.says.as_str())
                    .collect::<Vec<_>>()
                    .join("; ")
            ));
        }
    }
    out
}

/// A selection as a reader gets it: each pick resolved against this node, so
/// a document the next attempt will not receive is visible before it runs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SelectionView {
    pub channel: String,
    pub slug: String,
    pub work_item_id: String,
    /// The `If-Match` value for the next edit.
    pub hash: String,
    pub updated_ms: i64,
    #[serde(flatten)]
    pub selection: Selection,
    pub resolved: Vec<Resolved>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Resolved {
    pub role: Role,
    pub slug: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    pub known: bool,
    pub chars: usize,
    /// Why the next attempt would not receive it, when it would not.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<Reason>,
}

pub fn read(store: &Store, item: &WorkItem) -> Result<Option<SelectionView>, StoreError> {
    let Some(doc) = store.doc_get(&item.channel, &slug_for(&item.id))? else {
        return Ok(None);
    };
    if doc.format != "markdown" {
        return Ok(None);
    }
    let selection = Selection::parse(&doc.body);
    let mut resolved = Vec::new();
    for p in selection.ordered() {
        let found = store.doc_get(&item.channel, &p.slug)?;
        resolved.push(Resolved {
            role: p.role,
            slug: p.slug.clone(),
            title: found.as_ref().map(|d| d.title.clone()),
            known: found.is_some(),
            chars: found.as_ref().map(|d| d.body.trim().len()).unwrap_or(0),
            reason: match &found {
                None => Some(Reason::Absent),
                Some(d) if d.archived != 0 => Some(Reason::Archived),
                Some(d) if d.format != "markdown" => Some(Reason::Html),
                Some(_) => None,
            },
        });
    }
    Ok(Some(SelectionView {
        channel: doc.channel.clone(),
        slug: doc.slug.clone(),
        work_item_id: item.id.clone(),
        hash: doc.hash.clone(),
        updated_ms: doc.updated_ms,
        selection,
        resolved,
    }))
}

#[derive(Debug, thiserror::Error)]
pub enum ContextError {
    #[error("no work item {0}")]
    MissingItem(String),
    #[error("{0:?} is not a document slug")]
    Slug(String),
    #[error("{0:?} is not one of brief, research, decisions, constraints, documents")]
    Role(String),
    #[error("`{0}` is selected twice; a document plays one role")]
    Duplicate(String),
    #[error("the selection cannot include itself")]
    SelfReference,
    #[error("a selection holds at most {MAX_ENTRIES} documents")]
    TooMany,
    #[error("a note is at most {MAX_NOTE} characters, on one line")]
    Note,
    #[error("the selection changed since it was read; its hash is now {hash}")]
    Conflict { hash: String },
    #[error("{0}")]
    Write(String),
    #[error("{0}")]
    Store(#[from] StoreError),
}

#[derive(Debug, Default, Deserialize)]
pub struct SelectionInput {
    /// The whole selection, replacing the previous one.
    pub picks: Vec<PickInput>,
    pub title: Option<String>,
    /// The hash the caller last read. Absent on the first write.
    pub if_hash: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct PickInput {
    pub role: String,
    pub slug: String,
    #[serde(default)]
    pub note: String,
}

/// Replace an item's selection. The operator's path; sessions have none.
/// Preamble and anything else written by hand into the document stays.
pub fn write(
    store: &Store,
    bus: &Bus,
    site: &str,
    item_id: &str,
    input: SelectionInput,
) -> Result<SelectionView, ContextError> {
    let item = store
        .work_get(item_id)?
        .ok_or_else(|| ContextError::MissingItem(item_id.to_string()))?;
    let slug = slug_for(&item.id);
    let current = store.doc_get(&item.channel, &slug)?;
    if let Some(want) = input.if_hash.as_deref() {
        let have = current.as_ref().map(|d| d.hash.clone()).unwrap_or_default();
        if have != want {
            return Err(ContextError::Conflict { hash: have });
        }
    }
    if input.picks.len() > MAX_ENTRIES {
        return Err(ContextError::TooMany);
    }
    let mut selection = match &current {
        Some(doc) => Selection::parse(&doc.body),
        None => Selection::empty(&item),
    };
    if let Some(title) = input.title.map(|t| t.trim().to_string()) {
        if !title.is_empty() {
            selection.title = title;
        }
    }
    let mut picks: Vec<Pick> = Vec::with_capacity(input.picks.len());
    for p in input.picks {
        let role = Role::parse(&p.role).ok_or_else(|| ContextError::Role(p.role.clone()))?;
        let pick_slug = p.slug.trim().to_string();
        if !docs::valid_slug(&pick_slug) {
            return Err(ContextError::Slug(pick_slug));
        }
        if pick_slug == slug {
            return Err(ContextError::SelfReference);
        }
        if picks.iter().any(|q| q.slug == pick_slug) {
            return Err(ContextError::Duplicate(pick_slug));
        }
        let note = p.note.trim().to_string();
        if note.chars().count() > MAX_NOTE || note.contains('\n') {
            return Err(ContextError::Note);
        }
        picks.push(Pick {
            role,
            slug: pick_slug,
            note,
        });
    }
    selection.picks = picks;
    docs::write_document(
        store,
        bus,
        site,
        &item.channel,
        &slug,
        &selection.render(),
        current.as_ref().and(input.if_hash.as_deref()),
        false,
        None,
        None,
    )
    .map_err(|e| match e {
        docs::WriteError::Conflict { hash, .. } => ContextError::Conflict { hash },
        other => ContextError::Write(other.to_string()),
    })?;
    Ok(read(store, &item)?.expect("the selection was just written"))
}

fn floor_char(s: &str, at: usize) -> usize {
    let mut i = at.min(s.len());
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

    fn item() -> WorkItem {
        WorkItem {
            id: "item0001deadbeefcafe".into(),
            channel: "personal".into(),
            project_id: None,
            title: "Triage alerts".into(),
            body: String::new(),
            state: "open".into(),
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

    fn doc(store: &Store, slug: &str, body: &str, extra: serde_json::Value) {
        let mut row = json!({
            "channel": "personal", "slug": slug, "kind": docs::kind_of(slug),
            "title": docs::title_of(slug, body), "body": body,
            "hash": crate::corpus::hash_body(body), "created_ms": 1, "updated_ms": 1,
        });
        for (k, v) in extra.as_object().unwrap() {
            row[k] = v.clone();
        }
        store
            .write_change("n", "personal", "document", ChangeOp::Upsert, slug, row)
            .unwrap();
    }

    fn select(store: &Store, picks: &[(&str, &str, &str)]) {
        let it = item();
        let mut s = Selection::empty(&it);
        s.picks = picks
            .iter()
            .map(|(role, slug, note)| Pick {
                role: Role::parse(role).unwrap(),
                slug: slug.to_string(),
                note: note.to_string(),
            })
            .collect();
        doc(store, &slug_for(&it.id), &s.render(), json!({}));
    }

    #[test]
    fn a_selection_round_trips_and_keeps_what_it_does_not_understand() {
        let body = "# Context: Triage\n\nChosen for the triage item.\n\n\
                    ## Research\n\n- [doc:ref-interviews] three admins, one night shift\n\
                    Some prose under research.\n\n\
                    ## brief\n\n* [doc:brief-abc]\n\n\
                    ## Scratch\n\nnot a role\n\n\
                    ## Constraints\n\n- [doc:guide-api-limits] — rate limits\n";
        let s = Selection::parse(body);
        assert_eq!(s.title, "Context: Triage");
        assert_eq!(s.preamble, "Chosen for the triage item.");
        assert_eq!(
            s.picks,
            vec![
                Pick {
                    role: Role::Research,
                    slug: "ref-interviews".into(),
                    note: "three admins, one night shift".into()
                },
                Pick {
                    role: Role::Brief,
                    slug: "brief-abc".into(),
                    note: String::new()
                },
                Pick {
                    role: Role::Constraints,
                    slug: "guide-api-limits".into(),
                    note: "rate limits".into()
                },
            ]
        );
        assert!(s.extra.contains("Some prose under research."));
        assert!(s.extra.contains("## Scratch"));
        assert!(s.extra.contains("not a role"));
        // Delivery order is reading order, not the order the file was written.
        let order: Vec<_> = s.ordered().iter().map(|p| p.slug.as_str()).collect();
        assert_eq!(order, ["brief-abc", "ref-interviews", "guide-api-limits"]);
        // Rendering is canonical — roles in reading order — and stable once
        // canonical: nothing is lost and nothing moves a second time.
        let canonical = Selection::parse(&s.render());
        assert_eq!(
            canonical.picks,
            s.ordered().into_iter().cloned().collect::<Vec<_>>()
        );
        assert_eq!(canonical.preamble, s.preamble);
        assert_eq!(canonical.extra, s.extra);
        assert_eq!(Selection::parse(&canonical.render()), canonical);
        assert_eq!(canonical.render(), s.render());
    }

    #[test]
    fn delivery_is_whole_where_it_can_be_and_names_everything_else() {
        let store = Store::open_in_memory().unwrap();
        doc(
            &store,
            "brief-abc",
            "# The brief\n\nFor night-shift admins.",
            json!({}),
        );
        doc(
            &store,
            "ref-old",
            "# Old\n\nretired",
            json!({ "archived": true }),
        );
        doc(
            &store,
            "ref-page",
            "<p>page</p>",
            json!({ "format": "html", "entry_path": "index.html", "source_name": "index.html" }),
        );
        doc(&store, "ref-huge", &"r".repeat(BUNDLE_CHARS * 2), json!({}));
        doc(
            &store,
            "guide-late",
            "# Late\n\nsmall but after the cap",
            json!({}),
        );
        select(
            &store,
            &[
                ("research", "ref-huge", ""),
                ("documents", "guide-late", ""),
                ("brief", "brief-abc", "what good is"),
                ("research", "ref-old", ""),
                ("research", "ref-page", ""),
                ("decisions", "note-never-synced", ""),
            ],
        );
        let d = deliver(&store, &item()).unwrap().unwrap();
        let by = |slug: &str| d.received.iter().find(|r| r.slug == slug).unwrap();
        assert_eq!(by("brief-abc").delivery, Delivery::Full);
        assert!(d.text.contains("For night-shift admins."));
        assert!(d.text.contains("_Selected because: what good is_"));
        assert!(d.text.find("### Brief") < d.text.find("### Research"));
        assert_eq!(by("ref-huge").delivery, Delivery::Partial);
        assert_eq!(by("ref-huge").reason, Some(Reason::Cap));
        assert_eq!(by("guide-late").delivery, Delivery::Omitted);
        assert_eq!(by("guide-late").reason, Some(Reason::Cap));
        assert_eq!(by("ref-old").reason, Some(Reason::Archived));
        assert_eq!(by("ref-page").reason, Some(Reason::Html));
        assert_eq!(by("note-never-synced").reason, Some(Reason::Absent));
        assert!(!d.text.contains("retired"));
        assert!(!d.text.contains("<p>page</p>"));
        assert!(d.text.len() <= BUNDLE_CHARS + 2_000);
        // Every document that did not arrive whole is named, with a way to
        // get it.
        let named: Vec<_> = d.missing.iter().map(|m| m.what.as_str()).collect();
        for slug in [
            "ref-huge",
            "guide-late",
            "ref-old",
            "ref-page",
            "note-never-synced",
        ] {
            assert!(
                named.iter().any(|w| w.contains(slug)),
                "{slug} not named: {named:?}"
            );
        }
        assert!(d.missing.iter().all(|m| m.fetch.is_some()));
    }

    #[test]
    fn an_item_without_a_selection_has_no_context_and_no_receipt() {
        let store = Store::open_in_memory().unwrap();
        assert!(deliver(&store, &item()).unwrap().is_none());
        assert!(prepare(&store, "s1", &item()).unwrap().is_none());
        assert!(store.context_receipts(&item().id).unwrap().is_empty());
    }

    #[test]
    fn attempts_share_a_revision_until_what_they_receive_differs() {
        let store = Store::open_in_memory().unwrap();
        doc(&store, "brief-abc", "# Brief\n\nv1", json!({}));
        doc(&store, "ref-a", "# A\n\na", json!({}));
        select(
            &store,
            &[("brief", "brief-abc", ""), ("research", "ref-a", "")],
        );

        let (_, first) = prepare(&store, "s1", &item()).unwrap().unwrap();
        assert_eq!(first.revision, 1);
        assert!(first.previous_revision.is_none());
        assert!(revision_line(&first).contains("first attempt"));

        // Nothing changed: same digest, same revision, and it says so.
        let (_, second) = prepare(&store, "s2", &item()).unwrap().unwrap();
        assert_eq!(second.revision, 1);
        assert_eq!(second.digest, first.digest);
        assert_eq!(second.previous_session.as_deref(), Some("s1"));
        assert!(second.changes.is_empty());
        assert!(revision_line(&second).contains("Unchanged"));

        // The brief is edited, research is swapped for a decision, and one
        // document is gone from the node.
        doc(&store, "brief-abc", "# Brief\n\nv2", json!({}));
        doc(&store, "note-adr", "# ADR\n\nwe chose x", json!({}));
        select(
            &store,
            &[
                ("brief", "brief-abc", ""),
                ("decisions", "note-adr", ""),
                ("constraints", "ref-a", ""),
                ("documents", "ref-gone", ""),
            ],
        );
        let (delivered, third) = prepare(&store, "s3", &item()).unwrap().unwrap();
        assert_eq!(third.revision, 2);
        assert!(delivered.text.contains("v2"));
        let kinds: Vec<_> = third
            .changes
            .iter()
            .map(|c| (c.kind.as_str(), c.slug.as_str()))
            .collect();
        assert!(kinds.contains(&("edited", "brief-abc")), "{kinds:?}");
        assert!(kinds.contains(&("added", "note-adr")), "{kinds:?}");
        assert!(kinds.contains(&("moved", "ref-a")), "{kinds:?}");
        assert!(kinds.contains(&("added", "ref-gone")), "{kinds:?}");
        assert_eq!(third.omissions().count(), 1);
        let line = revision_line(&third);
        assert!(
            line.contains("Since the previous attempt (revision 1)"),
            "{line}"
        );
        assert!(line.contains("`brief-abc` edited"), "{line}");

        // The selection is deleted: the next attempt still gets a receipt,
        // and it says what went.
        let store_slug = slug_for(&item().id);
        store
            .delete_document_change("n", "personal", &store_slug)
            .unwrap();
        let (delivered, fourth) = prepare(&store, "s4", &item()).unwrap().unwrap();
        assert!(delivered.text.is_empty());
        assert_eq!(fourth.revision, 3);
        assert!(fourth.changes.iter().all(|c| c.kind == "removed"));
        assert_eq!(fourth.changes.len(), 4);

        // Returning to what an earlier attempt received is that revision
        // again, not a new one.
        select(
            &store,
            &[("brief", "brief-abc", ""), ("research", "ref-a", "")],
        );
        doc(&store, "brief-abc", "# Brief\n\nv1", json!({}));
        let (_, fifth) = prepare(&store, "s5", &item()).unwrap().unwrap();
        assert_eq!(fifth.revision, 1);

        let all = store.context_receipts(&item().id).unwrap();
        assert_eq!(all.len(), 5);
        assert_eq!(
            store.context_receipt_for("s3").unwrap().unwrap().revision,
            2
        );
    }

    #[test]
    fn the_write_path_refuses_what_a_selection_cannot_hold() {
        let store = Store::open_in_memory().unwrap();
        let bus = Bus::new();
        let it = crate::corpus::work::create(
            &store,
            &bus,
            "n",
            crate::corpus::work::NewWork {
                channel: "personal".into(),
                project_id: None,
                title: "Triage".into(),
                body: String::new(),
                deps: vec![],
                priority: 0,
                discovered_from: None,
                discovered_by_session: None,
            },
        )
        .unwrap();
        let pick = |role: &str, slug: &str| PickInput {
            role: role.into(),
            slug: slug.into(),
            note: String::new(),
        };
        let bad = |picks| {
            write(
                &store,
                &bus,
                "n",
                &it.id,
                SelectionInput {
                    picks,
                    ..Default::default()
                },
            )
            .unwrap_err()
        };
        assert!(matches!(
            bad(vec![pick("gossip", "ref-a")]),
            ContextError::Role(_)
        ));
        assert!(matches!(
            bad(vec![pick("research", "../x")]),
            ContextError::Slug(_)
        ));
        assert!(matches!(
            bad(vec![pick("research", "ref-a"), pick("brief", "ref-a")]),
            ContextError::Duplicate(_)
        ));
        assert!(matches!(
            bad(vec![pick("research", &slug_for(&it.id))]),
            ContextError::SelfReference
        ));

        let view = write(
            &store,
            &bus,
            "n",
            &it.id,
            SelectionInput {
                picks: vec![pick("research", "ref-a")],
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(view.selection.picks.len(), 1);
        assert_eq!(view.resolved[0].reason, Some(Reason::Absent));
        let doc = store
            .doc_get("personal", &slug_for(&it.id))
            .unwrap()
            .unwrap();
        assert_eq!(doc.kind, KIND);

        // An edit against a selection that moved is refused, not merged.
        let err = write(
            &store,
            &bus,
            "n",
            &it.id,
            SelectionInput {
                picks: vec![],
                if_hash: Some("stale".into()),
                ..Default::default()
            },
        )
        .unwrap_err();
        assert!(matches!(err, ContextError::Conflict { .. }));
    }
}
