//! The one path that writes `node.toml`.
//!
//! An edit is a list of typed operations applied to the file as it is, in
//! place: the comments, key order and formatting the operator wrote stay,
//! and only the values an operation names change. Under one lock, the edit
//!
//! 1. reads the file and, when the caller says which version it decided
//!    against, refuses if the file is no longer that version;
//! 2. applies the operations;
//! 3. loads the result as a `Config`, with every check a load makes, and
//!    refuses anything that would not load;
//! 4. records who changed which keys in the audit log (`config_audit`), and
//!    only then replaces the file, atomically;
//! 5. tells the operator's devices what changed, by whom.
//!
//! So nothing reaches the file without an audit row, and nothing the node
//! could not start with reaches it at all.

use std::fmt;
use std::path::Path;
use std::sync::Arc;

use sha2::{Digest, Sha256};
use toml_edit::{ArrayOfTables, DocumentMut, Item, Table, TableLike};

use super::Config;
use crate::config_schema::{self, Apply};
use crate::store::{config_audit::ConfigAuditRow, Store};

/// What the audit log shows in place of a secret key's value.
pub const REDACTED: &str = "[redacted]";

/// How many times `update` decides again after another edit landed between
/// its read and its write.
const RETRIES: usize = 5;

/// One step along a key path: a table key, or an entry of an array.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Seg {
    Key(String),
    Index(usize),
}

/// Where a value sits in the file: `mesh.hub_url`, `repo[2].egress`,
/// `providers."my.provider".models`.
#[derive(Debug, Clone, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct KeyPath(pub Vec<Seg>);

impl KeyPath {
    /// A path of table keys only.
    pub fn keys(keys: &[&str]) -> Self {
        Self(keys.iter().map(|k| Seg::Key(k.to_string())).collect())
    }

    pub fn key(mut self, key: &str) -> Self {
        self.0.push(Seg::Key(key.to_string()));
        self
    }

    pub fn index(mut self, index: usize) -> Self {
        self.0.push(Seg::Index(index));
        self
    }

    /// Read a path as `Display` writes it.
    pub fn parse(text: &str) -> Result<Self, String> {
        let bad = || format!("{text:?} is not a key path");
        let mut segs = Vec::new();
        let mut chars = text.chars().peekable();
        loop {
            let mut key = String::new();
            if chars.peek() == Some(&'"') {
                chars.next();
                loop {
                    match chars.next().ok_or_else(bad)? {
                        '"' => break,
                        '\\' => key.push(chars.next().ok_or_else(bad)?),
                        c => key.push(c),
                    }
                }
            } else {
                while let Some(&c) = chars.peek() {
                    if c == '.' || c == '[' {
                        break;
                    }
                    if !bare(c) {
                        return Err(bad());
                    }
                    key.push(c);
                    chars.next();
                }
                if key.is_empty() {
                    return Err(bad());
                }
            }
            segs.push(Seg::Key(key));
            while chars.peek() == Some(&'[') {
                chars.next();
                let mut digits = String::new();
                loop {
                    match chars.next().ok_or_else(bad)? {
                        ']' => break,
                        c => digits.push(c),
                    }
                }
                segs.push(Seg::Index(digits.parse().map_err(|_| bad())?));
            }
            match chars.next() {
                None => return Ok(Self(segs)),
                Some('.') => continue,
                Some(_) => return Err(bad()),
            }
        }
    }

    /// The table keys alone, which is how the schema names a key.
    pub fn schema_key(&self) -> Vec<&str> {
        self.0
            .iter()
            .filter_map(|seg| match seg {
                Seg::Key(k) => Some(k.as_str()),
                Seg::Index(_) => None,
            })
            .collect()
    }

    fn split_last(&self) -> Result<(&Seg, &[Seg]), EditError> {
        self.0
            .split_last()
            .ok_or_else(|| EditError::Op("an empty key path names nothing".into()))
    }

    fn prefix(segs: &[Seg]) -> Self {
        Self(segs.to_vec())
    }
}

fn bare(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '-'
}

impl fmt::Display for KeyPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (n, seg) in self.0.iter().enumerate() {
            match seg {
                Seg::Index(i) => write!(f, "[{i}]")?,
                Seg::Key(k) => {
                    if n > 0 {
                        f.write_str(".")?;
                    }
                    if !k.is_empty() && k.chars().all(bare) {
                        f.write_str(k)?;
                    } else {
                        write!(f, "\"{}\"", k.replace('\\', "\\\\").replace('"', "\\\""))?;
                    }
                }
            }
        }
        Ok(())
    }
}

/// One change to `node.toml`.
#[derive(Debug, Clone, PartialEq)]
pub enum Op {
    /// Set the value at `key`, creating the tables above it. A table is
    /// merged key by key over the one there (keys it does not name are
    /// removed), so what it keeps keeps its comments. A key ending in an
    /// index replaces that entry of an array.
    Set { key: KeyPath, value: toml::Value },
    /// Remove the key, or with an index the array entry. A key that is not
    /// there is already removed.
    Remove { key: KeyPath },
    /// Add an entry to an array of tables (`[[repo]]`, `[[service]]`, a
    /// provider's models): at `index`, or after the last.
    Insert {
        array: KeyPath,
        index: Option<usize>,
        entry: toml::Table,
    },
}

impl Op {
    pub fn set(key: KeyPath, value: impl Into<toml::Value>) -> Self {
        Op::Set {
            key,
            value: value.into(),
        }
    }

    /// Set `key` to what `value` serializes as.
    pub fn set_serialized<T: serde::Serialize + ?Sized>(
        key: KeyPath,
        value: &T,
    ) -> Result<Self, EditError> {
        let value =
            toml::Value::try_from(value).map_err(|e| EditError::Op(format!("{key}: {e}")))?;
        Ok(Op::Set { key, value })
    }

    pub fn remove(key: KeyPath) -> Self {
        Op::Remove { key }
    }

    pub fn append(array: KeyPath, entry: toml::Table) -> Self {
        Op::Insert {
            array,
            index: None,
            entry,
        }
    }

    /// The ops that turn `before` into `after` at `at`, key by key: only a
    /// key whose value differs is touched, so a table edited this way keeps
    /// every other key as the file has it.
    pub fn changes<T: serde::Serialize>(
        at: &KeyPath,
        before: &T,
        after: &T,
    ) -> Result<Vec<Op>, EditError> {
        let table =
            |v: &T| toml::Table::try_from(v).map_err(|e| EditError::Op(format!("{at}: {e}")));
        let mut ops = Vec::new();
        table_changes(at, &table(before)?, &table(after)?, &mut ops);
        Ok(ops)
    }
}

impl Op {
    /// The ops that make the `[[repo]]` table read `after`, given `before` as
    /// the file loads it: entry by entry and key by key, so whatever an entry
    /// keeps keeps its comments.
    pub fn replace_repos(
        before: &[super::Repo],
        after: &[super::Repo],
    ) -> Result<Vec<Op>, EditError> {
        replace_entries(&KeyPath::keys(&["repo"]), before, after)
    }

    /// Make entry `index` of `[[repo]]` read `entry`, or add it after the
    /// last when `index` is `None`.
    pub fn put_repo(
        before: &[super::Repo],
        index: Option<usize>,
        entry: &super::Repo,
    ) -> Result<Vec<Op>, EditError> {
        let array = KeyPath::keys(&["repo"]);
        match index.and_then(|i| before.get(i).map(|was| (i, was))) {
            Some((i, was)) => Op::changes(&array.index(i), was, entry),
            None => Ok(vec![Op::append(
                array,
                super::without_defaults(entry).map_err(EditError::Op)?,
            )]),
        }
    }
}

fn replace_entries<T: serde::Serialize + Default>(
    array: &KeyPath,
    before: &[T],
    after: &[T],
) -> Result<Vec<Op>, EditError> {
    let mut ops = Vec::new();
    for (i, (was, now)) in before.iter().zip(after).enumerate() {
        ops.extend(Op::changes(&array.clone().index(i), was, now)?);
    }
    for now in after.iter().skip(before.len()) {
        ops.push(Op::append(
            array.clone(),
            super::without_defaults(now).map_err(EditError::Op)?,
        ));
    }
    // From the end, so each index still names the entry it did.
    for i in (after.len()..before.len()).rev() {
        ops.push(Op::remove(array.clone().index(i)));
    }
    Ok(ops)
}

fn table_changes(at: &KeyPath, before: &toml::Table, after: &toml::Table, ops: &mut Vec<Op>) {
    for key in before.keys().filter(|k| !after.contains_key(*k)) {
        ops.push(Op::remove(at.clone().key(key)));
    }
    for (key, now) in after {
        match (before.get(key), now) {
            (Some(was), now) if was == now => {}
            (Some(toml::Value::Table(was)), toml::Value::Table(now)) => {
                table_changes(&at.clone().key(key), was, now, ops)
            }
            _ => ops.push(Op::set(at.clone().key(key), now.clone())),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum EditError {
    /// The file is not the version the edit was decided against.
    #[error("node.toml changed since it was read; it is now {current}")]
    Conflict { current: String },
    #[error("node.toml does not parse, so it will not be rewritten: {0}")]
    Unparseable(String),
    /// An operation does not fit the file: an index past the end, a key
    /// below a value that is not a table.
    #[error("{0}")]
    Op(String),
    /// The result would not load.
    #[error("{0}")]
    Invalid(String),
    #[error("the change could not be audited, so it was not written: {0}")]
    Audit(String),
    #[error("node.toml could not be written: {0}")]
    Io(#[from] std::io::Error),
}

impl From<EditError> for String {
    fn from(error: EditError) -> Self {
        error.to_string()
    }
}

/// The file as one reader saw it: what it loads as, and the hash an edit
/// decided against it passes back.
pub struct Snapshot {
    pub config: Config,
    pub hash: String,
}

/// One key an edit changed, with its values as the file held them.
#[derive(Debug, Clone, PartialEq)]
pub struct Change {
    pub key: KeyPath,
    pub before: Option<toml::Value>,
    pub after: Option<toml::Value>,
}

pub struct EditOutcome {
    pub old_hash: String,
    /// The file's hash now: `old_hash` when nothing changed.
    pub new_hash: String,
    pub changes: Vec<Change>,
    /// Whether a change waits for a restart (`config_schema::Apply`).
    pub restart_required: bool,
    /// The file as it now loads.
    pub config: Config,
    /// The audit row, when anything was written.
    pub audit_id: Option<String>,
    /// The operator's push, sent on the runtime the edit ran on. Detached;
    /// a short-lived process (the CLI) awaits it before exiting.
    pub notified: Option<tokio::task::JoinHandle<()>>,
}

impl EditOutcome {
    pub fn wrote(&self) -> bool {
        self.audit_id.is_some()
    }

    pub fn changed(&self) -> Vec<String> {
        self.changes.iter().map(|c| c.key.to_string()).collect()
    }

    /// Let the push finish, for a process about to exit.
    pub async fn settle(self) {
        if let Some(push) = self.notified {
            let _ = push.await;
        }
    }
}

/// The SHA-256 of the file's bytes, which is the version an edit names.
/// A missing file is the empty one.
pub fn hash(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

pub fn snapshot() -> Result<Snapshot, EditError> {
    snapshot_at(&Config::config_path())
}

pub fn snapshot_at(path: &Path) -> Result<Snapshot, EditError> {
    let bytes = read(path)?;
    let text = String::from_utf8(bytes).map_err(|_| unparseable(path, "not UTF-8"))?;
    // The retired harness is migrated as a load migrates it, in memory: the
    // file itself is rewritten by the next start, not by a reader.
    let loaded = match crate::legacy::migrate_config(&text) {
        Some(migration) => Config::from_toml(&migration.text, path),
        None => Config::from_toml(&text, path),
    };
    Ok(Snapshot {
        config: loaded.map_err(EditError::Unparseable)?,
        hash: hash(text.as_bytes()),
    })
}

/// Edit `node.toml` at its usual place. `expected_hash` is the version the
/// caller decided against (`Snapshot::hash`); `None` applies the operations
/// to whatever the file is now. `actor` is who, in words for the log and the
/// push: "Settings", "tracon mesh init".
pub fn edit(
    store: &Arc<Store>,
    ops: &[Op],
    expected_hash: Option<&str>,
    actor: &str,
) -> Result<EditOutcome, EditError> {
    edit_at(&Config::config_path(), store, ops, expected_hash, actor)
}

/// Read, decide and edit, deciding again if another edit lands in between.
/// For a writer whose operations depend on what the file holds now: the
/// decision sees the file the edit applies to, as it did when every writer
/// held the lock across its load and save.
pub fn update<E: From<EditError>>(
    store: &Arc<Store>,
    actor: &str,
    plan: impl FnMut(&Config) -> Result<Vec<Op>, E>,
) -> Result<EditOutcome, E> {
    update_at(&Config::config_path(), store, actor, plan)
}

pub fn update_at<E: From<EditError>>(
    path: &Path,
    store: &Arc<Store>,
    actor: &str,
    mut plan: impl FnMut(&Config) -> Result<Vec<Op>, E>,
) -> Result<EditOutcome, E> {
    let mut attempt = 0;
    loop {
        let snapshot = snapshot_at(path)?;
        let ops = plan(&snapshot.config)?;
        match edit_at(path, store, &ops, Some(&snapshot.hash), actor) {
            Err(EditError::Conflict { .. }) if attempt < RETRIES => attempt += 1,
            outcome => return Ok(outcome?),
        }
    }
}

pub fn edit_at(
    path: &Path,
    store: &Arc<Store>,
    ops: &[Op],
    expected_hash: Option<&str>,
    actor: &str,
) -> Result<EditOutcome, EditError> {
    let held = lock();
    let bytes = read(path)?;
    let old_hash = hash(&bytes);
    if expected_hash.is_some_and(|expected| expected != old_hash) {
        return Err(EditError::Conflict { current: old_hash });
    }
    let text = String::from_utf8(bytes).map_err(|_| unparseable(path, "not UTF-8"))?;
    let next = apply_to_text(&text, ops).map_err(|error| match error {
        EditError::Unparseable(e) => unparseable(path, &e),
        other => other,
    })?;
    let config = Config::from_toml(&next, path).map_err(EditError::Invalid)?;
    let changes = diff(&text, &next);
    if changes.is_empty() {
        return Ok(EditOutcome {
            new_hash: old_hash.clone(),
            old_hash,
            changes,
            restart_required: false,
            config,
            audit_id: None,
            notified: None,
        });
    }

    let new_hash = hash(next.as_bytes());
    let row = ConfigAuditRow {
        id: uuid::Uuid::now_v7().to_string(),
        at_ms: crate::store::now_ms(),
        actor: actor.to_string(),
        old_hash: old_hash.clone(),
        new_hash: new_hash.clone(),
        changes: serde_json::Value::Array(changes.iter().map(audit_entry).collect()),
    };
    store
        .config_audit_insert(&row)
        .map_err(|e| EditError::Audit(e.to_string()))?;
    let written = path
        .parent()
        .filter(|dir| !dir.as_os_str().is_empty())
        .map_or(Ok(()), std::fs::create_dir_all)
        .and_then(|()| super::write_replacing(path, next.as_bytes()));
    if let Err(error) = written {
        let _ = store.config_audit_delete(&row.id);
        return Err(error.into());
    }
    drop(held);

    let restart_required = changes
        .iter()
        .any(|c| config_schema::lookup(&c.key.schema_key()).apply == Apply::Restart);
    let keys: Vec<String> = changes.iter().map(|c| c.key.to_string()).collect();
    tracing::info!(actor, keys = %keys.join(", "), old = %old_hash, new = %new_hash, "node.toml changed");
    let notified = announce(store, &config, &row.id, &keys, actor);
    Ok(EditOutcome {
        old_hash,
        new_hash,
        changes,
        restart_required,
        config,
        audit_id: Some(row.id),
        notified,
    })
}

/// What `ops` make of `text`, without writing or checking it.
pub fn apply_to_text(text: &str, ops: &[Op]) -> Result<String, EditError> {
    let mut doc: DocumentMut = text
        .parse()
        .map_err(|e: toml_edit::TomlError| EditError::Unparseable(e.to_string()))?;
    for op in ops {
        apply(&mut doc, op)?;
    }
    Ok(doc.to_string())
}

/// Held for the span of one edit's read, write and audit. A `std` guard, so
/// nothing can hold it across an `.await`.
fn lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn read(path: &Path) -> Result<Vec<u8>, EditError> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(bytes),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(e.into()),
    }
}

fn unparseable(path: &Path, why: &str) -> EditError {
    EditError::Unparseable(format!("{}: {why}", path.display()))
}

fn announce(
    store: &Arc<Store>,
    config: &Config,
    audit_id: &str,
    keys: &[String],
    actor: &str,
) -> Option<tokio::task::JoinHandle<()>> {
    let Ok(runtime) = tokio::runtime::Handle::try_current() else {
        tracing::warn!("node.toml changed outside a runtime; no push was sent");
        return None;
    };
    const SHOWN: usize = 6;
    let mut named = keys
        .iter()
        .take(SHOWN)
        .cloned()
        .collect::<Vec<_>>()
        .join(", ");
    if keys.len() > SHOWN {
        named.push_str(&format!(" and {} more", keys.len() - SHOWN));
    }
    let body = format!("{named} by {actor}");
    let (store, config, id) = (store.clone(), config.clone(), audit_id.to_string());
    Some(runtime.spawn(async move {
        crate::notify::send_operator(
            &store,
            &config,
            &id,
            "node.toml changed".into(),
            body,
            "/settings".into(),
            &[],
        )
        .await;
    }))
}

fn audit_entry(change: &Change) -> serde_json::Value {
    let secret = config_schema::lookup(&change.key.schema_key()).secret;
    let shown = |value: &Option<toml::Value>| match value {
        None => serde_json::Value::Null,
        Some(_) if secret => serde_json::Value::String(REDACTED.into()),
        Some(value) => serde_json::to_value(value).unwrap_or_default(),
    };
    serde_json::json!({
        "key": change.key.to_string(),
        "before": shown(&change.before),
        "after": shown(&change.after),
    })
}

/// The keys whose values differ between two texts of the file, down to the
/// values themselves: a table that appears or goes is each of its keys. An
/// array of tables whose length changed is one change at the array.
fn diff(before: &str, after: &str) -> Vec<Change> {
    let parse = |text: &str| toml::from_str::<toml::Table>(text).unwrap_or_default();
    let (before, after) = (parse(before), parse(after));
    let mut out = Vec::new();
    diff_tables(&KeyPath::default(), &before, &after, &mut out);
    out
}

fn diff_tables(at: &KeyPath, before: &toml::Table, after: &toml::Table, out: &mut Vec<Change>) {
    let keys = before
        .keys()
        .chain(after.keys().filter(|k| !before.contains_key(*k)));
    for key in keys {
        diff_values(&at.clone().key(key), before.get(key), after.get(key), out);
    }
}

fn diff_values(
    at: &KeyPath,
    before: Option<&toml::Value>,
    after: Option<&toml::Value>,
    out: &mut Vec<Change>,
) {
    match (before, after) {
        (Some(was), Some(now)) if was == now => {}
        (Some(toml::Value::Table(was)), Some(toml::Value::Table(now))) => {
            diff_tables(at, was, now, out)
        }
        (None, Some(toml::Value::Table(now))) => diff_tables(at, &toml::Table::new(), now, out),
        (Some(toml::Value::Table(was)), None) => diff_tables(at, was, &toml::Table::new(), out),
        (Some(toml::Value::Array(was)), Some(toml::Value::Array(now)))
            if was.len() == now.len() && tables_of(was).is_some() && tables_of(now).is_some() =>
        {
            for (i, (was, now)) in was.iter().zip(now).enumerate() {
                diff_values(&at.clone().index(i), Some(was), Some(now), out);
            }
        }
        _ => out.push(Change {
            key: at.clone(),
            before: before.cloned(),
            after: after.cloned(),
        }),
    }
}

// --- applying operations ---------------------------------------------------

/// Where a walk down the document has got to: a table (or an entry of an
/// array of tables), or the item under a key.
enum Cursor<'a> {
    Table(&'a mut dyn TableLike),
    Item(&'a mut Item),
}

impl<'a> Cursor<'a> {
    fn table(self) -> Option<&'a mut dyn TableLike> {
        match self {
            Cursor::Table(table) => Some(table),
            Cursor::Item(item) => item.as_table_like_mut(),
        }
    }
}

fn not_table(at: &KeyPath) -> EditError {
    EditError::Op(format!("`{at}` is not a table"))
}

fn not_array(at: &KeyPath) -> EditError {
    EditError::Op(format!("`{at}` is not an array of tables"))
}

fn no_entry(at: &KeyPath, index: usize) -> EditError {
    EditError::Op(format!("`{at}` has no entry {index}"))
}

fn implicit_table() -> Item {
    let mut table = Table::new();
    table.set_implicit(true);
    Item::Table(table)
}

/// Walk to `path`, creating the tables it names when `create` says to.
/// `None` when something on the way is missing and `create` is off.
fn walk<'a>(
    doc: &'a mut DocumentMut,
    path: &[Seg],
    create: bool,
) -> Result<Option<Cursor<'a>>, EditError> {
    let mut cursor = Cursor::Table(doc.as_table_mut() as &mut dyn TableLike);
    for (n, seg) in path.iter().enumerate() {
        cursor = match seg {
            Seg::Key(key) => {
                let table = cursor
                    .table()
                    .ok_or_else(|| not_table(&KeyPath::prefix(&path[..n])))?;
                if !table.contains_key(key) {
                    if !create {
                        return Ok(None);
                    }
                    table.insert(key, implicit_table());
                }
                Cursor::Item(table.get_mut(key).expect("present"))
            }
            Seg::Index(index) => {
                let here = KeyPath::prefix(&path[..n]);
                let Cursor::Item(item) = cursor else {
                    return Err(not_array(&here));
                };
                Cursor::Table(entry_mut(item, *index, &here)?)
            }
        };
    }
    Ok(Some(cursor))
}

fn entry_mut<'a>(
    item: &'a mut Item,
    index: usize,
    at: &KeyPath,
) -> Result<&'a mut dyn TableLike, EditError> {
    match item {
        Item::ArrayOfTables(array) => array
            .get_mut(index)
            .map(|t| t as &mut dyn TableLike)
            .ok_or_else(|| no_entry(at, index)),
        Item::Value(toml_edit::Value::Array(array)) => array
            .get_mut(index)
            .ok_or_else(|| no_entry(at, index))?
            .as_inline_table_mut()
            .map(|t| t as &mut dyn TableLike)
            .ok_or_else(|| not_array(at)),
        _ => Err(not_array(at)),
    }
}

fn apply(doc: &mut DocumentMut, op: &Op) -> Result<(), EditError> {
    match op {
        Op::Set { key, value } => {
            let (last, parent) = key.split_last()?;
            let here = KeyPath::prefix(parent);
            let cursor = walk(doc, parent, true)?.expect("created");
            match last {
                Seg::Key(name) => {
                    let table = cursor.table().ok_or_else(|| not_table(&here))?;
                    match table.get_mut(name) {
                        Some(item) if !item.is_none() => set_item(item, value),
                        _ => {
                            table.insert(name, to_item(value));
                        }
                    }
                }
                Seg::Index(index) => {
                    let Cursor::Item(item) = cursor else {
                        return Err(not_array(&here));
                    };
                    match item {
                        Item::ArrayOfTables(array) => {
                            let entry = array
                                .get_mut(*index)
                                .ok_or_else(|| no_entry(&here, *index))?;
                            let toml::Value::Table(new) = value else {
                                return Err(EditError::Op(format!(
                                    "an entry of `{here}` is a table"
                                )));
                            };
                            reconcile(entry, new);
                        }
                        Item::Value(toml_edit::Value::Array(array)) => {
                            let slot = array
                                .get_mut(*index)
                                .ok_or_else(|| no_entry(&here, *index))?;
                            set_value(slot, value);
                        }
                        _ => return Err(not_array(&here)),
                    }
                }
            }
        }
        Op::Remove { key } => {
            let (last, parent) = key.split_last()?;
            let here = KeyPath::prefix(parent);
            let Some(cursor) = walk(doc, parent, false)? else {
                return Ok(());
            };
            match last {
                Seg::Key(name) => {
                    if let Some(table) = cursor.table() {
                        table.remove(name);
                    }
                }
                Seg::Index(index) => {
                    let Cursor::Item(item) = cursor else {
                        return Err(not_array(&here));
                    };
                    match item {
                        Item::ArrayOfTables(array) if *index < array.len() => {
                            array.remove(*index);
                        }
                        Item::Value(toml_edit::Value::Array(array)) if *index < array.len() => {
                            array.remove(*index);
                        }
                        Item::ArrayOfTables(_) | Item::Value(toml_edit::Value::Array(_)) => {
                            return Err(no_entry(&here, *index))
                        }
                        _ => return Err(not_array(&here)),
                    }
                }
            }
        }
        Op::Insert {
            array,
            index,
            entry,
        } => {
            let (Seg::Key(name), parent) = array.split_last()? else {
                return Err(EditError::Op(format!("`{array}` does not name an array")));
            };
            let here = KeyPath::prefix(parent);
            let table = walk(doc, parent, true)?
                .expect("created")
                .table()
                .ok_or_else(|| not_table(&here))?;
            if !table.contains_key(name) {
                table.insert(name, Item::ArrayOfTables(ArrayOfTables::new()));
            }
            match table.get_mut(name).expect("present") {
                Item::ArrayOfTables(tables) => {
                    let new = to_table(entry);
                    match index {
                        None => tables.push(new),
                        Some(at) if *at <= tables.len() => {
                            let mut all: Vec<Table> = tables.iter().cloned().collect();
                            all.insert(*at, new);
                            let mut rebuilt = ArrayOfTables::new();
                            for table in all {
                                rebuilt.push(table);
                            }
                            *tables = rebuilt;
                        }
                        Some(at) => return Err(no_entry(array, *at)),
                    }
                }
                Item::Value(toml_edit::Value::Array(values)) => {
                    let new = to_value(&toml::Value::Table(entry.clone()));
                    match index {
                        None => values.push(new),
                        Some(at) if *at <= values.len() => values.insert(*at, new),
                        Some(at) => return Err(no_entry(array, *at)),
                    }
                }
                _ => return Err(not_array(array)),
            }
        }
    }
    Ok(())
}

/// Write `value` into `item`, keeping what can be kept: a table merges key
/// by key, an array of tables entry by entry, and a value keeps the
/// comment beside it.
fn set_item(item: &mut Item, value: &toml::Value) {
    if let toml::Value::Table(new) = value {
        if let Some(table) = item.as_table_like_mut() {
            reconcile(table, new);
            return;
        }
    }
    if let (Item::ArrayOfTables(tables), toml::Value::Array(new)) = (&mut *item, value) {
        if let Some(new) = tables_of(new) {
            for (i, entry) in new.iter().enumerate() {
                match tables.get_mut(i) {
                    Some(table) => reconcile(table, entry),
                    None => tables.push(to_table(entry)),
                }
            }
            while tables.len() > new.len() {
                tables.remove(tables.len() - 1);
            }
            return;
        }
    }
    if let Item::Value(old) = item {
        set_value(old, value);
        return;
    }
    *item = to_item(value);
}

fn set_value(old: &mut toml_edit::Value, value: &toml::Value) {
    if let (Some(table), toml::Value::Table(new)) = (old.as_inline_table_mut(), value) {
        reconcile(table, new);
        return;
    }
    if plain(old).as_ref() == Some(value) {
        return;
    }
    let decor = old.decor().clone();
    *old = to_value(value);
    *old.decor_mut() = decor;
}

/// Make `table` hold exactly `new`'s keys, touching only those that differ.
fn reconcile(table: &mut dyn TableLike, new: &toml::Table) {
    let gone: Vec<String> = table
        .iter()
        .map(|(key, _)| key.to_string())
        .filter(|key| !new.contains_key(key))
        .collect();
    for key in gone {
        table.remove(&key);
    }
    for (key, value) in new {
        match table.get_mut(key) {
            Some(item) if !item.is_none() => set_item(item, value),
            _ => {
                table.insert(key, to_item(value));
            }
        }
    }
}

/// What a value in the document is, without its formatting.
fn plain(value: &toml_edit::Value) -> Option<toml::Value> {
    let mut bare = value.clone();
    bare.decor_mut().clear();
    toml::from_str::<toml::Table>(&format!("v = {bare}"))
        .ok()?
        .remove("v")
}

fn tables_of(values: &[toml::Value]) -> Option<Vec<&toml::Table>> {
    if values.is_empty() {
        return None;
    }
    values.iter().map(toml::Value::as_table).collect()
}

fn to_item(value: &toml::Value) -> Item {
    match value {
        toml::Value::Table(table) => Item::Table(to_table(table)),
        toml::Value::Array(values) => match tables_of(values) {
            Some(tables) => {
                let mut array = ArrayOfTables::new();
                for table in tables {
                    array.push(to_table(table));
                }
                Item::ArrayOfTables(array)
            }
            None => Item::Value(to_value(value)),
        },
        _ => Item::Value(to_value(value)),
    }
}

fn to_table(table: &toml::Table) -> Table {
    let mut out = Table::new();
    for (key, value) in table {
        out.insert(key, to_item(value));
    }
    out
}

fn to_value(value: &toml::Value) -> toml_edit::Value {
    match value {
        toml::Value::String(s) => s.as_str().into(),
        toml::Value::Integer(i) => (*i).into(),
        toml::Value::Float(f) => (*f).into(),
        toml::Value::Boolean(b) => (*b).into(),
        toml::Value::Datetime(d) => (*d).into(),
        toml::Value::Array(values) => {
            let mut array = toml_edit::Array::new();
            for value in values {
                array.push(to_value(value));
            }
            toml_edit::Value::Array(array)
        }
        toml::Value::Table(table) => {
            let mut inline = toml_edit::InlineTable::new();
            for (key, value) in table {
                inline.insert(key, to_value(value));
            }
            toml_edit::Value::InlineTable(inline)
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::config::Repo;

    /// A file the way an operator writes one: comments above and beside
    /// keys, sections in their own order, a table nobody has a struct for.
    const COMMENTED: &str = r#"# This node's configuration. Hand-written.
node_name = "kept"   # what the mesh calls it

# Sessions get more room here.
[session]
budget_tokens = 5    # small on purpose
default_channel = "work"

[harness]
# OpenCode until Claude is set up.
id = "opencode"

[[repo]]
# The blog builds with Zola.
path = "/srv/blog"
egress = ["github"]   # releases come from GitHub

[[repo]]
path = "/srv/notes"
checks = []
"#;

    fn store() -> Arc<Store> {
        Arc::new(Store::open_in_memory().unwrap())
    }

    fn file(text: &str) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("node.toml");
        std::fs::write(&path, text).unwrap();
        (dir, path)
    }

    fn read(path: &Path) -> String {
        std::fs::read_to_string(path).unwrap()
    }

    fn set(key: &str, value: impl Into<toml::Value>) -> Op {
        Op::set(KeyPath::parse(key).unwrap(), value)
    }

    /// Every comment and the order of every section survive, and a changed
    /// value keeps the comment beside it.
    fn assert_kept(text: &str) {
        for line in [
            "# This node's configuration. Hand-written.",
            "# what the mesh calls it",
            "# Sessions get more room here.",
            "# OpenCode until Claude is set up.",
            "# The blog builds with Zola.",
            "# releases come from GitHub",
            "# small on purpose",
        ] {
            assert!(text.contains(line), "lost {line:?}:\n{text}");
        }
        let at = |needle: &str| {
            text.find(needle)
                .unwrap_or_else(|| panic!("{needle} missing:\n{text}"))
        };
        assert!(at("node_name") < at("[session]"), "{text}");
        assert!(at("[session]") < at("[harness]"), "{text}");
        assert!(at("[harness]") < at("[[repo]]"), "{text}");
        assert!(at("/srv/blog") < at("/srv/notes"), "{text}");
    }

    #[test]
    fn an_edit_changes_the_values_it_names_and_nothing_else() {
        let (_dir, path) = file(COMMENTED);
        let store = store();
        let outcome = edit_at(
            &path,
            &store,
            &[
                set("session.budget_tokens", 9_000_i64),
                set("harness.id", "claude"),
                set("mesh.hub_url", "https://hub.example"),
            ],
            None,
            "test",
        )
        .unwrap();
        let text = read(&path);
        assert_kept(&text);
        assert!(
            text.contains("budget_tokens = 9000    # small on purpose"),
            "{text}"
        );
        assert!(text.contains("id = \"claude\""), "{text}");
        // Nothing the edit did not name was written: no defaults appear.
        assert!(!text.contains("harness_image"), "{text}");
        assert!(!text.contains("[providers"), "{text}");
        assert_eq!(
            outcome.changed(),
            ["harness.id", "session.budget_tokens", "mesh.hub_url"]
        );
        assert!(outcome.restart_required);
        assert_eq!(outcome.new_hash, hash(text.as_bytes()));
        assert_eq!(outcome.old_hash, hash(COMMENTED.as_bytes()));
        let back = Config::try_load_from(&path).unwrap();
        assert_eq!(back.harness.id, "claude");
        assert_eq!(back.mesh.hub_url.as_deref(), Some("https://hub.example"));
    }

    #[test]
    fn a_stale_hash_is_a_conflict_and_the_file_is_untouched() {
        let (_dir, path) = file(COMMENTED);
        let store = store();
        let stale = hash(b"node_name = \"older\"\n");
        let err = edit_at(
            &path,
            &store,
            &[set("harness.id", "claude")],
            Some(&stale),
            "test",
        )
        .err()
        .unwrap();
        match err {
            EditError::Conflict { current } => assert_eq!(current, hash(COMMENTED.as_bytes())),
            other => panic!("expected a conflict, got {other}"),
        }
        assert_eq!(read(&path), COMMENTED);
        assert!(store.config_audit_recent(10).unwrap().is_empty());

        // The current hash goes through.
        let current = snapshot_at(&path).unwrap().hash;
        edit_at(
            &path,
            &store,
            &[set("harness.id", "claude")],
            Some(&current),
            "test",
        )
        .unwrap();
        assert!(read(&path).contains("claude"));
    }

    #[test]
    fn a_result_that_would_not_load_is_refused_and_the_file_is_untouched() {
        let (_dir, path) = file(COMMENTED);
        let store = store();
        for op in [
            set("mesh.heartbeat_secs", "often"),
            set("repo[0].egress", vec!["not a host"]),
            set("notify.contact", "phone"),
        ] {
            let err = edit_at(&path, &store, std::slice::from_ref(&op), None, "test")
                .err()
                .unwrap_or_else(|| panic!("{op:?} was accepted"));
            assert!(matches!(err, EditError::Invalid(_)), "{op:?}: {err}");
            assert_eq!(read(&path), COMMENTED);
        }
        // An operation that does not fit the file is refused the same way.
        let err = edit_at(&path, &store, &[set("node_name.x", 1_i64)], None, "test")
            .err()
            .unwrap();
        assert!(matches!(err, EditError::Op(_)), "{err}");
        let err = edit_at(
            &path,
            &store,
            &[Op::remove(KeyPath::parse("repo[7]").unwrap())],
            None,
            "test",
        )
        .err()
        .unwrap();
        assert!(err.to_string().contains("no entry 7"), "{err}");
        assert_eq!(read(&path), COMMENTED);
        assert!(store.config_audit_recent(10).unwrap().is_empty());
    }

    #[test]
    fn a_file_that_does_not_parse_is_never_rewritten() {
        let broken = "[mesh\nhub_url = \n";
        let (_dir, path) = file(broken);
        let err = edit_at(
            &path,
            &store(),
            &[set("harness.id", "claude")],
            None,
            "test",
        )
        .err()
        .unwrap();
        assert!(matches!(err, EditError::Unparseable(_)), "{err}");
        assert_eq!(read(&path), broken);
        assert!(snapshot_at(&path).is_err());
    }

    #[test]
    fn an_edit_that_changes_nothing_writes_nothing() {
        let (_dir, path) = file(COMMENTED);
        let store = store();
        let outcome = edit_at(
            &path,
            &store,
            &[
                set("session.budget_tokens", 5_i64),
                set("harness.id", "opencode"),
            ],
            None,
            "test",
        )
        .unwrap();
        assert!(!outcome.wrote());
        assert!(outcome.changes.is_empty());
        assert_eq!(read(&path), COMMENTED);
        assert!(store.config_audit_recent(10).unwrap().is_empty());
    }

    /// Every write leaves a row naming who and what, and a push to the
    /// operator's devices through the ordinary operator notification path.
    #[tokio::test]
    async fn every_write_is_audited_and_announced() {
        let (_dir, path) = file(COMMENTED);
        let store = store();
        store
            .push_subscription_upsert(&crate::store::PushSubscriptionRow {
                id: "phone".into(),
                session_hash: None,
                endpoint: "https://push.invalid/device".into(),
                p256dh: "key".into(),
                auth: "auth".into(),
                user_agent: None,
                created_ms: crate::store::now_ms(),
                last_ok_ms: None,
                fail_count: 0,
                last_error: None,
                last_error_ms: None,
            })
            .unwrap();
        let outcome = edit_at(
            &path,
            &store,
            &[
                set("harness.id", "claude"),
                set("repo[1].timeout_secs", 60_i64),
            ],
            None,
            "Settings",
        )
        .unwrap();
        let id = outcome.audit_id.clone().unwrap();
        let rows = store.config_audit_recent(10).unwrap();
        assert_eq!(rows.len(), 1);
        let row = &rows[0];
        assert_eq!(row.id, id);
        assert_eq!(row.actor, "Settings");
        assert_eq!(row.old_hash, hash(COMMENTED.as_bytes()));
        assert_eq!(row.new_hash, hash(read(&path).as_bytes()));
        assert_eq!(
            row.changes,
            serde_json::json!([
                { "key": "harness.id", "before": "opencode", "after": "claude" },
                { "key": "repo[1].timeout_secs", "before": null, "after": 60 },
            ])
        );
        // The push went out through `notify::send_operator`, keyed by the
        // audit row: this node has no contact to sign with, so the device's
        // attempt says it was not sent, which is still an attempt recorded.
        let push = outcome.notified.expect("a push on the runtime");
        push.await.unwrap();
        let attempts = store.notification_attempts(&id).unwrap();
        assert_eq!(attempts.len(), 1, "{attempts:?}");

        // A second write is a second row, chained by hash.
        let second = edit_at(
            &path,
            &store,
            &[set("node_name", "renamed")],
            None,
            "tracon mesh init",
        )
        .unwrap();
        let rows = store.config_audit_recent(10).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].old_hash, row.new_hash);
        assert_eq!(rows[0].new_hash, second.new_hash);
        assert_eq!(rows[0].actor, "tracon mesh init");
    }

    /// A secret's value is recorded as changed, never as what it was or is.
    /// No key the node reads is a secret today, so this is checked through a
    /// key it does not read, which the schema holds to the strictest answer.
    #[test]
    fn a_secret_value_never_reaches_the_audit_log() {
        let (_dir, path) = file(COMMENTED);
        let store = store();
        edit_at(
            &path,
            &store,
            &[set("legacy.token", "hunter2"), set("node_name", "shown")],
            None,
            "test",
        )
        .unwrap();
        let row = &store.config_audit_recent(1).unwrap()[0];
        let changes = row.changes.to_string();
        assert!(!changes.contains("hunter2"), "{changes}");
        assert_eq!(
            row.changes,
            serde_json::json!([
                { "key": "node_name", "before": "kept", "after": "shown" },
                { "key": "legacy.token", "before": null, "after": REDACTED },
            ])
        );

        let secret = Change {
            key: KeyPath::keys(&["anything"]),
            before: Some("old".into()),
            after: None,
        };
        let entry = audit_entry(&secret);
        assert_eq!(entry["before"], REDACTED);
        assert_eq!(entry["after"], serde_json::Value::Null);
    }

    #[test]
    fn update_decides_again_when_another_edit_lands_first() {
        let (_dir, path) = file(COMMENTED);
        let store = store();
        let mut calls = 0;
        let outcome = update_at(&path, &store, "test", |cfg| {
            calls += 1;
            if calls == 1 {
                // Someone else's edit, between this read and this write.
                let mut text = read(&path);
                text.push_str("\n[memory]\npromote_at = \"03:00\"\n");
                std::fs::write(&path, text).unwrap();
            }
            Ok::<_, EditError>(vec![set(
                "session.budget_tokens",
                cfg.session.budget_tokens + 1,
            )])
        })
        .unwrap();
        assert_eq!(calls, 2);
        assert!(outcome.wrote());
        let back = Config::try_load_from(&path).unwrap();
        assert_eq!(back.session.budget_tokens, 6);
        assert_eq!(back.memory.promote_at, "03:00");
    }

    /// The `[[repo]]` operations the writers use (Settings, an approved egress
    /// ask, an approved repository setup) touch the entry and the keys that
    /// change, and the comments on everything else stay.
    #[test]
    fn repo_writers_keep_the_rest_of_the_file() {
        let (_dir, path) = file(COMMENTED);
        let store = store();
        let before = snapshot_at(&path).unwrap().config.repo;

        // An egress ask saved to the blog's entry.
        let mut blog = before[0].clone();
        blog.egress.push("crates.io".into());
        blog.session_egress = true;
        edit_at(
            &path,
            &store,
            &Op::put_repo(&before, Some(0), &blog).unwrap(),
            None,
            "test",
        )
        .unwrap();
        let text = read(&path);
        assert_kept(&text);
        assert!(text.contains("session_egress = true"), "{text}");
        assert!(!text.contains("prepare"), "{text}");

        // A repository with no entry gets one, after the others.
        let before = snapshot_at(&path).unwrap().config.repo;
        let added = Repo {
            path: "/srv/new".into(),
            prepare: vec!["bun install".into()],
            ..Default::default()
        };
        edit_at(
            &path,
            &store,
            &Op::put_repo(&before, None, &added).unwrap(),
            None,
            "test",
        )
        .unwrap();
        let text = read(&path);
        assert_kept(&text);
        assert!(text.find("/srv/notes").unwrap() < text.find("/srv/new").unwrap());
        assert!(!text.contains("session_egress = false"), "{text}");

        // Settings replaces the whole table: the second entry goes, the
        // others are edited where they stand.
        let before = snapshot_at(&path).unwrap().config.repo;
        let after = vec![before[0].clone(), before[2].clone()];
        let ops = Op::replace_repos(&before, &after).unwrap();
        edit_at(&path, &store, &ops, None, "test").unwrap();
        let back = Config::try_load_from(&path).unwrap();
        let paths: Vec<_> = back.repo.iter().map(|r| r.path.clone()).collect();
        assert_eq!(
            paths,
            [PathBuf::from("/srv/blog"), PathBuf::from("/srv/new")]
        );
        assert_eq!(back.repo[1].prepare, ["bun install"]);
        assert!(read(&path).contains("# The blog builds with Zola."));
    }

    /// The settings form, the provider writer and the mesh writers, on the
    /// same commented file.
    #[test]
    fn settings_provider_and_mesh_writers_keep_the_rest_of_the_file() {
        let (_dir, path) = file(COMMENTED);
        let store = store();
        let cfg = snapshot_at(&path).unwrap().config;
        let patch = crate::http::settings::apply(
            &cfg,
            &serde_json::json!({
                "session": { "budget_tokens": 7 },
                "gateway": { "allow_hosts": ["^example\\.com$"] },
                "providers": { "openai-codex": { "models": [{ "id": "gpt-6" }] } },
            }),
        )
        .unwrap();
        edit_at(&path, &store, &patch.ops, None, "Settings").unwrap();
        assert_kept(&read(&path));

        // A provider added whole, beside one the form wrote.
        let provider = crate::config::Provider {
            credential: "openrouter".into(),
            upstream: "https://openrouter.ai/api".into(),
            ..Default::default()
        };
        let op =
            Op::set_serialized(KeyPath::keys(&["providers", "openrouter"]), &provider).unwrap();
        edit_at(&path, &store, &[op], None, "Connections").unwrap();
        // Pairing with a hub and leaving it.
        let hub = KeyPath::keys(&["mesh", "hub_url"]);
        edit_at(
            &path,
            &store,
            &[Op::set(hub.clone(), "https://hub")],
            None,
            "test",
        )
        .unwrap();
        edit_at(&path, &store, &[Op::remove(hub)], None, "test").unwrap();

        let text = read(&path);
        assert_kept(&text);
        assert!(
            text.contains("budget_tokens = 7    # small on purpose"),
            "{text}"
        );
        let back = Config::try_load_from(&path).unwrap();
        assert_eq!(back.providers["openrouter"].credential, "openrouter");
        assert_eq!(back.providers["openai-codex"].models[0].id, "gpt-6");
        // The default provider was written whole, so it kept its credential.
        assert_eq!(back.providers["openai-codex"].credential, "openai-codex");
        assert!(back.mesh.hub_url.is_none());
        assert_eq!(store.config_audit_recent(10).unwrap().len(), 4);
    }

    /// A provider the file never named is a default entry; editing it must
    /// not leave a partial one that shadows its credential, or drop the
    /// others.
    #[test]
    fn a_provider_edit_on_a_file_without_providers_keeps_them_whole() {
        let (_dir, path) = file("node_name = \"n\"\n");
        let cfg = snapshot_at(&path).unwrap().config;
        let mut models = cfg.providers["openai-codex"].models.clone();
        models.truncate(1);
        let patch = crate::http::settings::apply(
            &cfg,
            &serde_json::json!({ "providers": { "openai-codex": { "models": models } } }),
        )
        .unwrap();
        edit_at(&path, &store(), &patch.ops, None, "test").unwrap();
        let back = Config::try_load_from(&path).unwrap();
        let codex = &back.providers["openai-codex"];
        assert_eq!(codex.models, models);
        assert_eq!(codex.credential, "openai-codex");
        assert!(!codex.upstream.is_empty());
        assert!(back.providers.contains_key("anthropic"));

        // Declaring none is written as a declaration. (What a load makes of
        // `models = []` is `from_toml`'s business: today it fills the
        // defaults back in.)
        let cfg = snapshot_at(&path).unwrap().config;
        let patch = crate::http::settings::apply(
            &cfg,
            &serde_json::json!({ "providers": { "anthropic": { "models": [] } } }),
        )
        .unwrap();
        edit_at(&path, &store(), &patch.ops, None, "test").unwrap();
        assert!(read(&path).contains("models = []"), "{}", read(&path));
        let back = Config::try_load_from(&path).unwrap();
        assert_eq!(back.providers["anthropic"].credential, "anthropic");
    }

    /// The edit renames a new file into place, which must not turn a
    /// symlinked `node.toml` into a copy or reset its mode.
    #[cfg(unix)]
    #[test]
    fn an_edit_keeps_the_files_link_and_mode() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real.toml");
        let link = dir.path().join("node.toml");
        std::fs::write(&real, "node_name = \"n\"\n").unwrap();
        std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o640)).unwrap();
        std::os::unix::fs::symlink(&real, &link).unwrap();
        edit_at(
            &link,
            &store(),
            &[set("harness.id", "claude")],
            None,
            "test",
        )
        .unwrap();
        assert!(std::fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink());
        assert!(read(&real).contains("claude"));
        let mode = std::fs::metadata(&real).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o640);
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 2);
    }

    #[test]
    fn a_missing_file_is_created_with_only_what_was_set() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fresh/node.toml");
        let snap = snapshot_at(&path).unwrap();
        assert_eq!(snap.hash, hash(b""));
        edit_at(
            &path,
            &store(),
            &[set("mesh.hub_url", "https://hub")],
            Some(&snap.hash),
            "test",
        )
        .unwrap();
        assert_eq!(read(&path).trim(), "[mesh]\nhub_url = \"https://hub\"");
    }

    #[test]
    fn entries_are_set_inserted_and_removed_in_either_spelling() {
        for text in [
            "[[service]]\nname = \"a\" # first\nport = 1\n\n[[service]]\nname = \"c\"\nport = 3\n",
            "service = [{ name = \"a\", port = 1 }, { name = \"c\", port = 3 }] # inline\n",
        ] {
            let image = format!("img@sha256:{}", "a".repeat(64));
            let entry = |name: &str, port: i64| {
                let mut t = toml::Table::new();
                t.insert("name".into(), name.into());
                t.insert("image".into(), image.clone().into());
                t.insert("port".into(), port.into());
                t
            };
            let ops = [
                set("service[0].image", image.as_str()),
                set("service[1].image", image.as_str()),
                Op::Insert {
                    array: KeyPath::keys(&["service"]),
                    index: Some(1),
                    entry: entry("b", 2),
                },
                Op::append(KeyPath::keys(&["service"]), entry("d", 4)),
                Op::remove(KeyPath::parse("service[3]").unwrap()),
                Op::Set {
                    key: KeyPath::parse("service[2]").unwrap(),
                    value: entry("c", 30).into(),
                },
            ];
            let next = apply_to_text(text, &ops).unwrap();
            let cfg = Config::from_toml(&next, Path::new("node.toml")).unwrap();
            let got: Vec<_> = cfg
                .service
                .iter()
                .map(|s| (s.name.as_str(), s.port))
                .collect();
            assert_eq!(got, [("a", 1), ("b", 2), ("c", 30)], "{next}");
            assert!(
                next.contains("# first") || next.contains("# inline"),
                "{next}"
            );
        }
    }

    #[test]
    fn a_key_path_reads_as_it_is_written() {
        for text in [
            "node_name",
            "repo[2].egress",
            "providers.\"my.provider\".models[0].id",
            "github.dispatch.\"owner/name\"",
        ] {
            let path = KeyPath::parse(text).unwrap();
            assert_eq!(path.to_string(), text);
        }
        assert_eq!(
            KeyPath::parse("providers.\"a.b\".models[1].id")
                .unwrap()
                .schema_key(),
            ["providers", "a.b", "models", "id"]
        );
        for bad in ["", "a..b", "a.", "a[x]", "a b", "\"open"] {
            assert!(KeyPath::parse(bad).is_err(), "{bad:?}");
        }
    }
}
