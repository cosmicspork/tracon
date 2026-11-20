//! Generated display names for the text the node writes: approval titles,
//! errors, readiness, orientation and CLI listings. The derivation and the
//! word lists are `proto::names`, shared with the interface, so a name the
//! node writes is the one the operator sees on the page.
//!
//! The operator can also name a record (`store::names`, replicated as
//! `entity_name`). The rules for such a name, and how text the operator or a
//! caller typed resolves to a record, are here.

pub use proto::names::{generated, kind};

use serde::Serialize;

use crate::store::names::{NameWrite, NamedRecord};
use crate::store::{Store, StoreError};
use crate::stream::Bus;

/// The kinds the operator can name. A task's title is its name and a node is
/// named in `node.toml`; an external agent client has no record here to hang
/// a name on.
pub const RENAMABLE: &[&str] = &[
    kind::SESSION,
    kind::REVIEW,
    kind::REPORT,
    kind::APPROVAL,
    kind::MANIFEST,
    kind::TRANSFER,
    kind::CHECK,
];

/// The kinds `resolve` looks through when it is not told one.
pub const RESOLVABLE: &[&str] = &[
    kind::SESSION,
    kind::REVIEW,
    kind::REPORT,
    kind::APPROVAL,
    kind::MANIFEST,
    kind::TASK,
    kind::TRANSFER,
    kind::CHECK,
];

/// The longest name, in characters.
pub const MAX_LEN: usize = 64;

/// The shortest id prefix that resolves on its own: shorter ones are a
/// UUIDv7's timestamp and match whole minutes of records.
pub const MIN_PREFIX: usize = 6;

/// An operator name, trimmed, or why it cannot be one: 1 to 64 characters on
/// one line with no control characters, and not something that reads as an
/// id (hex, dashes allowed, 8 characters or more), which would make an id
/// argument ambiguous.
pub fn validate(raw: &str) -> Result<String, String> {
    let name = raw.trim();
    if name.is_empty() {
        return Err("a name cannot be empty".into());
    }
    if name.chars().count() > MAX_LEN {
        return Err(format!("a name is at most {MAX_LEN} characters"));
    }
    if name.chars().any(char::is_control) {
        return Err("a name is one line with no control characters".into());
    }
    let hexish = name.chars().all(|c| c.is_ascii_hexdigit() || c == '-');
    if hexish && name.chars().filter(char::is_ascii_hexdigit).count() >= 8 {
        return Err("a name that is all hex reads as an id; add a word".into());
    }
    Ok(name.to_string())
}

/// A record as an answer names it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Hit {
    pub kind: String,
    pub id: String,
    /// The operator's name if it has one, else the generated name.
    pub name: String,
    pub channel: String,
    pub created_ms: i64,
}

/// What some typed text names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolved {
    One(Hit),
    None,
    /// Several records answer; the newest first.
    Many(Vec<Hit>),
}

/// How a record reads: the operator's name if it has one, else its generated
/// name.
pub fn display(store: &Store, kind: &str, id: &str) -> String {
    store
        .name_get(kind, id)
        .ok()
        .flatten()
        .map(|n| n.name)
        .unwrap_or_else(|| generated(kind, id))
}

fn hit(store: &Store, r: NamedRecord) -> Hit {
    Hit {
        name: display(store, &r.kind, &r.id),
        kind: r.kind,
        id: r.id,
        channel: r.channel,
        created_ms: r.created_ms,
    }
}

fn settle(store: &Store, mut found: Vec<NamedRecord>) -> Option<Resolved> {
    found.sort_by(|a, b| b.created_ms.cmp(&a.created_ms).then(a.id.cmp(&b.id)));
    found.dedup_by(|a, b| a.kind == b.kind && a.id == b.id);
    match found.len() {
        0 => None,
        1 => Some(Resolved::One(hit(store, found.remove(0)))),
        _ => Some(Resolved::Many(
            found.into_iter().map(|r| hit(store, r)).collect(),
        )),
    }
}

/// Resolve `q` to a record of `kind` (or of any kind when `None`), on
/// `channel` when given. In order, the first step that finds anything
/// answers: an operator name (ignoring case), a generated name (so the old
/// name still finds a renamed record), a full id, then an id prefix of at
/// least six characters.
pub fn resolve(
    store: &Store,
    kind: Option<&str>,
    q: &str,
    channel: Option<&str>,
) -> Result<Resolved, StoreError> {
    let q = q.trim();
    if q.is_empty() {
        return Ok(Resolved::None);
    }
    // A review's page shows reports too, so either kind looks at both.
    let kinds: Vec<&str> = match kind {
        Some(kind::REVIEW | kind::REPORT) => vec![kind::REVIEW, kind::REPORT],
        Some(k) => vec![k],
        None => RESOLVABLE.to_vec(),
    };
    let on_channel = |r: &NamedRecord| channel.is_none_or(|c| r.channel == c);

    // The operator's names. A name whose record this node does not hold
    // still answers: the record may be a peer's, not mirrored here.
    let mut named = Vec::new();
    for k in &kinds {
        for n in store.names_matching(k, q, channel)? {
            let created_ms = store
                .named_record(&n.kind, &n.id)?
                .map(|r| r.created_ms)
                .unwrap_or(n.updated_ms);
            named.push(NamedRecord {
                kind: n.kind,
                id: n.id,
                channel: n.channel,
                created_ms,
            });
        }
    }
    if let Some(r) = settle(store, named) {
        return Ok(r);
    }

    let lower = q.to_lowercase();
    let mut records: Vec<NamedRecord> = Vec::new();
    for k in &kinds {
        records.extend(store.named_records(k)?.into_iter().filter(on_channel));
    }
    let generated_hits: Vec<NamedRecord> = records
        .iter()
        .filter(|r| lower.starts_with(&format!("{}-", r.kind)))
        .filter(|r| generated(&r.kind, &r.id) == lower)
        .cloned()
        .collect();
    if let Some(r) = settle(store, generated_hits) {
        return Ok(r);
    }
    let exact: Vec<NamedRecord> = records.iter().filter(|r| r.id == q).cloned().collect();
    if let Some(r) = settle(store, exact) {
        return Ok(r);
    }
    if q.chars().count() >= MIN_PREFIX {
        let prefixed: Vec<NamedRecord> = records
            .iter()
            .filter(|r| r.id.to_lowercase().starts_with(&lower))
            .cloned()
            .collect();
        if let Some(r) = settle(store, prefixed) {
            return Ok(r);
        }
    }
    Ok(Resolved::None)
}

/// What an ambiguous answer says: how many records `q` names, and each by
/// name and id, so a caller can pick one.
pub fn ambiguous(q: &str, hits: &[Hit]) -> String {
    let listed: Vec<String> = hits
        .iter()
        .map(|h| format!("{} ({} {})", h.name, h.kind, h.id))
        .collect();
    format!(
        "{q} names {} records; use one of: {}",
        hits.len(),
        listed.join(", ")
    )
}

/// Why a rename or a revert was refused.
#[derive(Debug, thiserror::Error)]
pub enum RenameError {
    #[error("{0} records cannot be renamed")]
    NotRenamable(String),
    #[error("{0}")]
    Invalid(String),
    #[error("no {kind} {id} on this node")]
    Missing { kind: String, id: String },
    #[error("{} is already the name of {} {}", .0.name, .0.kind, .0.id)]
    Clash(crate::store::names::EntityName),
    #[error(transparent)]
    Store(#[from] StoreError),
}

/// What a rename or a revert left: the record's name now, and its generated
/// one.
#[derive(Debug, Clone, Serialize)]
pub struct Renamed {
    pub kind: String,
    pub id: String,
    pub channel: String,
    pub name: String,
    pub generated: String,
}

/// Name `kind`'s record `id` (an empty or absent `name` reverts it), as the
/// operator, from this node. A peer's mirrored record is named the same
/// way: ids are global, so the local write replicates to every node.
///
/// A launch manifest is known by its digest before any session launches
/// with it, so one this node has not recorded is named on `channel`, when
/// given and known here.
pub fn rename(
    store: &Store,
    bus: &Bus,
    site: &str,
    kind: &str,
    id: &str,
    name: Option<&str>,
    channel: Option<&str>,
) -> Result<Renamed, RenameError> {
    if !RENAMABLE.contains(&kind) {
        return Err(RenameError::NotRenamable(kind.to_string()));
    }
    let name = match name.map(str::trim).filter(|n| !n.is_empty()) {
        Some(n) => Some(validate(n).map_err(RenameError::Invalid)?),
        None => None,
    };
    let unrecorded_manifest = || -> Result<Option<NamedRecord>, StoreError> {
        let Some(channel) = channel.filter(|_| kind == kind::MANIFEST) else {
            return Ok(None);
        };
        if id.len() != 64 || !is_full_id(id) || store.channel_get(channel)?.is_none() {
            return Ok(None);
        }
        Ok(Some(NamedRecord {
            kind: kind.to_string(),
            id: id.to_string(),
            channel: channel.to_string(),
            created_ms: 0,
        }))
    };
    let record = match store.named_record(kind, id)? {
        Some(r) => r,
        None => unrecorded_manifest()?.ok_or_else(|| RenameError::Missing {
            kind: kind.to_string(),
            id: id.to_string(),
        })?,
    };
    match store.name_write(site, &record.channel, &record.kind, id, name.as_deref())? {
        NameWrite::Clash(holder) => return Err(RenameError::Clash(holder)),
        NameWrite::Unchanged => {}
        NameWrite::Written(change) => bus.publish(crate::stream::Frame::Changes {
            channel: record.channel.clone(),
            changes: vec![change],
        }),
    }
    let generated = generated(&record.kind, id);
    Ok(Renamed {
        name: name.unwrap_or_else(|| generated.clone()),
        kind: record.kind,
        id: id.to_string(),
        channel: record.channel,
        generated,
    })
}

/// Whether `id` is a whole id as the node mints them: a UUID or 64 hex
/// characters. Anything shorter is a prefix, which names nothing.
pub fn is_full_id(id: &str) -> bool {
    let hex = |s: &str| s.bytes().all(|b| b.is_ascii_hexdigit());
    match id.len() {
        64 => hex(id),
        36 => {
            let parts: Vec<&str> = id.split('-').collect();
            parts.iter().map(|p| p.len()).eq([8, 4, 4, 4, 12]) && parts.iter().all(|p| hex(p))
        }
        _ => false,
    }
}

/// A record someone named by id, read back: its generated name for a whole
/// id, or the text as given (cut to 12 characters) for a prefix or anything
/// else, which names nothing.
pub fn handle(kind: &str, given: &str) -> String {
    let given = given.trim();
    if is_full_id(given) {
        generated(kind, given)
    } else {
        given.chars().take(12).collect()
    }
}

pub fn session(id: &str) -> String {
    generated(kind::SESSION, id)
}

pub fn task(id: &str) -> String {
    generated(kind::TASK, id)
}

pub fn manifest(digest: &str) -> String {
    generated(kind::MANIFEST, digest)
}

/// A node by its own name when there is one, else its generated `node-…` name.
/// An id that is not a node id (64 hex characters) is already something to
/// read, and reads as given.
pub fn node(id: &str, name: Option<&str>) -> String {
    match name.map(str::trim).filter(|n| !n.is_empty()) {
        Some(name) => name.to_string(),
        None if id.len() == 64 && is_full_id(id) => generated(kind::NODE, id),
        None => id.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_name_is_one_short_line_that_does_not_read_as_an_id() {
        assert_eq!(validate("  Login flow ").unwrap(), "Login flow");
        assert_eq!(validate(&"é".repeat(64)).unwrap().chars().count(), 64);
        for bad in [
            "",
            "   ",
            &"x".repeat(65),
            "two\nlines",
            "tab\there",
            "deadbeef",
            "019a9e5c-7f20-7a3b-8c1d-2e3f4a5b6c7d",
            "DEADBEEF00",
        ] {
            assert!(validate(bad).is_err(), "{bad:?} should be refused");
        }
        // Short hex, or hex with a word, is a name.
        assert!(validate("cafe").is_ok());
        assert!(validate("deadbeef fix").is_ok());
    }

    #[test]
    fn a_whole_id_is_named_and_a_prefix_is_read_as_given() {
        let uuid = "019a9e5c-7f20-7a3b-8c1d-2e3f4a5b6c7d";
        assert!(is_full_id(uuid));
        assert_eq!(handle(kind::SESSION, uuid), "session-cubic-briar-06");
        assert!(!is_full_id("019a9e5c"));
        assert_eq!(handle(kind::TASK, " 0b0e29c4 "), "0b0e29c4");
        assert!(is_full_id(&"ab".repeat(32)));
        assert!(!is_full_id(&"zz".repeat(32)));
        assert_eq!(node("x", Some("bazzite")), "bazzite");
        assert!(node(&"ab".repeat(32), Some(" ")).starts_with("node-"));
        assert_eq!(node("peer-b", None), "peer-b");
    }
}
