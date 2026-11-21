//! What a call to a plugin's tool is lent: the one function both the
//! dispatch and the approval card read, so what a card says Allow lends is
//! what the run is lent.
//!
//! Each binding the channel holds belongs to the first credential the
//! manifest declares that matches it, by name or pattern; a binding none
//! matches is never lent. A credential with `select` lends a call only the
//! binding the call's argument names (`<name>-<value>`, or `<name>` when the
//! call names none); one without lends every binding it owns.

use serde_json::Value;

use super::manifest::{Credential, Lend, Manifest};

/// What one declared credential lends a call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Lent<'m> {
    pub credential: &'m Credential,
    /// The bindings lent, by name, in the channel's order.
    pub bindings: Vec<String>,
    /// Why the call cannot be lent what it names, so nothing runs: the
    /// binding its argument names is not one the channel binds for this
    /// credential, or the argument is not a binding's name.
    pub refusal: Option<String>,
}

/// What a call with `args` is lent of each credential `manifest` declares,
/// from the bindings `available` to its channel. One lent only on approval
/// lends nothing, and refuses nothing, unless the call is `approved`.
///
/// `args` is `None` for a request that is not a tool call (a plugin page's),
/// which has no argument to select by and is lent every binding a
/// credential owns.
pub fn lending<'m>(
    manifest: &'m Manifest,
    available: &[&str],
    args: Option<&Value>,
    approved: bool,
) -> Vec<Lent<'m>> {
    let owner = |binding: &str| manifest.credentials.iter().position(|c| c.matches(binding));
    manifest
        .credentials
        .iter()
        .enumerate()
        .map(|(index, credential)| {
            let owned: Vec<&str> = available
                .iter()
                .copied()
                .filter(|b| owner(b) == Some(index))
                .collect();
            let mut lent = Lent {
                credential,
                bindings: Vec::new(),
                refusal: None,
            };
            if credential.lend == Lend::Approved && !approved {
                return lent;
            }
            match (&credential.select, args) {
                (Some(select), Some(args)) => match selected(credential, select, args) {
                    Ok(None) => {
                        // The call names none: the credential's own binding,
                        // when the channel binds it.
                        if owned.contains(&credential.name.as_str()) {
                            lent.bindings.push(credential.name.clone());
                        }
                    }
                    Ok(Some(binding)) if owned.contains(&binding.as_str()) => {
                        lent.bindings.push(binding);
                    }
                    Ok(Some(binding)) => {
                        lent.refusal = Some(if available.contains(&binding.as_str()) {
                            format!(
                                "the call's {select} names binding {binding}, which is not \
                                 one credential {} may be lent",
                                credential.name
                            )
                        } else {
                            format!(
                                "the call's {select} names binding {binding}, which this \
                                 channel does not bind"
                            )
                        });
                    }
                    Err(why) => lent.refusal = Some(why),
                },
                _ => lent.bindings = owned.iter().map(|b| b.to_string()).collect(),
            }
            lent
        })
        .collect()
}

/// The binding `args` names for `credential` by its `select` argument:
/// `None` when it names none (absent, null or empty).
fn selected(credential: &Credential, select: &str, args: &Value) -> Result<Option<String>, String> {
    match args.get(select) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) if s.is_empty() => Ok(None),
        Some(Value::String(s))
            if s.bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.')) =>
        {
            Ok(Some(format!("{}-{s}", credential.name)))
        }
        Some(_) => Err(format!(
            "the call's {select} must be a string of letters, digits, '-', '_' or '.': it names \
             the {} binding the call is lent",
            credential.name
        )),
    }
}

/// The first reason in `lent` that the call cannot run, if any.
pub fn refusal(lent: &[Lent<'_>]) -> Option<String> {
    lent.iter().find_map(|l| l.refusal.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn manifest(select: bool) -> Manifest {
        let select = if select { "select = \"profile\"\n" } else { "" };
        Manifest::parse(&format!(
            r#"
name = "sql-review"
version = "0.3.0"
contract = 1
exec = ["bin/sql-review"]

[[credential]]
name = "sql-read"
pattern = "sql-read-*"
{select}
[[credential]]
name = "sql-write"
pattern = "sql-write-*"
lend = "approved"
{select}
[[tool]]
name = "write"
hint = "write"
"#
        ))
        .unwrap()
    }

    const BOUND: &[&str] = &[
        "github",
        "sql-read",
        "sql-read-tst",
        "sql-read-prd",
        "sql-write-tst",
        "sql-write-prd",
    ];

    fn names(lent: &[Lent<'_>]) -> Vec<String> {
        lent.iter().flat_map(|l| l.bindings.clone()).collect()
    }

    #[test]
    fn without_select_every_owned_binding_is_lent() {
        let m = manifest(false);
        let args = json!({ "profile": "tst" });
        assert_eq!(
            names(&lending(&m, BOUND, Some(&args), true)),
            [
                "sql-read",
                "sql-read-tst",
                "sql-read-prd",
                "sql-write-tst",
                "sql-write-prd"
            ]
        );
        assert_eq!(
            names(&lending(&m, BOUND, Some(&args), false)),
            ["sql-read", "sql-read-tst", "sql-read-prd"]
        );
    }

    #[test]
    fn a_selecting_credential_lends_only_the_binding_the_call_names() {
        let m = manifest(true);
        let tst = json!({ "profile": "tst", "sql": "UPDATE a" });
        assert_eq!(
            names(&lending(&m, BOUND, Some(&tst), true)),
            ["sql-read-tst", "sql-write-tst"]
        );
        assert_eq!(
            names(&lending(&m, BOUND, Some(&tst), false)),
            ["sql-read-tst"]
        );
        // Naming none: the credential's own binding, where bound.
        let none = json!({ "sql": "SELECT 1" });
        let lent = lending(&m, BOUND, Some(&none), true);
        assert_eq!(names(&lent), ["sql-read"]);
        assert_eq!(refusal(&lent), None);
        // A page's request has no argument to select by.
        assert_eq!(names(&lending(&m, BOUND, None, false)).len(), 3);
    }

    #[test]
    fn a_binding_the_channel_does_not_hold_is_refused_naming_it() {
        let m = manifest(true);
        let nope = json!({ "profile": "nope" });
        let lent = lending(&m, BOUND, Some(&nope), true);
        assert!(names(&lent).is_empty());
        assert_eq!(
            refusal(&lent).unwrap(),
            "the call's profile names binding sql-read-nope, which this channel does not bind"
        );
        // Unapproved, the approved-only credential is not asked for.
        let read_only = json!({ "profile": "ro" });
        let bound = ["sql-read-ro", "sql-write-tst"];
        assert_eq!(refusal(&lending(&m, &bound, Some(&read_only), false)), None);
        assert!(refusal(&lending(&m, &bound, Some(&read_only), true))
            .unwrap()
            .contains("sql-write-ro"));
        for bad in [json!({ "profile": "a/b" }), json!({ "profile": 3 })] {
            assert!(refusal(&lending(&m, BOUND, Some(&bad), false))
                .unwrap()
                .contains("must be a string"));
        }
    }

    #[test]
    fn a_selected_binding_another_credential_owns_is_not_lent() {
        // `sql-*` would select `sql-write-tst` from profile `write-tst`, but
        // that binding is the approved-only credential's.
        let m = Manifest::parse(
            r#"
name = "sql-review"
version = "0.3.0"
contract = 1
exec = ["bin/sql-review"]

[[credential]]
name = "sql-write"
pattern = "sql-write-*"
lend = "approved"

[[credential]]
name = "sql"
pattern = "sql-*"
select = "profile"
"#,
        )
        .unwrap();
        let args = json!({ "profile": "write-tst" });
        let lent = lending(&m, BOUND, Some(&args), false);
        assert!(names(&lent).is_empty());
        assert!(refusal(&lent)
            .unwrap()
            .contains("not one credential sql may be lent"));
    }
}
