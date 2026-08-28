//! The work ledger's pure half: ids minted offline, and the ready-work order
//! every replica computes identically from the same rows.
//!
//! Beads-inspired: the tool does the topological thinking and serves only
//! unblocked items; the model picks. Nothing here touches SQLite, so the node
//! and the hub call the same function over rows they each hold.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const OPEN: &str = "open";
pub const CLOSED: &str = "closed";

/// A ledger row as both sides read it. `deps` are the ids this item waits on.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WorkItem {
    pub id: String,
    pub channel: String,
    pub project_id: Option<String>,
    pub title: String,
    pub body: String,
    pub state: String,
    pub priority: i64,
    pub deps: Vec<String>,
    pub discovered_from: Option<String>,
    pub discovered_by_session: Option<String>,
    pub phase_plan_slug: Option<String>,
    pub closed_by_session: Option<String>,
    pub created_ms: i64,
    pub updated_ms: i64,
}

/// `sha256("tracon/work-item" ‖ 0x1f ‖ channel ‖ 0x1f ‖ project ‖ 0x1f ‖ site ‖
/// 0x1f ‖ created_ms ‖ 0x1f ‖ title)`, hex. Two nodes minting during a hub
/// outage cannot collide: the site is in the preimage.
pub fn item_id(
    channel: &str,
    project_id: &str,
    site: &str,
    created_ms: i64,
    title: &str,
) -> String {
    let mut h = Sha256::new();
    for (i, part) in [
        "tracon/work-item",
        channel,
        project_id,
        site,
        &created_ms.to_string(),
        title,
    ]
    .iter()
    .enumerate()
    {
        if i > 0 {
            h.update([0x1f]);
        }
        h.update(part.as_bytes());
    }
    hex::encode(h.finalize())
}

/// Why an open item is not ready.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Blocker {
    /// Waits on an open item.
    Open { id: String },
    /// Waits on an id this replica has never seen: treated as blocking, and
    /// said so, rather than silently ready.
    Unknown { id: String },
    /// Part of a dependency cycle; nothing in it can ever become ready.
    Cycle,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum Readiness {
    Ready,
    Blocked { by: Vec<Blocker> },
    Closed,
}

/// Every item with its derived readiness, in the deterministic order:
/// a Kahn topological pass over open items (ids break ties), then ready
/// items by `(priority desc, created_ms asc, id asc)`. Closed items keep
/// their place at the end, newest closed first.
pub fn status(items: &[WorkItem]) -> Vec<(WorkItem, Readiness)> {
    let by_id: BTreeMap<&str, &WorkItem> = items.iter().map(|i| (i.id.as_str(), i)).collect();
    let open: BTreeSet<&str> = items
        .iter()
        .filter(|i| i.state != CLOSED)
        .map(|i| i.id.as_str())
        .collect();

    // Kahn over the open subgraph: an open item's in-degree counts only its
    // open, known deps. What never reaches zero is in (or behind) a cycle.
    let mut indeg: BTreeMap<&str, usize> = BTreeMap::new();
    let mut rev: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for id in &open {
        let item = by_id[id];
        let mut n = 0;
        for d in &item.deps {
            if open.contains(d.as_str()) {
                n += 1;
                rev.entry(d.as_str()).or_default().push(id);
            }
        }
        indeg.insert(id, n);
    }
    let mut queue: BTreeSet<&str> = indeg
        .iter()
        .filter(|(_, n)| **n == 0)
        .map(|(id, _)| *id)
        .collect();
    let mut order: Vec<&str> = Vec::new();
    while let Some(id) = queue.iter().next().copied() {
        queue.remove(id);
        order.push(id);
        for next in rev.get(id).into_iter().flatten() {
            let n = indeg.get_mut(next).expect("open item");
            *n -= 1;
            if *n == 0 {
                queue.insert(next);
            }
        }
    }
    let in_cycle: BTreeSet<&str> = open
        .iter()
        .filter(|id| !order.contains(*id))
        .copied()
        .collect();

    let readiness = |item: &WorkItem| -> Readiness {
        if item.state == CLOSED {
            return Readiness::Closed;
        }
        if in_cycle.contains(item.id.as_str()) {
            return Readiness::Blocked {
                by: vec![Blocker::Cycle],
            };
        }
        let mut by = Vec::new();
        for d in &item.deps {
            match by_id.get(d.as_str()) {
                Some(dep) if dep.state == CLOSED => {}
                Some(_) => by.push(Blocker::Open { id: d.clone() }),
                None => by.push(Blocker::Unknown { id: d.clone() }),
            }
        }
        if by.is_empty() {
            Readiness::Ready
        } else {
            Readiness::Blocked { by }
        }
    };

    let mut ready: Vec<&WorkItem> = Vec::new();
    let mut blocked: Vec<&WorkItem> = Vec::new();
    for id in &order {
        let item = by_id[id];
        match readiness(item) {
            Readiness::Ready => ready.push(item),
            _ => blocked.push(item),
        }
    }
    for id in &in_cycle {
        blocked.push(by_id[id]);
    }
    ready.sort_by(|a, b| {
        b.priority
            .cmp(&a.priority)
            .then(a.created_ms.cmp(&b.created_ms))
            .then(a.id.cmp(&b.id))
    });
    let mut closed: Vec<&WorkItem> = items.iter().filter(|i| i.state == CLOSED).collect();
    closed.sort_by(|a, b| b.updated_ms.cmp(&a.updated_ms).then(a.id.cmp(&b.id)));

    ready
        .into_iter()
        .chain(blocked)
        .chain(closed)
        .map(|i| (i.clone(), readiness(i)))
        .collect()
}

/// Only the ready items, in order.
pub fn ready(items: &[WorkItem]) -> Vec<WorkItem> {
    status(items)
        .into_iter()
        .filter(|(_, r)| *r == Readiness::Ready)
        .map(|(i, _)| i)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(id: &str, deps: &[&str], priority: i64, created_ms: i64) -> WorkItem {
        WorkItem {
            id: id.into(),
            channel: "personal".into(),
            project_id: None,
            title: id.into(),
            body: String::new(),
            state: OPEN.into(),
            priority,
            deps: deps.iter().map(|d| d.to_string()).collect(),
            discovered_from: None,
            discovered_by_session: None,
            phase_plan_slug: None,
            closed_by_session: None,
            created_ms,
            updated_ms: created_ms,
        }
    }

    #[test]
    fn id_is_a_pinned_vector_scoped_by_site_and_channel() {
        let a = item_id("personal", "p", "A", 1000, "Title");
        assert_eq!(
            a,
            "6ad8350b711cc2a26e6297d3c7a6ee588e19b267067d9e2014a8744d32bd3704"
        );
        assert_ne!(a, item_id("personal", "p", "B", 1000, "Title"));
        assert_ne!(a, item_id("work", "p", "A", 1000, "Title"));
    }

    #[test]
    fn ready_order_is_deterministic_across_permutations() {
        let base = vec![
            item("a", &[], 0, 10),
            item("b", &["a"], 5, 20),
            item("c", &[], 5, 30),
            item("d", &[], 5, 30),
            item("e", &["zzz"], 9, 5),
        ];
        let expect: Vec<String> = ready(&base).into_iter().map(|i| i.id).collect();
        assert_eq!(expect, vec!["c", "d", "a"]);
        let mut perm = base.clone();
        for _ in 0..6 {
            perm.rotate_left(1);
            perm.swap(0, 2);
            let got: Vec<String> = ready(&perm).into_iter().map(|i| i.id).collect();
            assert_eq!(got, expect);
        }
        let st = status(&base);
        let e = st.iter().find(|(i, _)| i.id == "e").unwrap();
        assert_eq!(
            e.1,
            Readiness::Blocked {
                by: vec![Blocker::Unknown { id: "zzz".into() }]
            }
        );
        let b = st.iter().find(|(i, _)| i.id == "b").unwrap();
        assert_eq!(
            b.1,
            Readiness::Blocked {
                by: vec![Blocker::Open { id: "a".into() }]
            }
        );
    }

    #[test]
    fn closing_a_dep_frees_its_dependents_and_cycles_stay_blocked() {
        let mut items = vec![
            item("a", &[], 0, 1),
            item("b", &["a"], 0, 2),
            item("x", &["y"], 0, 3),
            item("y", &["x"], 0, 4),
            item("z", &["x"], 0, 5),
        ];
        let r: Vec<String> = ready(&items).into_iter().map(|i| i.id).collect();
        assert_eq!(r, vec!["a"]);
        items[0].state = CLOSED.into();
        let r: Vec<String> = ready(&items).into_iter().map(|i| i.id).collect();
        assert_eq!(r, vec!["b"]);
        let st = status(&items);
        for id in ["x", "y", "z"] {
            let (_, r) = st.iter().find(|(i, _)| i.id == id).unwrap();
            assert!(matches!(r, Readiness::Blocked { .. }), "{id}: {r:?}");
        }
        let (_, x) = st.iter().find(|(i, _)| i.id == "x").unwrap();
        assert_eq!(
            *x,
            Readiness::Blocked {
                by: vec![Blocker::Cycle]
            }
        );
        // Closed items come last.
        assert_eq!(st.last().unwrap().0.id, "a");
        assert_eq!(st.last().unwrap().1, Readiness::Closed);
    }
}
