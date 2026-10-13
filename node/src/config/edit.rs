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
//!    refuses anything that would not load, or that makes a key name a
//!    program or path the path guard refuses (`config::path_guard`);
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
    /// index replaces that entry of an array. An array of plain values is
    /// replaced whole when it differs: a comment between its elements goes
    /// with it, while the comments around the key stay.
    Set { key: KeyPath, value: toml::Value },
    /// Remove the key, or with an index the array entry. A key that is not
    /// there is already removed. A removed table takes the comment directly
    /// above its header with it; a comment block set apart by a blank line
    /// (a file's header, a note on a section) moves to what follows.
    Remove { key: KeyPath },
    /// Put the entries of an array of tables in a new order: entry `k` is
    /// the one that was at `order[k]`. Each entry keeps its comments.
    Reorder { array: KeyPath, order: Vec<usize> },
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
    /// the file loads it. Entries are matched by `path` (in order, where two
    /// share one), so a removed or moved entry takes its own comments with
    /// it; a matched entry is edited key by key, keeping the comments on what
    /// it keeps.
    pub fn replace_repos(
        before: &[super::Repo],
        after: &[super::Repo],
    ) -> Result<Vec<Op>, EditError> {
        replace_entries(&KeyPath::keys(&["repo"]), before, after, |r| &r.path)
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

/// The ops that make an array of tables read `after`, entries matched by
/// `key`: unmatched old entries are removed, the survivors put in `after`'s
/// order, new entries inserted where `after` has them, and every matched
/// entry edited key by key.
fn replace_entries<T: serde::Serialize + Default, K: PartialEq>(
    array: &KeyPath,
    before: &[T],
    after: &[T],
    key: impl Fn(&T) -> &K,
) -> Result<Vec<Op>, EditError> {
    // For each entry of `after`, the old entry it continues, if any.
    let mut used = vec![false; before.len()];
    let matched: Vec<Option<usize>> = after
        .iter()
        .map(|now| {
            let found = (0..before.len()).find(|&i| !used[i] && key(&before[i]) == key(now));
            if let Some(i) = found {
                used[i] = true;
            }
            found
        })
        .collect();

    let mut ops = Vec::new();
    // From the end, so each index still names the entry it did.
    for i in (0..before.len()).rev().filter(|&i| !used[i]) {
        ops.push(Op::remove(array.clone().index(i)));
    }
    // Where each survivor sits once the others are gone, in `after`'s order.
    let survivors: Vec<usize> = (0..before.len()).filter(|&i| used[i]).collect();
    let order: Vec<usize> = matched
        .iter()
        .flatten()
        .map(|i| survivors.iter().position(|s| s == i).expect("a survivor"))
        .collect();
    if order.windows(2).any(|w| w[0] > w[1]) {
        ops.push(Op::Reorder {
            array: array.clone(),
            order,
        });
    }
    for (j, now) in after.iter().enumerate() {
        if matched[j].is_none() {
            ops.push(Op::Insert {
                array: array.clone(),
                index: Some(j),
                entry: super::without_defaults(now).map_err(EditError::Op)?,
            });
        }
    }
    for (j, now) in after.iter().enumerate() {
        if let Some(i) = matched[j] {
            ops.extend(Op::changes(&array.clone().index(j), &before[i], now)?);
        }
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
    /// The result would make a key name a program or path the path guard
    /// refuses, or move a key only the host may change.
    #[error("{}", super::path_guard::describe(.0))]
    Refused(Vec<super::path_guard::Refusal>),
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
    Ok(Snapshot {
        config: load_text(&text, path).map_err(EditError::Unparseable)?,
        hash: hash(text.as_bytes()),
    })
}

/// `text` as a start would load it, without writing anything: the retired
/// harness is migrated in memory, as a load migrates it (the file itself is
/// rewritten by the next start, not by a reader).
pub fn load_text(text: &str, path: &Path) -> Result<Config, String> {
    match crate::legacy::migrate_config(text) {
        Some(migration) => Config::from_toml(&migration.text, path),
        None => Config::from_toml(text, path),
    }
}

/// Settle the audit rows an interrupted edit left pending: landed if the
/// file is now the version the row wrote, lost if not. The serving node runs
/// this at start.
///
/// `before_load` is the file's hash as it was before this start loaded it:
/// a load that migrates the retired harness rewrites the file, and a row
/// that wrote the version it migrated still landed.
pub fn reconcile_audit(store: &Store, before_load: Option<&str>) -> Result<usize, EditError> {
    reconcile_audit_at(&Config::config_path(), store, before_load)
}

pub fn reconcile_audit_at(
    path: &Path,
    store: &Store,
    before_load: Option<&str>,
) -> Result<usize, EditError> {
    let current = file_hash(path)?;
    let mut versions = vec![current.as_str()];
    versions.extend(before_load);
    store
        .config_audit_settle(&versions)
        .map_err(|e| EditError::Audit(e.to_string()))
}

/// The hash of the file at `path` as it is now: the empty file's when there
/// is none.
pub fn file_hash(path: &Path) -> Result<String, EditError> {
    Ok(hash(&read(path)?))
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
    let held = lock(path)?;
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
    // Checked as a load would see it, so a file still naming the retired
    // harness (migrated at every load) can be edited.
    let config = match crate::legacy::migrate_config(&next) {
        Some(migration) => Config::from_toml(&migration.text, path),
        None => Config::from_toml(&next, path),
    }
    .map_err(EditError::Invalid)?;
    let changes = diff(&text, &next);
    check_builder(&config, &changes)?;
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
    // No key may come to name a program, a secret or a directory the path
    // guard refuses. A key that already did is left to the start's warning,
    // so it does not hold up an unrelated edit.
    let before = match crate::legacy::migrate_config(&text) {
        Some(migration) => Config::from_toml(&migration.text, path),
        None => Config::from_toml(&text, path),
    }
    .ok();
    let refused = super::path_guard::new_refusals(before.as_ref(), &config);
    if !refused.is_empty() {
        return Err(EditError::Refused(refused));
    }

    let new_hash = hash(next.as_bytes());
    let row = ConfigAuditRow {
        id: uuid::Uuid::now_v7().to_string(),
        at_ms: crate::store::now_ms(),
        actor: actor.to_string(),
        old_hash: old_hash.clone(),
        new_hash: new_hash.clone(),
        changes: serde_json::Value::Array(changes.iter().map(audit_entry).collect()),
        state: "pending".into(),
    };
    // Pending until the file is in place: a crash between the two leaves a
    // row `reconcile_audit` settles at the next start, by the file's hash.
    store
        .config_audit_insert(&row)
        .map_err(|e| EditError::Audit(e.to_string()))?;
    if let Err(error) = super::write_replacing(path, next.as_bytes()) {
        let _ = store.config_audit_delete(&row.id);
        return Err(error.into());
    }
    if let Err(error) = store.config_audit_landed(&row.id) {
        tracing::warn!(%error, id = %row.id, "node.toml was written; its audit row stays pending until the next start");
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

/// What an edit would make of the file, decided before it is confirmed:
/// the text it would write, what that loads as, and the keys it changes.
/// Neither the path guard nor anything else that looks at the host has run:
/// `edit` runs them, under its lock, when it writes.
pub struct Preview {
    pub config: Config,
    pub changes: Vec<Change>,
    /// The text the edit would write.
    pub text: String,
}

/// Apply `ops` to `text` and load the result with every check a load makes,
/// without writing anything.
pub fn preview(text: &str, ops: &[Op]) -> Result<Preview, EditError> {
    let next = apply_to_text(text, ops)?;
    let config = match crate::legacy::migrate_config(&next) {
        Some(migration) => Config::from_toml(&migration.text, &Config::config_path()),
        None => Config::from_toml(&next, &Config::config_path()),
    }
    .map_err(EditError::Invalid)?;
    let changes = diff(text, &next);
    check_builder(&config, &changes)?;
    Ok(Preview {
        config,
        changes,
        text: next,
    })
}

/// The file's text, and its hash.
pub fn read_text(path: &Path) -> Result<(String, String), EditError> {
    let bytes = read(path)?;
    let hash = hash(&bytes);
    let text = String::from_utf8(bytes).map_err(|_| unparseable(path, "not UTF-8"))?;
    Ok((text, hash))
}

/// [`edit`], for a node that is running on `cfg`: every key the schema
/// calls live applies at once through `cfg`'s `ConfigHandle`
/// (`Config::apply_edit`), the `[[repo]]` table among them, and the rest
/// is owed a restart. Taken under the table's lock, so it cannot
/// interleave with a `[[repo]]` writer.
pub fn edit_live(
    cfg: &Config,
    store: &Arc<Store>,
    ops: &[Op],
    expected_hash: Option<&str>,
    actor: &str,
) -> Result<EditOutcome, EditError> {
    edit_live_at(
        &Config::config_path(),
        cfg,
        store,
        ops,
        expected_hash,
        actor,
    )
}

pub fn edit_live_at(
    path: &Path,
    cfg: &Config,
    store: &Arc<Store>,
    ops: &[Op],
    expected_hash: Option<&str>,
    actor: &str,
) -> Result<EditOutcome, EditError> {
    let _repos = repo_lock();
    let outcome = edit_at(path, store, ops, expected_hash, actor)?;
    // The live keys (the `[[repo]]` table among them) apply now; the
    // others wait for a restart (`Config::apply_edit`).
    cfg.apply_edit_at(path, &outcome);
    Ok(outcome)
}

/// Make the `[[repo]]` table read `entries`, if the file is still the
/// version `expected_hash` names: a client that read the table and saves it
/// whole replaces exactly what it read, never an edit it did not see.
pub fn replace_repos_expecting(
    cfg: &Config,
    store: &Arc<Store>,
    expected_hash: &str,
    entries: &[super::Repo],
    actor: &str,
) -> Result<EditOutcome, EditError> {
    replace_repos_expecting_at(
        &Config::config_path(),
        cfg,
        store,
        expected_hash,
        entries,
        actor,
    )
}

pub fn replace_repos_expecting_at(
    path: &Path,
    cfg: &Config,
    store: &Arc<Store>,
    expected_hash: &str,
    entries: &[super::Repo],
    actor: &str,
) -> Result<EditOutcome, EditError> {
    let _repos = repo_lock();
    let snapshot = snapshot_at(path)?;
    if snapshot.hash != expected_hash {
        return Err(EditError::Conflict {
            current: snapshot.hash,
        });
    }
    let ops = Op::replace_repos(&snapshot.config.repo, entries)?;
    let outcome = edit_at(path, store, &ops, Some(expected_hash), actor)?;
    cfg.apply_edit_at(path, &outcome);
    Ok(outcome)
}

/// An error about the file, as said to a caller who is not at the node: the
/// TOML parser quotes the offending line of the file, which can hold any
/// value or comment written beside the one in question, so the quoted lines
/// are dropped and the line number and the reason are kept.
pub fn without_excerpt(message: &str) -> String {
    message
        .lines()
        .filter(|line| {
            let rest = line
                .trim_start()
                .trim_start_matches(|c: char| c.is_ascii_digit());
            !rest.trim_start().starts_with('|')
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// `value`, found at `at` (table keys, indexes left out), as a reader may
/// see it: down to each value, a secret key's replaced with [`REDACTED`],
/// and so is a key the schema does not know (`config_schema::lookup`).
pub fn redact(at: &[String], value: &toml::Value) -> toml::Value {
    match value {
        toml::Value::Table(table) => toml::Value::Table(
            table
                .iter()
                .map(|(key, value)| {
                    let mut below = at.to_vec();
                    below.push(key.clone());
                    (key.clone(), redact(&below, value))
                })
                .collect(),
        ),
        toml::Value::Array(values) if tables_of(values).is_some() => {
            toml::Value::Array(values.iter().map(|v| redact(at, v)).collect())
        }
        _ => {
            let key: Vec<&str> = at.iter().map(String::as_str).collect();
            match config_schema::lookup(&key).secret {
                true => toml::Value::String(REDACTED.into()),
                false => value.clone(),
            }
        }
    }
}

/// A builder the node cannot use is reported at start rather than fatal
/// there, so a node keeps running with one; but an edit that writes the
/// table is held to it.
fn check_builder(config: &Config, changes: &[Change]) -> Result<(), EditError> {
    let builder = KeyPath::keys(&["runtime", "kubernetes", "builder"]);
    if changes.iter().any(|c| c.key.0.starts_with(&builder.0)) {
        if let Some(settings) = &config.runtime.kubernetes.builder {
            crate::boundary::kubernetes::build::validate_settings(settings).map_err(|why| {
                EditError::Invalid(format!("[runtime.kubernetes.builder]: {why}"))
            })?;
        }
    }
    Ok(())
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

/// Held for the span of one edit's read, write and audit: a `std` guard
/// for this process (so nothing can hold it across an `.await`), and an
/// exclusive lock on `node.toml.lock` beside the file for every other one,
/// so `tracon mesh init` and a serving node cannot lose each other's edits.
pub(crate) struct Held {
    _process: std::sync::MutexGuard<'static, ()>,
    _file: std::fs::File,
}

pub(crate) fn lock(path: &Path) -> Result<Held, EditError> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let process = LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let target = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    if let Some(dir) = target.parent().filter(|dir| !dir.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir)?;
    }
    let lock_path = target.with_extension("toml.lock");
    // Made once, as private as the directory it is in; held through a read
    // descriptor, which is all a lock needs.
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&lock_path)
    {
        Ok(made) => {
            #[cfg(unix)]
            if let Some(dir) = lock_path.parent().filter(|d| !d.as_os_str().is_empty()) {
                use std::os::unix::fs::PermissionsExt;
                let mode = std::fs::metadata(dir)?.permissions().mode() & 0o666;
                made.set_permissions(std::fs::Permissions::from_mode(mode))?;
            }
            drop(made);
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e.into()),
    }
    let file = std::fs::File::open(&lock_path)?;
    file.lock()?;
    Ok(Held {
        _process: process,
        _file: file,
    })
}

/// The table every `[[repo]]` writer reads, changes and writes under, from
/// `Config::repos()` through `LiveRepos::set`. Taken before the edit lock.
pub(crate) fn repo_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Change the `[[repo]]` table the node runs on and write it to the file,
/// as one step: `change` is given the live table (`Config::repos`), the file
/// is edited entry by entry towards what it leaves, and the live table is set
/// to what the file then holds, all under one lock, so two writers cannot
/// both start from the same table and the live table never runs ahead of or
/// behind the file.
pub fn update_repos<T, E: From<EditError>>(
    cfg: &Config,
    store: &Arc<Store>,
    actor: &str,
    change: impl FnOnce(&mut Vec<super::Repo>) -> Result<T, E>,
) -> Result<(T, EditOutcome), E> {
    update_repos_at(&Config::config_path(), cfg, store, actor, change)
}

pub fn update_repos_at<T, E: From<EditError>>(
    path: &Path,
    cfg: &Config,
    store: &Arc<Store>,
    actor: &str,
    change: impl FnOnce(&mut Vec<super::Repo>) -> Result<T, E>,
) -> Result<(T, EditOutcome), E> {
    let _repos = repo_lock();
    let mut entries = cfg.repos().as_ref().clone();
    let value = change(&mut entries)?;
    let outcome = update_at(path, store, actor, |file| {
        Op::replace_repos(&file.repo, &entries)
    })?;
    if outcome.wrote() {
        cfg.apply_edit_at(path, &outcome);
    } else {
        cfg.live_repo.set(outcome.config.repo.clone());
    }
    Ok((value, outcome))
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

pub(crate) fn audit_entry(change: &Change) -> serde_json::Value {
    let at: Vec<String> = change
        .key
        .schema_key()
        .into_iter()
        .map(str::to_string)
        .collect();
    let shown = |value: &Option<toml::Value>| match value {
        None => serde_json::Value::Null,
        Some(value) => redacted(&at, value),
    };
    serde_json::json!({
        "key": change.key.to_string(),
        "before": shown(&change.before),
        "after": shown(&change.after),
    })
}

/// `value` as the log may show it: down to each value, a secret key's
/// replaced. A whole table or array of tables is checked key by key, so a
/// secret inside an entry stays hidden when the entry appears whole.
fn redacted(at: &[String], value: &toml::Value) -> serde_json::Value {
    match value {
        toml::Value::Table(table) => serde_json::Value::Object(
            table
                .iter()
                .map(|(key, value)| {
                    let mut below = at.to_vec();
                    below.push(key.clone());
                    (key.clone(), redacted(&below, value))
                })
                .collect(),
        ),
        toml::Value::Array(values) if tables_of(values).is_some() => {
            serde_json::Value::Array(values.iter().map(|v| redacted(at, v)).collect())
        }
        _ => {
            let key: Vec<&str> = at.iter().map(String::as_str).collect();
            match config_schema::lookup(&key).secret {
                true => serde_json::Value::String(REDACTED.into()),
                false => serde_json::to_value(value).unwrap_or_default(),
            }
        }
    }
}

/// The keys whose values differ between two texts of the file, down to the
/// values themselves: a table that appears or goes is each of its keys. An
/// array of tables whose length changed is one change at the array.
pub(crate) fn diff(before: &str, after: &str) -> Vec<Change> {
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
        // A table made or removed empty is a change of its own: `[x]` is
        // what was written.
        (None, Some(toml::Value::Table(t))) | (Some(toml::Value::Table(t)), None)
            if t.is_empty() =>
        {
            out.push(Change {
                key: at.clone(),
                before: before.cloned(),
                after: after.cloned(),
            })
        }
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
            let removed = match last {
                Seg::Key(name) => cursor.table().and_then(|table| table.remove(name)),
                Seg::Index(index) => {
                    let Cursor::Item(item) = cursor else {
                        return Err(not_array(&here));
                    };
                    match item {
                        Item::ArrayOfTables(array) if *index < array.len() => {
                            Some(Item::Table(array.remove(*index)))
                        }
                        Item::Value(toml_edit::Value::Array(array)) if *index < array.len() => {
                            array.remove(*index);
                            None
                        }
                        Item::ArrayOfTables(_) | Item::Value(toml_edit::Value::Array(_)) => {
                            return Err(no_entry(&here, *index))
                        }
                        _ => return Err(not_array(&here)),
                    }
                }
            };
            let first = match &removed {
                Some(Item::Table(table)) => Some(table),
                Some(Item::ArrayOfTables(tables)) => tables.get(0),
                _ => None,
            };
            if let Some((position, kept)) = first.and_then(|t| Some((t.position()?, detached(t)?)))
            {
                rehome(doc, position, &kept);
            }
        }
        Op::Reorder { array, order } => {
            let mut seen = vec![false; order.len()];
            let Some(Cursor::Item(item)) = walk(doc, &array.0, false)? else {
                return Err(not_array(array));
            };
            let len = match &*item {
                Item::ArrayOfTables(tables) => tables.len(),
                Item::Value(toml_edit::Value::Array(values)) => values.len(),
                _ => return Err(not_array(array)),
            };
            if order.len() != len
                || order
                    .iter()
                    .any(|&i| i >= len || std::mem::replace(&mut seen[i], true))
            {
                return Err(EditError::Op(format!(
                    "{order:?} is not an order of `{array}`'s {len} entries"
                )));
            }
            match item {
                Item::ArrayOfTables(tables) => {
                    let mut old: Vec<Table> = tables.iter().cloned().collect();
                    // A header or banner above the first entry belongs to
                    // the place, not the entry: it stays first.
                    let banner = match order.first() {
                        Some(&first) if first != 0 => peel(&mut old[0]),
                        _ => None,
                    };
                    let mut rebuilt = ArrayOfTables::new();
                    // The entries move; the places they render at do not.
                    // What sits under a moved entry renders after it.
                    for (slot, &from) in order.iter().enumerate() {
                        let mut table = old[from].clone();
                        if let Some(position) = old[slot].position() {
                            table.set_position(position);
                            place_children(&mut table, position);
                        }
                        if slot == 0 {
                            if let Some(banner) = &banner {
                                prepend(&mut table, banner);
                            }
                        }
                        rebuilt.push(table);
                    }
                    *tables = rebuilt;
                }
                Item::Value(toml_edit::Value::Array(values)) => {
                    let old: Vec<toml_edit::Value> = values.iter().cloned().collect();
                    values.clear();
                    for &from in order {
                        values.push_formatted(old[from].clone());
                    }
                }
                _ => unreachable!("checked above"),
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
                    let mut new = to_table(entry);
                    // Rendered where the entry it goes before is.
                    if let Some(position) = index
                        .and_then(|at| tables.get(at))
                        .and_then(Table::position)
                    {
                        new.set_position(position);
                        place_children(&mut new, position);
                    }
                    match index {
                        None => tables.push(new),
                        Some(at) if *at <= tables.len() => {
                            let mut all: Vec<Table> = tables.iter().cloned().collect();
                            // The banner above the first entry stays first.
                            if *at == 0 {
                                if let Some(banner) = all.first_mut().and_then(peel) {
                                    prepend(&mut new, &banner);
                                }
                            }
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

/// The part of the comment above a table's header that is set apart from
/// it by a blank line: not the table's own note, so it outlives the table.
fn detached(table: &Table) -> Option<String> {
    let prefix = table.decor().prefix()?.as_str()?;
    let lines: Vec<&str> = prefix.split_inclusive('\n').collect();
    let blank = lines.iter().rposition(|line| line.trim().is_empty())?;
    let kept: String = lines[..=blank].concat();
    kept.contains('#').then_some(kept)
}

/// Take the detached part off the comment above `table`'s header, leaving
/// its own note.
fn peel(table: &mut Table) -> Option<String> {
    let banner = detached(table)?;
    let prefix = table.decor().prefix()?.as_str()?.to_string();
    let own = &prefix[banner.len()..];
    table
        .decor_mut()
        .set_prefix(if own.is_empty() { "\n" } else { own }.to_string());
    Some(banner)
}

/// Put `text` above the comment already above `table`'s header.
fn prepend(table: &mut Table, text: &str) {
    let below = table
        .decor()
        .prefix()
        .and_then(|p| p.as_str())
        .unwrap_or("\n")
        .to_string();
    table.decor_mut().set_prefix(format!("{text}{below}"));
}

/// Put `text` above the first table rendered after `position`, or at the end
/// of the file when none is.
fn rehome(doc: &mut DocumentMut, position: isize, text: &str) {
    let mut next: Option<isize> = None;
    visit_tables(doc.as_table_mut(), &mut |table| {
        if let Some(p) = table.position().filter(|p| *p > position) {
            next = Some(next.map_or(p, |n| n.min(p)));
        }
        false
    });
    let placed = next.is_some_and(|target| {
        visit_tables(doc.as_table_mut(), &mut |table| {
            if table.position() != Some(target) {
                return false;
            }
            prepend(table, text);
            true
        })
    });
    if !placed {
        let trailing = doc.trailing().as_str().unwrap_or("").to_string();
        doc.set_trailing(format!("{trailing}\n{text}"));
    }
}

/// Every table under `table`, depth first, until `f` says to stop.
fn visit_tables(table: &mut Table, f: &mut dyn FnMut(&mut Table) -> bool) -> bool {
    for (_, item) in table.iter_mut() {
        match item {
            Item::Table(child) => {
                if f(child) || visit_tables(child, f) {
                    return true;
                }
            }
            Item::ArrayOfTables(children) => {
                for child in children.iter_mut() {
                    if f(child) || visit_tables(child, f) {
                        return true;
                    }
                }
            }
            _ => {}
        }
    }
    false
}

/// Render the tables under `table` right after it, at its `position`: a
/// tie keeps the order they are visited in, which is parent first.
fn place_children(table: &mut Table, position: isize) {
    visit_tables(table, &mut |child| {
        child.set_position(position);
        false
    });
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
        // The file, its link, and the lock beside the file; no temporary left.
        let mut names: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        assert_eq!(names, ["node.toml", "real.toml", "real.toml.lock"]);
        // The lock is as private as its directory, and never executable.
        let dir_mode = std::fs::metadata(dir.path()).unwrap().permissions().mode();
        let lock_mode = std::fs::metadata(dir.path().join("real.toml.lock"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(lock_mode & 0o777, dir_mode & 0o666);
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

    /// Two `[[repo]]` writers at once each start from the table the other
    /// left, so neither undoes the other, and the live table ends as the file.
    #[test]
    fn repo_writers_at_once_lose_nothing() {
        let (_dir, path) = file("[[repo]]\npath = \"/r\"\n");
        let store = store();
        let cfg = Arc::new(snapshot_at(&path).unwrap().config);
        let writers: Vec<_> = (0..8)
            .map(|i| {
                let (path, cfg, store) = (path.clone(), cfg.clone(), store.clone());
                std::thread::spawn(move || {
                    update_repos_at(&path, &cfg, &store, "test", |entries| {
                        entries[0].egress.push(format!("h{i}.example"));
                        Ok::<_, EditError>(())
                    })
                    .unwrap();
                })
            })
            .collect();
        for writer in writers {
            writer.join().unwrap();
        }
        let file = Config::try_load_from(&path).unwrap();
        assert_eq!(file.repo[0].egress.len(), 8, "{:?}", file.repo[0].egress);
        assert_eq!(cfg.repos()[0].egress, file.repo[0].egress);

        // An operator's removal is not undone by a later ask.
        update_repos_at(&path, &cfg, &store, "Settings", |entries| {
            entries[0].egress.retain(|h| h != "h0.example");
            Ok::<_, EditError>(())
        })
        .unwrap();
        update_repos_at(&path, &cfg, &store, "ask", |entries| {
            entries[0].egress.push("late.example".into());
            Ok::<_, EditError>(())
        })
        .unwrap();
        let egress = Config::try_load_from(&path).unwrap().repo[0].egress.clone();
        assert!(!egress.contains(&"h0.example".to_string()), "{egress:?}");
        assert!(egress.contains(&"late.example".to_string()), "{egress:?}");
    }

    const THREE: &str = r#"# The operator's file.

[[repo]]
# Never open session egress here.
path = "/srv/blog"

[repo.style]
conventional = true

[[repo]]
# Mine.
path = "/srv/mine"

[[repo]]
# Notes.
path = "/srv/notes"
"#;

    /// Entries are matched by path: removing the first takes its own
    /// comment and leaves the others' where they are.
    #[test]
    fn removing_an_entry_takes_its_comments_with_it() {
        let (_dir, path) = file(THREE);
        let before = snapshot_at(&path).unwrap().config.repo;
        let after = vec![before[1].clone(), before[2].clone()];
        let ops = Op::replace_repos(&before, &after).unwrap();
        edit_at(&path, &store(), &ops, None, "test").unwrap();
        let text = read(&path);
        assert!(!text.contains("Never open"), "{text}");
        assert!(!text.contains("[repo.style]"), "{text}");
        assert!(text.starts_with("# The operator's file."), "{text}");
        let mine = text.find("# Mine.").unwrap();
        assert!(mine < text.find("/srv/mine").unwrap(), "{text}");
        assert!(
            text.find("/srv/mine").unwrap() < text.find("# Notes.").unwrap(),
            "{text}"
        );
        let back = Config::try_load_from(&path).unwrap();
        let paths: Vec<_> = back.repo.iter().map(|r| r.path.clone()).collect();
        assert_eq!(
            paths,
            [PathBuf::from("/srv/mine"), PathBuf::from("/srv/notes")]
        );
    }

    /// A moved entry carries its comments and its sub-tables.
    #[test]
    fn reordering_entries_moves_their_comments_with_them() {
        let (_dir, path) = file(THREE);
        let before = snapshot_at(&path).unwrap().config.repo;
        let after = vec![before[2].clone(), before[0].clone(), before[1].clone()];
        let ops = Op::replace_repos(&before, &after).unwrap();
        assert!(
            ops.iter().any(|op| matches!(op, Op::Reorder { .. })),
            "{ops:?}"
        );
        edit_at(&path, &store(), &ops, None, "test").unwrap();
        let text = read(&path);
        let at = |needle: &str| {
            text.find(needle)
                .unwrap_or_else(|| panic!("{needle}: {text}"))
        };
        assert!(at("# Notes.") < at("/srv/notes"), "{text}");
        assert!(at("/srv/notes") < at("# Never open"), "{text}");
        assert!(at("# Never open") < at("/srv/blog"), "{text}");
        assert!(at("/srv/blog") < at("[repo.style]"), "{text}");
        assert!(at("[repo.style]") < at("# Mine."), "{text}");
        let back = Config::try_load_from(&path).unwrap();
        let paths: Vec<_> = back.repo.iter().map(|r| r.path.clone()).collect();
        assert_eq!(
            paths,
            [
                PathBuf::from("/srv/notes"),
                PathBuf::from("/srv/blog"),
                PathBuf::from("/srv/mine")
            ]
        );
        // The style stayed with its entry.
        assert!(back.repo[1].style.as_ref().is_some_and(|s| s.conventional));
        assert!(back.repo[0].style.is_none() && back.repo[2].style.is_none());

        // A new entry lands where the table puts it, between the others.
        let before = back.repo.clone();
        let mut after = before.clone();
        after.insert(
            1,
            Repo {
                path: "/srv/new".into(),
                ..Default::default()
            },
        );
        edit_at(
            &path,
            &store(),
            &Op::replace_repos(&before, &after).unwrap(),
            None,
            "test",
        )
        .unwrap();
        let paths: Vec<_> = Config::try_load_from(&path)
            .unwrap()
            .repo
            .iter()
            .map(|r| r.path.clone())
            .collect();
        assert_eq!(paths[1], PathBuf::from("/srv/new"), "{}", read(&path));
        assert!(Config::try_load_from(&path).unwrap().repo[2]
            .style
            .is_some());
    }

    /// The header of a file that starts with a table outlives that table.
    #[test]
    fn removing_the_first_table_keeps_the_files_header() {
        let text = "# Header, written once.\n# Second line.\n\n# About the blog.\n[[repo]]\npath = \"/srv/blog\"\n\n[memory]\npromote_at = \"03:00\"\n";
        let next = apply_to_text(text, &[Op::remove(KeyPath::parse("repo[0]").unwrap())]).unwrap();
        assert!(
            next.contains("# Header, written once.\n# Second line."),
            "{next}"
        );
        assert!(!next.contains("About the blog"), "{next}");
        assert!(
            next.find("# Header").unwrap() < next.find("[memory]").unwrap(),
            "{next}"
        );
        // With nothing after it, the header stays in the file anyway.
        let lone = "# Header.\n\n[mesh]\nhub_url = \"https://hub\"\n";
        let next = apply_to_text(lone, &[Op::remove(KeyPath::keys(&["mesh"]))]).unwrap();
        assert!(next.contains("# Header."), "{next:?}");
    }

    /// An entry that appears whole is redacted key by key.
    #[test]
    fn a_secret_inside_a_new_entry_is_redacted() {
        let (_dir, path) = file(COMMENTED);
        let store = store();
        let mut entry = toml::Table::new();
        entry.insert("name".into(), "browser".into());
        entry.insert(
            "image".into(),
            format!("img@sha256:{}", "a".repeat(64)).into(),
        );
        entry.insert("port".into(), 9222_i64.into());
        entry.insert("token".into(), "hunter2".into());
        edit_at(
            &path,
            &store,
            &[Op::append(KeyPath::keys(&["service"]), entry)],
            None,
            "test",
        )
        .unwrap();
        let row = &store.config_audit_recent(1).unwrap()[0];
        let shown = row.changes.to_string();
        assert!(!shown.contains("hunter2"), "{shown}");
        assert_eq!(row.changes[0]["key"], "service");
        assert_eq!(row.changes[0]["after"][0]["name"], "browser");
        assert_eq!(row.changes[0]["after"][0]["token"], REDACTED);
    }

    #[test]
    fn an_empty_table_made_on_purpose_is_written() {
        let (_dir, path) = file(COMMENTED);
        let outcome = edit_at(
            &path,
            &store(),
            &[Op::set(
                KeyPath::keys(&["github", "dispatch"]),
                toml::Table::new(),
            )],
            None,
            "test",
        )
        .unwrap();
        assert!(outcome.wrote());
        assert_eq!(outcome.changed(), ["github.dispatch"]);
        assert!(read(&path).contains("[github.dispatch]"), "{}", read(&path));
    }

    /// A builder the node could not use is refused when an edit writes the
    /// table; one already in the file does not block an unrelated edit.
    #[test]
    fn an_edit_of_the_builder_is_held_to_what_a_builder_needs() {
        let (_dir, path) = file(COMMENTED);
        let store = store();
        let err = edit_at(
            &path,
            &store,
            &[set("runtime.kubernetes.builder.registry", "not-a-registry")],
            None,
            "test",
        )
        .err()
        .unwrap();
        assert!(err.to_string().contains("registry must be"), "{err}");
        assert_eq!(read(&path), COMMENTED);
        edit_at(
            &path,
            &store,
            &[set(
                "runtime.kubernetes.builder.registry",
                "registry.example.com/team",
            )],
            None,
            "test",
        )
        .unwrap();

        let broken = format!("{COMMENTED}\n[runtime.kubernetes.builder]\nregistry = \"nope\"\n");
        let (_dir, path) = file(&broken);
        edit_at(&path, &store, &[set("node_name", "other")], None, "test").unwrap();
    }

    /// A program an agent could have written, or a secret of the node's own
    /// as the embedding key, is refused; the file is untouched.
    #[test]
    fn a_path_the_guard_refuses_is_not_written() {
        let planted = Config::state_dir().join("workspaces/planted/gh");
        std::fs::create_dir_all(planted.parent().unwrap()).unwrap();
        std::fs::write(&planted, "#!/bin/sh\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&planted, std::fs::Permissions::from_mode(0o755)).unwrap();
        let (_dir, path) = file(COMMENTED);
        let store = store();
        for (key, value) in [
            ("publish.gh", planted.to_string_lossy().into_owned()),
            (
                "embed.api_key_file",
                Config::db_path().to_string_lossy().into_owned(),
            ),
            (
                "embed.api_key_file",
                crate::mesh::identity::seed_path()
                    .to_string_lossy()
                    .into_owned(),
            ),
            ("consulta.args", String::new()),
        ] {
            let op = match key {
                "consulta.args" => Op::set(
                    KeyPath::parse(key).unwrap(),
                    toml::Value::Array(
                        ["run", "python", "-c", "print(1)"]
                            .map(|a| toml::Value::String(a.into()))
                            .to_vec(),
                    ),
                ),
                _ => set(key, value.as_str()),
            };
            let err = edit_at(&path, &store, &[op], None, "test").err().unwrap();
            assert!(matches!(err, EditError::Refused(_)), "{key}: {err}");
            assert!(err.to_string().contains("path guard"), "{key}: {err}");
            assert_eq!(read(&path), COMMENTED);
        }
    }

    /// A key that already broke a rule is the start's warning, not a reason
    /// to refuse an edit of something else.
    #[test]
    fn a_key_already_refused_does_not_block_another_edit() {
        let planted = Config::state_dir().join("workspaces/already/gh");
        let broken = format!(
            "{COMMENTED}\n[publish]\ngh = {:?}\n",
            planted.to_string_lossy()
        );
        let (_dir, path) = file(&broken);
        edit_at(&path, &store(), &[set("node_name", "other")], None, "test").unwrap();
        // Writing it again as it is changes nothing; writing another bad
        // value is refused.
        let elsewhere = Config::state_dir().join("workspaces/other/gh");
        let err = edit_at(
            &path,
            &store(),
            &[set("publish.gh", elsewhere.to_string_lossy().as_ref())],
            None,
            "test",
        )
        .err()
        .unwrap();
        assert!(err.to_string().contains("path guard"), "{err}");
    }

    #[test]
    fn a_harness_the_node_cannot_run_is_refused() {
        let (_dir, path) = file(COMMENTED);
        let err = edit_at(&path, &store(), &[set("harness.id", "aider")], None, "test")
            .err()
            .unwrap();
        assert!(matches!(err, EditError::Invalid(_)), "{err}");
        assert_eq!(read(&path), COMMENTED);
    }

    /// A row is pending until its file is in place; one an interrupted edit
    /// left behind is settled at the next start by the file's hash.
    #[test]
    fn an_interrupted_write_is_settled_by_the_files_hash() {
        let (_dir, path) = file(COMMENTED);
        let store = store();
        let outcome = edit_at(&path, &store, &[set("node_name", "n2")], None, "test").unwrap();
        assert_eq!(store.config_audit_recent(1).unwrap()[0].state, "landed");
        let pending = |id: &str, new_hash: &str| ConfigAuditRow {
            id: id.into(),
            at_ms: crate::store::now_ms(),
            actor: "test".into(),
            old_hash: outcome.new_hash.clone(),
            new_hash: new_hash.into(),
            changes: serde_json::json!([]),
            state: "pending".into(),
        };
        store
            .config_audit_insert(&pending("landed-before-crash", &outcome.new_hash))
            .unwrap();
        store
            .config_audit_insert(&pending("never-renamed", &hash(b"other")))
            .unwrap();
        // Landed, and a later write started from it: the file has moved on.
        store
            .config_audit_insert(&pending("built-on", &hash(b"middle")))
            .unwrap();
        let mut later = pending("later", &hash(b"later"));
        later.old_hash = hash(b"middle");
        store.config_audit_insert(&later).unwrap();
        store.config_audit_landed("later").unwrap();
        // Landed, and the start's own load then migrated the file.
        store
            .config_audit_insert(&pending("then-migrated", &hash(b"pre-migration")))
            .unwrap();
        let before_load = hash(b"pre-migration");
        assert_eq!(
            reconcile_audit_at(&path, &store, Some(&before_load)).unwrap(),
            4
        );
        let rows = store.config_audit_recent(10).unwrap();
        let state = |id: &str| rows.iter().find(|r| r.id == id).unwrap().state.clone();
        assert_eq!(state("landed-before-crash"), "landed");
        assert_eq!(state("built-on"), "landed");
        assert_eq!(state("then-migrated"), "landed");
        assert_eq!(state("never-renamed"), "lost");
    }

    const BANNERED: &str = "# File header.\n\n[[repo]]\n# a's note\npath = \"/a\"\n\n[[repo]]\npath = \"/b\"\n\n[[repo]]\npath = \"/c\"\n";

    /// Whatever ends up first under the file's header, the header stays at
    /// the top of the file.
    #[test]
    fn the_files_header_stays_first_whatever_moves() {
        let repo = |p: &str| Repo {
            path: p.into(),
            ..Default::default()
        };
        let before = vec![repo("/a"), repo("/b"), repo("/c")];
        for after in [
            vec![repo("/c"), repo("/b")],
            vec![repo("/c"), repo("/a"), repo("/b")],
            vec![repo("/new"), repo("/a"), repo("/b"), repo("/c")],
        ] {
            let ops = Op::replace_repos(&before, &after).unwrap();
            let next = apply_to_text(BANNERED, &ops).unwrap();
            assert!(next.starts_with("# File header.\n"), "{ops:?}\n{next}");
            assert_eq!(next.matches("# File header.").count(), 1, "{next}");
            let back = Config::from_toml(&next, Path::new("node.toml")).unwrap();
            let paths: Vec<_> = back.repo.iter().map(|r| r.path.clone()).collect();
            let want: Vec<_> = after.iter().map(|r| r.path.clone()).collect();
            assert_eq!(paths, want, "{next}");
            // a's own note stays with a, wherever it went.
            if let Some(a) = next.find("path = \"/a\"") {
                assert!(next[..a].ends_with("# a's note\n"), "{next}");
            }
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

    /// A parse error said off the machine names where, never what: the
    /// line the TOML parser quotes could be any line of the file.
    #[test]
    fn a_load_error_said_elsewhere_quotes_no_line_of_the_file() {
        let text = "[review]\nmax_files = { a = 1, token = \"sk-secret\" }\n";
        let err = Config::from_toml(text, Path::new("/x/node.toml")).unwrap_err();
        assert!(err.contains("sk-secret"), "{err}");
        let shown = without_excerpt(&err);
        assert!(!shown.contains("sk-secret"), "{shown}");
        assert!(shown.contains("line 2"), "{shown}");
        assert!(shown.contains("invalid type"), "{shown}");
    }

    /// A preview is the edit without the write: the keys it would change,
    /// what the result loads as, and the same refusal a load would make.
    #[test]
    fn a_preview_says_what_an_edit_would_change_and_writes_nothing() {
        let (_dir, path) = file(COMMENTED);
        let seen = preview(
            COMMENTED,
            &[
                set("session.budget_tokens", 9),
                set(
                    "repo[1].checks",
                    toml::Value::Array(vec!["just check".into()]),
                ),
            ],
        )
        .unwrap();
        let keys: Vec<String> = seen.changes.iter().map(|c| c.key.to_string()).collect();
        assert_eq!(keys, ["repo[1].checks", "session.budget_tokens"]);
        assert_eq!(seen.config.session.budget_tokens, 9);
        assert_eq!(read(&path), COMMENTED);
        let err = preview(COMMENTED, &[set("harness.id", "nonesuch")])
            .err()
            .unwrap();
        assert!(matches!(err, EditError::Invalid(_)), "{err}");
    }

    /// A reader sees every secret, and every key the node does not know, as
    /// the placeholder; the rest as written.
    #[test]
    fn a_reader_never_sees_a_secret_or_an_unknown_value() {
        let table: toml::Table = toml::from_str(
            "node_name = \"n\"\n[qa]\nbrowser = \"sk-live\"\n[publish]\ngh = \"gh\"\n[[repo]]\npath = \"/x\"\nmystery = \"m\"\n",
        )
        .unwrap();
        let shown = redact(&[], &toml::Value::Table(table));
        assert_eq!(shown["node_name"].as_str(), Some("n"));
        assert_eq!(shown["publish"]["gh"].as_str(), Some("gh"));
        assert_eq!(shown["qa"]["browser"].as_str(), Some(REDACTED));
        assert_eq!(shown["repo"][0]["path"].as_str(), Some("/x"));
        assert_eq!(shown["repo"][0]["mystery"].as_str(), Some(REDACTED));
    }

    /// An edit that changes `[[repo]]` through `edit_live` is in effect for
    /// the running node at once, as a `[[repo]]` writer's is.
    #[test]
    fn a_live_edit_moves_the_live_repo_table_with_the_file() {
        let (_dir, path) = file(COMMENTED);
        let store = store();
        let cfg = snapshot_at(&path).unwrap().config;
        assert_eq!(cfg.repos().len(), 2);
        let outcome = edit_live_at(
            &path,
            &cfg,
            &store,
            &[Op::remove(KeyPath::parse("repo[1]").unwrap())],
            None,
            "test",
        )
        .unwrap();
        assert!(!outcome.restart_required);
        assert_eq!(cfg.repos().len(), 1);
        // A key elsewhere leaves the live table alone.
        edit_live_at(&path, &cfg, &store, &[set("node_name", "x")], None, "test").unwrap();
        assert_eq!(cfg.repos().len(), 1);
    }

    /// A table saved whole replaces exactly the version it was read from.
    #[test]
    fn a_whole_repo_table_is_saved_only_over_the_version_it_was_read_from() {
        let (_dir, path) = file(COMMENTED);
        let store = store();
        let read_at = snapshot_at(&path).unwrap();
        let mut entries = read_at.config.repo.clone();
        entries.pop();
        // Someone else edits first.
        edit_at(&path, &store, &[set("node_name", "other")], None, "test").unwrap();
        let err = replace_repos_expecting_at(
            &path,
            &read_at.config,
            &store,
            &read_at.hash,
            &entries,
            "test",
        )
        .err()
        .unwrap();
        let EditError::Conflict { current } = err else {
            panic!("{err}")
        };
        assert_eq!(current, file_hash(&path).unwrap());
        let outcome =
            replace_repos_expecting_at(&path, &read_at.config, &store, &current, &entries, "test")
                .unwrap();
        assert!(outcome.wrote());
        assert_eq!(snapshot_at(&path).unwrap().config.repo.len(), 1);
        assert_eq!(read_at.config.repos().len(), 1);
        assert!(read(&path).contains("node_name = \"other\""));
    }
}
