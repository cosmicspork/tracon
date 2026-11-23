//! The admin routes a peer may run on this node over the mesh.
//!
//! An admin peer (named in `[mesh] admin_peers`) administers this node
//! through `admin_request`, which runs one of this node's own routes as its
//! interface would. Only the routes listed here are reachable that way: the
//! actions an operator takes on a node they administer from another (plugin
//! updates, rollbacks and restarts, authority grants, reopening a channel to
//! external harnesses and its notifications, repository recipes, restarting
//! the node), and reads of the lists those act on. Nothing that changes who
//! may reach or administer the node, or where its credentials go, is here:
//! the operator token and passkeys, the mesh's membership, credentials and
//! their sharing, `[auth]` and `[mesh]`, policy signing, boundary setup, and
//! installing or removing a plugin stay with the node itself.
//!
//! The same table words the passkey prompt on the sending node and decides
//! what this node runs, so the two cannot disagree on what a route does.

use serde_json::Value;

/// What a route is: a read, which an admin peer runs with no statement, or a
/// write, worded for the prompt from its path parameters and body (or
/// refused, with why, when the body asks for more than a peer may).
#[derive(Clone, Copy)]
enum Kind {
    Read,
    Write(fn(&[String], &Value) -> Result<String, String>),
}

/// `(method, path under /api/ with {} for a parameter, kind)`.
const ROUTES: &[(&str, &str, Kind)] = &[
    ("GET", "plugins", Kind::Read),
    ("POST", "plugins/{}/update", Kind::Write(plugin_update)),
    ("POST", "plugins/{}/rollback", Kind::Write(plugin_rollback)),
    ("POST", "plugins/{}/restart", Kind::Write(plugin_restart)),
    ("GET", "authority/grants", Kind::Read),
    ("POST", "authority/grants", Kind::Write(grant_create)),
    ("DELETE", "authority/grants/{}", Kind::Write(grant_revoke)),
    ("GET", "external", Kind::Read),
    ("POST", "external/{}/stop", Kind::Write(external_stop)),
    ("POST", "external/{}/start", Kind::Write(external_start)),
    ("GET", "channels", Kind::Read),
    ("PUT", "channels/{}/bindings", Kind::Write(channel_notify)),
    ("GET", "repos/environments", Kind::Read),
    ("POST", "repos/offer", Kind::Write(recipe_offer)),
    ("GET", "repos/offers", Kind::Read),
    ("GET", "repos/offers/{}", Kind::Read),
    ("POST", "repos/offers/{}/accept", Kind::Write(recipe_accept)),
    ("GET", "restart", Kind::Read),
    ("POST", "admin/restart", Kind::Write(restart)),
    ("DELETE", "admin/restart", Kind::Write(restart_withdraw)),
];

/// What a peer's request for `method` `path` with `body` is here: `Ok(None)`
/// for a read, `Ok(Some(line))` for a write the prompt shows as `line`, or
/// why no peer may send it. `path` is the whole path (`/api/...`), with no
/// query.
pub(crate) fn describe(method: &str, path: &str, body: &Value) -> Result<Option<String>, String> {
    let (kind, params) = find(method, path)?;
    match kind {
        Kind::Read if body.is_null() => Ok(None),
        Kind::Read => Err("a read carries no body".into()),
        Kind::Write(line) => line(&params, body).map(Some),
    }
}

fn find(method: &str, path: &str) -> Result<(Kind, Vec<String>), String> {
    let refused = || {
        format!(
            "{method} {} is not an action a peer may take on this node",
            crate::http::passkey::clean_text(path, 120)
        )
    };
    let rest = path.strip_prefix("/api/").ok_or_else(refused)?;
    let segments: Vec<&str> = rest.split('/').collect();
    for (m, pattern, kind) in ROUTES {
        if *m != method {
            continue;
        }
        let shape: Vec<&str> = pattern.split('/').collect();
        if shape.len() != segments.len() {
            continue;
        }
        let mut params = Vec::new();
        let fits = shape.iter().zip(&segments).all(|(want, got)| match *want {
            "{}" if param(got) => {
                params.push((*got).to_string());
                true
            }
            "{}" => false,
            want => want == *got,
        });
        if fits {
            return Ok((*kind, params));
        }
    }
    Err(refused())
}

/// A path parameter as these routes name things (a plugin, a channel, a
/// grant or an offer): no separator, no encoding, nothing to read twice.
fn param(segment: &str) -> bool {
    !segment.is_empty()
        && segment.len() <= 128
        && segment != "."
        && segment != ".."
        && segment
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
}

fn text<'a>(body: &'a Value, key: &str) -> &'a str {
    body[key].as_str().unwrap_or_default()
}

fn plugin_update(p: &[String], body: &Value) -> Result<String, String> {
    Ok(match text(body, "channel") {
        "" => format!("Check plugin {} for an update", p[0]),
        channel => format!("Check plugin {} for an update, its card on {channel}", p[0]),
    })
}

fn plugin_rollback(p: &[String], _: &Value) -> Result<String, String> {
    Ok(format!("Roll plugin {} back to its previous version", p[0]))
}

fn plugin_restart(p: &[String], body: &Value) -> Result<String, String> {
    Ok(format!(
        "Restart plugin {} on {}",
        p[0],
        text(body, "channel")
    ))
}

fn grant_create(_: &[String], body: &Value) -> Result<String, String> {
    let mut line = format!(
        "Grant {} {} on {} in {}",
        text(body, "verdict"),
        text(body, "action"),
        text(body, "target"),
        text(body, "channel"),
    );
    if let Some(revision) = body["revision"].as_str().filter(|r| !r.is_empty()) {
        line.push_str(&format!(" at {revision}"));
    }
    if body["expires_ms"].is_number() {
        line.push_str(", until it expires");
    }
    Ok(line)
}

fn grant_revoke(p: &[String], _: &Value) -> Result<String, String> {
    Ok(format!("Revoke authority grant {}", p[0]))
}

fn external_stop(p: &[String], _: &Value) -> Result<String, String> {
    Ok(format!("Stop external harnesses on {}", p[0]))
}

fn external_start(p: &[String], _: &Value) -> Result<String, String> {
    Ok(format!("Reopen {} to external harnesses", p[0]))
}

/// A peer changes a channel's `notify` binding and nothing else: the other
/// bindings are the channel's own, handed to every member with its key, and
/// changing them is for a member's own operator.
fn channel_notify(p: &[String], body: &Value) -> Result<String, String> {
    let keys = body.as_object().ok_or("bindings must be an object")?.keys();
    for key in keys {
        if key != "notify" && !key.starts_with("notify.") {
            return Err(format!(
                "a peer changes only a channel's notify binding, not {}",
                crate::http::passkey::clean_text(key, 60)
            ));
        }
    }
    Ok(format!("Change {}'s notifications", p[0]))
}

fn recipe_offer(_: &[String], body: &Value) -> Result<String, String> {
    let what = match (text(body, "path"), text(body, "repo")) {
        ("", "") => "a repository".to_string(),
        ("", repo) => repo.to_string(),
        (path, _) => path.to_string(),
    };
    let to = match body["to_nodes"].as_array() {
        Some(nodes) => format!("{} of {}'s members", nodes.len(), text(body, "channel")),
        None => format!("{}'s members", text(body, "channel")),
    };
    Ok(format!("Offer the recipe for {what} to {to}"))
}

fn recipe_accept(p: &[String], body: &Value) -> Result<String, String> {
    Ok(match text(body, "path") {
        "" => format!("Accept repository recipe offer {} into [[repo]]", p[0]),
        path => format!(
            "Accept repository recipe offer {} into [[repo]] as {path}",
            p[0]
        ),
    })
}

fn restart(_: &[String], body: &Value) -> Result<String, String> {
    Ok(match text(body, "mode") {
        "now" => "Restart the node now, ending its sessions".into(),
        _ => "Restart the node once its sessions finish".into(),
    })
}

fn restart_withdraw(_: &[String], _: &Value) -> Result<String, String> {
    Ok("Withdraw the node's waiting restart".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn only_the_listed_routes_are_reachable_and_reads_carry_nothing() {
        assert_eq!(describe("GET", "/api/plugins", &Value::Null), Ok(None));
        assert_eq!(
            describe("POST", "/api/plugins/hora/rollback", &Value::Null),
            Ok(Some("Roll plugin hora back to its previous version".into()))
        );
        for (method, path) in [
            // Out of scope: who may reach or administer the node, and where
            // its credentials go.
            ("PUT", "/api/auth/token"),
            ("POST", "/api/passkeys/register/options"),
            ("POST", "/api/credentials/import"),
            ("POST", "/api/mesh/init"),
            ("POST", "/api/admin/mesh/hub-share"),
            ("POST", "/api/admin/policy/apply"),
            ("POST", "/api/boundary/setup"),
            ("POST", "/api/plugins"),
            ("DELETE", "/api/plugins/hora"),
            ("POST", "/api/admin/maintenance/install"),
            ("PATCH", "/api/config"),
            // A listed path with another method, or a stretched one.
            ("DELETE", "/api/plugins/hora/rollback"),
            ("POST", "/api/plugins/hora/rollback/x"),
            ("POST", "/api/plugins/../auth/rollback"),
            ("POST", "/api/plugins/a%2Fb/rollback"),
            ("POST", "/api/plugins//rollback"),
            ("POST", "api/plugins/hora/rollback"),
            ("POST", "/api/plugins/hora/rollback?x=1"),
            ("DELETE", "/api/external/personal/stop"),
        ] {
            let e = describe(method, path, &Value::Null).unwrap_err();
            assert!(
                e.contains("not an action a peer may take"),
                "{method} {path}: {e}"
            );
        }
        assert!(describe("GET", "/api/plugins", &json!({})).is_err());
    }

    #[test]
    fn a_peer_changes_only_a_channels_notify_binding() {
        let path = "/api/channels/personal/bindings";
        assert_eq!(
            describe("PUT", path, &json!({ "notify": { "approvals": false } })),
            Ok(Some("Change personal's notifications".into()))
        );
        assert!(describe("PUT", path, &json!({ "notify.approvals": true })).is_ok());
        for body in [
            json!({ "notify": {}, "repos": ["x"] }),
            json!({ "external_stopped": false }),
            json!([1]),
        ] {
            assert!(describe("PUT", path, &body).is_err(), "{body}");
        }
    }

    #[test]
    fn each_write_says_what_it_does() {
        let line =
            |method: &str, path: &str, body: Value| describe(method, path, &body).unwrap().unwrap();
        assert_eq!(
            line(
                "POST",
                "/api/plugins/hora/restart",
                json!({ "channel": "personal" })
            ),
            "Restart plugin hora on personal"
        );
        assert_eq!(
            line(
                "POST",
                "/api/authority/grants",
                json!({ "verdict": "allow", "action": "merge", "target": "cosmicspork/tracon", "channel": "personal", "revision": "abc" })
            ),
            "Grant allow merge on cosmicspork/tracon in personal at abc"
        );
        assert_eq!(
            line(
                "POST",
                "/api/admin/restart",
                json!({ "mode": "now", "expected_hash": "h", "confirm": true })
            ),
            "Restart the node now, ending its sessions"
        );
        assert_eq!(
            line("POST", "/api/external/personal/start", Value::Null),
            "Reopen personal to external harnesses"
        );
    }
}
