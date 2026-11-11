//! A consumer's side of templates made by peers (`[digitalocean] templates
//! = "peer"`): what each builder says in its hello of the templates it
//! holds, and which of them this node has asked for.
//!
//! Only a hello the mirror verified as the sender's own is recorded (a node
//! speaks only for itself), and only while the sender keeps saying hello:
//! one not heard from within [`FRESH`] is taken to hold nothing.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use super::shared::{Advert, AdvertState, Adverts};
use super::templates::TemplateState;

/// How long a builder's word stands without another hello: a few of its
/// heartbeats.
pub const FRESH: Duration = Duration::from_secs(5 * 60);

/// How often a template that is not ready, or that no builder holds, is
/// asked for again.
pub const ASK_AGAIN: Duration = Duration::from_secs(15 * 60);

/// How often a failed one is.
pub const ASK_AFTER_FAILURE: Duration = Duration::from_secs(3600);

/// How often a ready one in use is asked for, so its builder keeps it.
pub const TOUCH_EVERY: Duration = Duration::from_secs(24 * 3600);

#[derive(Debug, Clone)]
struct Builder {
    name: String,
    seen_ms: i64,
    templates: Vec<Advert>,
}

/// What this node asked one builder for one key, and what came back.
#[derive(Debug, Clone)]
struct Asked {
    peer: String,
    at_ms: i64,
    refused: Option<String>,
}

#[derive(Default)]
struct Peers {
    builders: HashMap<String, Builder>,
    asked: HashMap<String, Asked>,
}

fn peers() -> &'static Mutex<Peers> {
    static PEERS: std::sync::OnceLock<Mutex<Peers>> = std::sync::OnceLock::new();
    PEERS.get_or_init(Default::default)
}

/// Record `sender`'s word from its verified hello: `adverts` is its
/// `do_templates`, absent for a node that builds none.
pub fn record(sender: &str, name: &str, adverts: Option<&serde_json::Value>, now: i64) {
    let mut peers = peers().lock().unwrap();
    match adverts.and_then(|v| serde_json::from_value::<Adverts>(v.clone()).ok()) {
        Some(adverts) => {
            let templates = adverts
                .templates
                .into_iter()
                .filter(|a| super::portable::valid_key(&a.key) && template_name(&a.name))
                .collect();
            peers.builders.insert(
                sender.to_string(),
                Builder {
                    name: name.to_string(),
                    seen_ms: now,
                    templates,
                },
            );
        }
        None => {
            peers.builders.remove(sender);
        }
    }
}

/// Whether a builder's word names a template as the runtime names one:
/// what goes into each session's manifest here.
fn template_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 63
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// Whether `id` (a node id or name) is the builder `wanted` names.
fn names(id: &str, builder: &Builder, wanted: &str) -> bool {
    id == wanted || builder.name == wanted
}

fn fresh(builder: &Builder, now: i64) -> bool {
    now - builder.seen_ms < FRESH.as_millis() as i64
}

/// The builders heard from lately that `only` allows, by id.
fn candidates<'a>(
    peers: &'a Peers,
    only: Option<&'a str>,
    now: i64,
) -> impl Iterator<Item = (&'a String, &'a Builder)> + 'a {
    let mut found: Vec<_> = peers
        .builders
        .iter()
        .filter(move |(id, b)| fresh(b, now) && only.is_none_or(|w| names(id, b, w)))
        .collect();
    found.sort_by(|a, b| a.0.cmp(b.0));
    found.into_iter()
}

/// A ready template for `key` and the builder holding it, `(id, name,
/// template)`.
pub fn ready(key: &str, only: Option<&str>, now: i64) -> Option<(String, String, String)> {
    let peers = peers().lock().unwrap();
    let found = candidates(&peers, only, now).find_map(|(id, b)| {
        b.templates
            .iter()
            .find(|a| a.key == key && a.state == AdvertState::Ready)
            .map(|a| (id.clone(), b.name.clone(), a.name.clone()))
    });
    found
}

/// The builder to ask for `key`, `(id, name)`: the one `only` names, else
/// one that already lists it, else the first heard from.
pub fn builder_for(key: &str, only: Option<&str>, now: i64) -> Option<(String, String)> {
    let peers = peers().lock().unwrap();
    let all: Vec<_> = candidates(&peers, only, now).collect();
    all.iter()
        .find(|(_, b)| b.templates.iter().any(|a| a.key == key))
        .or_else(|| all.first())
        .map(|(id, b)| ((*id).clone(), b.name.clone()))
}

/// Where `key`'s template stands, as the builders say and as this node
/// asked.
pub fn state(key: &str, only: Option<&str>, now: i64) -> TemplateState {
    let peers = peers().lock().unwrap();
    let all: Vec<_> = candidates(&peers, only, now).collect();
    let listed = all.iter().find_map(|(_, b)| {
        b.templates
            .iter()
            .filter(|a| a.key == key)
            .min_by_key(|a| match a.state {
                AdvertState::Ready => 0,
                AdvertState::Building => 1,
                AdvertState::Preparing => 2,
                AdvertState::Failed => 3,
            })
            .map(|a| (b.name.clone(), a.clone()))
    });
    match listed {
        Some((_, a)) if a.state == AdvertState::Ready => TemplateState::Ready {
            template_id: a.name,
        },
        Some((peer, a)) if a.state == AdvertState::Failed => TemplateState::Failed {
            reason: format!(
                "on {peer}: {}",
                a.reason.unwrap_or_else(|| "it failed".into())
            ),
        },
        Some((peer, a)) => TemplateState::OnPeer {
            peer,
            detail: match a.state {
                AdvertState::Building => "building",
                _ => "preparing",
            }
            .into(),
        },
        None => match (peers.asked.get(key), all.is_empty()) {
            (
                Some(Asked {
                    refused: Some(why),
                    peer,
                    ..
                }),
                _,
            ) => TemplateState::Failed {
                reason: format!("{peer} refused it: {why}"),
            },
            (Some(asked), _) => TemplateState::OnPeer {
                peer: asked.peer.clone(),
                detail: "asked for".into(),
            },
            (None, true) => TemplateState::NoBuilder {
                named: only.map(str::to_string),
            },
            (None, false) => TemplateState::Unknown,
        },
    }
}

/// Whether `key` should be asked for now, given where it stands.
pub fn should_ask(key: &str, state: &TemplateState, now: i64) -> bool {
    let since = peers()
        .lock()
        .unwrap()
        .asked
        .get(key)
        .map(|a| now - a.at_ms);
    let after = match state {
        TemplateState::Ready { .. } => TOUCH_EVERY,
        TemplateState::Failed { .. } => ASK_AFTER_FAILURE,
        TemplateState::NoBuilder { .. } => return false,
        _ => ASK_AGAIN,
    };
    since.is_none_or(|since| since >= after.as_millis() as i64)
}

/// This node asked `peer` (by name) for `key` at `now`.
pub fn asked(key: &str, peer: &str, now: i64) {
    peers().lock().unwrap().asked.insert(
        key.to_string(),
        Asked {
            peer: peer.to_string(),
            at_ms: now,
            refused: None,
        },
    );
}

/// What `peer` answered: a refusal is shown until it is asked again.
pub fn answered(key: &str, peer: &str, refused: Option<String>) {
    if let Some(asked) = peers()
        .lock()
        .unwrap()
        .asked
        .get_mut(key)
        .filter(|a| a.peer == peer)
    {
        asked.refused = refused;
    }
}

#[cfg(test)]
pub(crate) fn clear() {
    *peers().lock().unwrap() = Peers::default();
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    fn advert(state: &str) -> serde_json::Value {
        serde_json::json!({"templates": [
            {"key": KEY, "name": "tracon-tp-0123456789abcdef", "state": state},
            {"key": "not a key", "name": "x", "state": "ready"},
        ]})
    }

    /// A builder's word, while it keeps saying hello, is what the template's
    /// state is; asking is paced by it; and a builder named in the settings
    /// is the only one taken.
    #[test]
    fn a_builders_hello_says_where_a_template_stands() {
        clear();
        let now = 1_000_000;
        assert_eq!(
            state(KEY, None, now),
            TemplateState::NoBuilder { named: None }
        );
        record("node-b", "builder", Some(&advert("building")), now);
        assert_eq!(
            state(KEY, None, now),
            TemplateState::OnPeer {
                peer: "builder".into(),
                detail: "building".into()
            }
        );
        assert_eq!(ready(KEY, None, now), None);
        assert!(should_ask(KEY, &state(KEY, None, now), now));
        asked(KEY, "builder", now);
        assert!(!should_ask(KEY, &state(KEY, None, now), now + 1));

        record("node-b", "builder", Some(&advert("ready")), now);
        assert_eq!(
            ready(KEY, None, now),
            Some((
                "node-b".into(),
                "builder".into(),
                "tracon-tp-0123456789abcdef".into()
            ))
        );
        assert_eq!(ready(KEY, Some("builder"), now).unwrap().0, "node-b");
        assert_eq!(ready(KEY, Some("node-b"), now).unwrap().0, "node-b");
        assert_eq!(ready(KEY, Some("other"), now), None);
        let touched = now + TOUCH_EVERY.as_millis() as i64;
        assert!(!should_ask(KEY, &state(KEY, None, now), now + 1));
        assert!(should_ask(KEY, &state(KEY, None, touched), touched));
        // Gone quiet: its word lapses.
        let later = now + FRESH.as_millis() as i64;
        assert_eq!(ready(KEY, None, later), None);
        // Builds no more.
        record("node-b", "builder", None, now);
        assert_eq!(builder_for(KEY, None, now), None);

        record(
            "node-c",
            "other",
            Some(&serde_json::json!({"templates": []})),
            now,
        );
        assert_eq!(
            builder_for(KEY, None, now),
            Some(("node-c".into(), "other".into()))
        );
        answered(KEY, "other", Some("no recipe".into()));
        // Asked of another peer: its answer is not this one's.
        assert!(matches!(
            state(KEY, None, now),
            TemplateState::OnPeer { .. }
        ));
        asked(KEY, "other", now);
        answered(KEY, "other", Some("no recipe".into()));
        assert!(matches!(
            state(KEY, None, now),
            TemplateState::Failed { .. }
        ));
        clear();
    }
}
