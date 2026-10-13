//! The line a passkey prompt shows for a change to `node.toml`.
//!
//! The prompt is the last thing between a phished confirmation and a write,
//! so it is built to be read by someone who did not make the request: what
//! could change what the node runs, trusts, reads or sends comes first,
//! with its new value, and is never cut short. A change too large to show
//! that way is refused, to be split. Routine keys follow, as many as fit,
//! and how many more. An entry of an array of tables is named by its own
//! `path`, `name` or `id`, never by an index alone, and its changed fields
//! are listed.

use crate::config::edit::{Change, Seg};
use crate::config_schema::{self, KEYS};

use super::passkey::{clean_text, MAX_SUMMARY};

const HEAD: &str = "Change node.toml: ";
/// The longest a map entry's own name (`providers.<name>`,
/// `github.dispatch.<owner/name>`) is shown.
const MAX_WILD: usize = 24;
/// The longest any other key segment is shown.
const MAX_SEGMENT: usize = 48;
/// The longest a value is shown.
const MAX_VALUE: usize = 72;

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
    "repo.image",
    "repo.checks",
    "service.image",
    "service.command",
    "supervision.checks",
    "launch.plugins",
    "embed.base_url",
    "embed.api_key_file",
    "embed.provider",
    "github.dispatch",
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

fn cut(text: &str, max: usize) -> String {
    let clean = clean_text(text, usize::MAX);
    match clean.chars().count() > max {
        true => format!("{}...", clean.chars().take(max).collect::<String>()),
        false => clean,
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
                let entry = docs.and_then(|(before, after)| {
                    at(after, &segs[..=i])
                        .or_else(|| at(before, &segs[..=i]))
                        .and_then(toml::Value::as_table)
                        .and_then(entry_name)
                });
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

fn show_value(at: &[&str], value: &toml::Value) -> String {
    if config_schema::lookup(at).secret || !config_schema::known(at) {
        return "(a new value, not shown)".into();
    }
    let text = match value {
        toml::Value::String(s) => s.clone(),
        toml::Value::Array(items) => format!(
            "[{}]",
            items
                .iter()
                .map(|v| match v {
                    toml::Value::String(s) => s.clone(),
                    other => other.to_string(),
                })
                .collect::<Vec<_>>()
                .join(", ")
        ),
        other => other.to_string(),
    };
    cut(&text, MAX_VALUE)
}

/// What names an entry of an array of tables, for the operator.
fn entry_name(entry: &toml::Table) -> Option<String> {
    ["path", "name", "id"]
        .iter()
        .find_map(|k| entry.get(*k).and_then(toml::Value::as_str))
        .map(|name| cut(name, MAX_SEGMENT))
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

/// One field of an entry, as the prompt lists it: with its value when it is
/// sensitive.
fn field(array: &[&str], key: &str, value: Option<&toml::Value>) -> (bool, String) {
    let mut full = array.to_vec();
    full.push(key);
    let sensitive = sensitive(&full);
    let shown = cut(key, MAX_SEGMENT);
    match (sensitive, value) {
        (true, Some(v)) => (true, format!("{shown} = {}", show_value(&full, v))),
        (true, None) => (true, format!("{shown} removed")),
        (false, _) => (false, shown),
    }
}

/// An array of tables that gained, lost or reordered entries, entry by
/// entry: each added or changed entry by its name with its fields, each
/// removed one by its name.
fn array_items(key: &[Seg], before: &[&toml::Table], after: &[&toml::Table], out: &mut Vec<Item>) {
    let names: Vec<&str> = key
        .iter()
        .filter_map(|s| match s {
            Seg::Key(k) => Some(k.as_str()),
            Seg::Index(_) => None,
        })
        .collect();
    let shown = show_key(key, None);
    let mut listed = false;
    for (i, now) in after.iter().enumerate() {
        if before.contains(now) {
            continue;
        }
        let name = entry_name(now).unwrap_or_else(|| format!("#{i}"));
        let was = before
            .iter()
            .find(|b| entry_name(b).is_some() && entry_name(b) == entry_name(now));
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
                && entry_name(now).is_some()
                && ["path", "name", "id"].contains(&k.as_str())
            {
                continue;
            }
            fields.push(field(&names, k, value));
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
        let name = entry_name(was);
        if name.is_some() && after.iter().any(|a| entry_name(a) == name) {
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
/// fit a prompt: such an edit is to be split, never shown cut short.
pub fn summarize(
    before: &toml::Table,
    after: &toml::Table,
    changes: &[Change],
) -> Result<String, String> {
    let mut items = Vec::new();
    for change in changes {
        let segs = &change.key.0;
        let names: Vec<&str> = change.key.schema_key();
        if let (Some(was), Some(now)) = (
            tables(change.before.as_ref()),
            tables(change.after.as_ref()),
        ) {
            if !(was.is_empty() && now.is_empty()) {
                array_items(segs, &was, &now, &mut items);
                continue;
            }
        }
        // A key inside an entry: named by its entry.
        let shown = show_key(segs, Some((before, after)));
        let is_sensitive = sensitive(&names);
        let text = match (is_sensitive, &change.after) {
            (true, Some(value)) => format!("{shown} = {}", show_value(&names, value)),
            (true, None) => format!("{shown} removed"),
            (false, _) => shown,
        };
        items.push(Item {
            sensitive: is_sensitive,
            text,
        });
    }

    let (loud, quiet): (Vec<Item>, Vec<Item>) = items.into_iter().partition(|i| i.sensitive);
    let mut line = HEAD.to_string();
    line.push_str(
        &loud
            .iter()
            .map(|i| i.text.as_str())
            .collect::<Vec<_>>()
            .join("; "),
    );
    if line.chars().count() > MAX_SUMMARY {
        return Err(format!(
            "this edit changes more sensitive keys than one confirmation can show ({}); split it into smaller edits",
            loud.len()
        ));
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
            > MAX_SUMMARY
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
        summarize(&before, &after, &seen.changes)
    }

    fn set(key: &str, value: impl Into<toml::Value>) -> Op {
        Op::set(KeyPath::parse(key).unwrap(), value)
    }

    #[test]
    fn a_sensitive_key_comes_first_with_its_value_however_many_routine_keys_there_are() {
        let mut ops = Vec::new();
        for key in [
            "session.budget_tokens",
            "session.permission_timeout_secs",
            "session.approval_expiry_secs",
            "session.claim_grace_secs",
            "review.max_diff_lines",
            "review.max_files",
            "mesh.heartbeat_secs",
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
        assert!(line.starts_with("Change node.toml: "), "{line}");
        let gh = line.find("publish.gh = /opt/evil/gh").expect(&line);
        let budget = line.find("session.budget_tokens");
        assert!(budget.is_none_or(|b| gh < b), "{line}");
        assert!(line.contains("mesh.heartbeat_secs = 5"), "{line}");
        assert!(line.ends_with("more"), "{line}");
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
        assert!(line.contains("repo /src/app added: "), "{line}");
        assert!(line.contains("prepare_credentials = [NOVA]"), "{line}");
        assert!(line.contains("prepare = [composer install]"), "{line}");
        assert!(line.contains("timeout_secs"), "{line}");
        assert!(
            !line.contains("/src/old"),
            "an unchanged entry is not listed: {line}"
        );

        let line = prompt(
            "[[repo]]\npath = \"/src/old\"\n[[repo]]\npath = \"/src/app\"\n",
            &[Op::remove(KeyPath::parse("repo[0]").unwrap())],
        )
        .unwrap();
        assert_eq!(line, "Change node.toml: repo /src/old removed");

        // A field of an entry is named by its entry.
        let line = prompt(
            "[[repo]]\npath = \"/src/app\"\n",
            &[set(
                "repo[0].egress",
                toml::Value::Array(vec!["crates".into()]),
            )],
        )
        .unwrap();
        assert_eq!(
            line,
            "Change node.toml: repo[0] (/src/app).egress = [crates]"
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
        assert!(line.contains("= https://h.example/v1"), "{line}");
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
            &["embed", "base_url"],
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
        ] {
            assert!(!sensitive(key), "{key:?}");
        }
    }
}
