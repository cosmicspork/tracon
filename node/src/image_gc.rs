//! Which images a node may remove, on any provider that keeps an image store.
//!
//! A container host's image store is shared: two nodes, or a node and a test,
//! can build under the same names (a repository image's name carries no
//! node). So ownership is a label, not a name. Every image a node builds or
//! commits — a repository's image and the `-src` image under it, a layered
//! `-claude`/`-native` image, a build-steps image, a prepared snapshot —
//! carries [`owner_labels`]: the node's `[boundary] resource_prefix` and its
//! node id. A sweep offers only images carrying this node's pair. An image
//! with no owner labels at all, but named the way this node names its own, is
//! from before the labels (or from a node that did not know its id yet): it is
//! listed as `unowned` and removed only when the operator adopts it.
//!
//! Labels pass to an image built or committed on a labelled one, so a label
//! alone does not make an image the node's: it also has to carry a name the
//! node gives its images, or be a `<none>` image nothing is built on.
//!
//! What can go, by class:
//!
//! - **base**: a stock base other than the one this node pins, unless an image
//!   that stays is built on it. Pulled, so it carries no node's label: only a
//!   node with the default prefix (the store's own node) takes it for its own,
//!   and a node with a prefix of its own only when it adopts unlabelled images.
//! - **repository**: a repository image no current build, snapshot or open
//!   session uses.
//! - **snapshot**: a prepared snapshot that is no repository's current one.
//! - **dangling**: an image of this node's whose tag a later build took.
//! - **legacy**: with sandbox images, the harness images and the layered and
//!   `-src` repository images nothing runs any more; only when asked for.
//!
//! Nothing a container uses (running or stopped, any node's) is removed, and
//! nothing any remaining image is built on. An unnamed intermediate image (a
//! build's layer) goes with the last image built on it, as the store prunes
//! it, so it never holds what it stands on once those go. Removing can still
//! free more than a list foresaw (a container gone in between), so an applied
//! sweep lists and removes again until a pass removes nothing. The
//! classification is pure, over
//! an image list a provider reads ([`Image`]); [`podman`] reads and removes
//! Podman's, and [`apple`] `container`'s.

use std::collections::{BTreeMap, HashMap, HashSet};

use serde::Serialize;

/// On an image a node built or committed: the node's `[boundary]
/// resource_prefix`.
pub const PREFIX_LABEL: &str = "io.tracon.node.prefix";
/// On an image a node built or committed: the node's id.
pub const NODE_LABEL: &str = "io.tracon.node.id";

/// An image made this recently may be a build whose row is not written yet
/// (the tag lands before the store hears of it), so it is never removed.
pub const GRACE_MS: i64 = 60 * 60 * 1000;

static NODE_ID: std::sync::OnceLock<String> = std::sync::OnceLock::new();

/// This process's node id, once the node knows it; images built after carry
/// it.
pub fn set_node_id(id: &str) {
    let _ = NODE_ID.set(id.to_string());
}

pub fn node_id() -> Option<&'static str> {
    NODE_ID.get().map(String::as_str)
}

/// The labels every image this node builds or commits carries.
pub fn owner_labels() -> Vec<(String, String)> {
    let mut labels = vec![(
        PREFIX_LABEL.to_string(),
        crate::config::resource_prefix().to_string(),
    )];
    if let Some(id) = node_id() {
        labels.push((NODE_LABEL.to_string(), id.to_string()));
    }
    labels
}

/// `labels` with this node's owner labels added.
pub fn with_owner(labels: &[(String, String)]) -> Vec<(String, String)> {
    let mut out = labels.to_vec();
    out.extend(owner_labels());
    out
}

/// One image as a runtime's store holds it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Image {
    pub id: String,
    /// The image it was built or committed on, where the store knows.
    pub parent: Option<String>,
    /// `repository:tag`, every one the image has.
    pub names: Vec<String>,
    /// `sha256:…`, every digest the store knows the image by.
    pub digests: Vec<String>,
    pub labels: BTreeMap<String, String>,
    /// Bytes, its parent's layers included.
    pub size: u64,
    /// Containers (running or not) made from it.
    pub containers: u64,
    pub created_ms: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Class {
    Base,
    Repository,
    Snapshot,
    Dangling,
    Legacy,
    Unowned,
}

impl Class {
    pub fn as_str(self) -> &'static str {
        match self {
            Class::Base => "base",
            Class::Repository => "repository",
            Class::Snapshot => "snapshot",
            Class::Dangling => "dangling",
            Class::Legacy => "legacy",
            Class::Unowned => "unowned",
        }
    }
}

/// An image something of this node's still names, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Kept {
    /// `name@sha256:…`, `name:tag`, or an image id.
    pub reference: String,
    pub why: String,
    /// Named only by what runs without sandbox images (a session layer): with
    /// them on, it keeps nothing.
    pub legacy: bool,
}

/// What a sweep knows about this node.
#[derive(Debug, Clone, Default)]
pub struct Knowledge {
    pub prefix: String,
    pub node_id: String,
    /// The stock base's repository (`ghcr.io/cosmicspork/tracon-sandbox-base`),
    /// where this node runs on it.
    pub base_repository: Option<String>,
    /// The base this node pins, by digest; `None` until it has pinned one.
    pub pinned_base: Option<String>,
    /// Whether a superseded base is this node's to remove: pulled images carry
    /// no node's label, so only the store's default node takes them as its
    /// own.
    pub bases_are_ours: bool,
    pub kept: Vec<Kept>,
    /// Sessions run from the bundle on the base (Podman, Apple, Kubernetes
    /// over the protocol), so harness images and layers over repository
    /// images serve nothing.
    pub bundled: bool,
    /// The harness images' repositories, and the old gateway's.
    pub legacy_repositories: Vec<String>,
    pub now_ms: i64,
    /// Remove legacy images too.
    pub legacy: bool,
    /// Take unlabelled images named as this node's for its own.
    pub adopt_unlabelled: bool,
    /// The images of containers the sweep removes before it gets to images
    /// (a transfer's, left after its copy): each one counts a container
    /// fewer, so a dry run lists what the apply would free.
    pub departing: Vec<String>,
}

impl Knowledge {
    fn repo_stem(&self) -> String {
        format!("localhost/{}-repo-", self.prefix)
    }

    fn snapshot_stem(&self) -> String {
        format!("localhost/{}-snapshot", self.prefix)
    }
}

/// What the sweep makes of one image.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Verdict {
    /// Into the image list classified.
    pub index: usize,
    pub class: Class,
    pub remove: bool,
    pub reason: String,
    /// What removing it frees, about: its size less the nearest named image
    /// it stands on, whose layers it shares.
    pub reclaim: u64,
}

/// `name` less its tag (a `:` after the last `/`) or digest.
pub fn repository_of(name: &str) -> &str {
    let name = name.split_once('@').map_or(name, |(name, _)| name);
    match name.rsplit_once(':') {
        Some((repository, tag)) if !tag.contains('/') => repository,
        _ => name,
    }
}

/// How an image shows itself to be this node's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Owner {
    Ours,
    Unlabelled,
    Theirs,
}

fn owner(image: &Image, k: &Knowledge) -> Owner {
    match (image.labels.get(PREFIX_LABEL), image.labels.get(NODE_LABEL)) {
        (None, None) => Owner::Unlabelled,
        (Some(prefix), Some(node)) if *prefix == k.prefix && *node == k.node_id => Owner::Ours,
        // Built by this node before it knew its id: its, probably, but
        // not provably.
        (Some(prefix), None) if *prefix == k.prefix => Owner::Unlabelled,
        _ => Owner::Theirs,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Shape {
    Base,
    Harness,
    Snapshot,
    /// A repository image; `true` for a layer over one or the image under
    /// one (`-claude`, `-native`, `-src`).
    Repository(bool),
    Dangling,
}

/// The suffixes `repo_image` names the images around a repository's own.
const LAYERED: [&str; 3] = ["-src", "-claude", "-native"];

fn shape(image: &Image, k: &Knowledge, owner: Owner, leaf: bool) -> Option<Shape> {
    if image.names.is_empty() {
        return (owner == Owner::Ours && leaf).then_some(Shape::Dangling);
    }
    let repositories: Vec<&str> = image.names.iter().map(|n| repository_of(n)).collect();
    if let Some(base) = &k.base_repository {
        if repositories.iter().any(|r| r == base) {
            return Some(Shape::Base);
        }
    }
    if repositories
        .iter()
        .any(|r| k.legacy_repositories.iter().any(|l| l == r))
    {
        return Some(Shape::Harness);
    }
    let snapshot_stem = k.snapshot_stem();
    if repositories.iter().any(|r| r.starts_with(&snapshot_stem))
        || (owner == Owner::Ours && image.labels.contains_key(crate::sandbox_images::KEY_LABEL))
    {
        return Some(Shape::Snapshot);
    }
    let repo_stem = k.repo_stem();
    if let Some(repository) = repositories.iter().find(|r| r.starts_with(&repo_stem)) {
        return Some(Shape::Repository(
            LAYERED.iter().any(|suffix| repository.ends_with(suffix)),
        ));
    }
    None
}

/// Whether this node named `name` for an image of `shape`.
fn claims(name: &str, shape: Shape, k: &Knowledge) -> bool {
    let repository = repository_of(name);
    match shape {
        Shape::Base => Some(repository) == k.base_repository.as_deref(),
        Shape::Harness => k.legacy_repositories.iter().any(|l| l == repository),
        Shape::Snapshot => repository.starts_with(&k.snapshot_stem()),
        Shape::Repository(_) => repository.starts_with(&k.repo_stem()),
        Shape::Dangling => false,
    }
}

/// The image `reference` names in `images`: by id, `name:tag`, or digest.
pub fn resolve_in(images: &[Image], reference: &str) -> Option<usize> {
    if reference.is_empty() {
        return None;
    }
    let bare = reference.strip_prefix("sha256:").unwrap_or(reference);
    if let Some(at) = images
        .iter()
        .position(|i| i.id == bare || (bare.len() >= 12 && i.id.starts_with(bare)))
    {
        return Some(at);
    }
    if let Some((_, digest)) = reference.split_once('@') {
        return images
            .iter()
            .position(|i| i.digests.iter().any(|d| d == digest));
    }
    images
        .iter()
        .position(|i| i.names.iter().any(|n| n == reference))
}

/// Classify every image of this node's in `images`. Images that are not (or
/// another node's) get no verdict.
pub fn classify(images: &[Image], k: &Knowledge) -> Vec<Verdict> {
    let by_id: HashMap<&str, usize> = images
        .iter()
        .enumerate()
        .map(|(at, i)| (i.id.as_str(), at))
        .collect();
    let mut children: Vec<Vec<usize>> = vec![Vec::new(); images.len()];
    for (at, image) in images.iter().enumerate() {
        if let Some(parent) = image.parent.as_deref().and_then(|p| by_id.get(p)) {
            children[*parent].push(at);
        }
    }
    // What each image stands on: its parent, and the image a snapshot's
    // label names (a commit's parent is not always recorded).
    let parents: Vec<Vec<usize>> = images
        .iter()
        .map(|image| {
            let mut parents = Vec::new();
            if let Some(parent) = image.parent.as_deref().and_then(|p| by_id.get(p)) {
                parents.push(*parent);
            }
            if let Some(from) = image.labels.get(crate::sandbox_images::FROM_LABEL) {
                parents.extend(resolve_in(images, from));
            }
            parents
        })
        .collect();
    let mut kept: HashMap<usize, &str> = HashMap::new();
    for entry in &k.kept {
        if entry.legacy && k.bundled {
            continue;
        }
        if let Some(at) = resolve_in(images, &entry.reference) {
            kept.entry(at).or_insert(&entry.why);
        }
    }
    let pinned = k.pinned_base.as_deref().and_then(|p| resolve_in(images, p));
    let mut leaving: HashMap<usize, u64> = HashMap::new();
    for reference in &k.departing {
        if let Some(at) = resolve_in(images, reference) {
            *leaving.entry(at).or_default() += 1;
        }
    }

    let mut verdicts = Vec::new();
    for (at, image) in images.iter().enumerate() {
        let owner = owner(image, k);
        if owner == Owner::Theirs {
            continue;
        }
        let Some(shape) = shape(image, k, owner, children[at].is_empty()) else {
            continue;
        };
        let containers = image
            .containers
            .saturating_sub(leaving.get(&at).copied().unwrap_or(0));
        let (class, remove, reason) = judge(image, at, containers, shape, owner, k, &kept, pinned);
        verdicts.push(Verdict {
            index: at,
            class,
            remove,
            reason,
            reclaim: 0,
        });
    }

    // Nothing anything that stays stands on goes: every image not removed,
    // listed or not, keeps what it is built on, and an image kept that way
    // keeps what it stands on in turn. An unlisted intermediate (no name, no
    // container) goes with the last image built on it, as the store prunes
    // it then: it holds nothing once they go.
    let listed: HashSet<usize> = verdicts.iter().map(|v| v.index).collect();
    let intermediate = |at: usize| {
        let image = &images[at];
        !listed.contains(&at)
            && image.names.is_empty()
            && image.containers == 0
            && !children[at].is_empty()
    };
    loop {
        let mut removing: HashSet<usize> = verdicts
            .iter()
            .filter(|v| v.remove)
            .map(|v| v.index)
            .collect();
        loop {
            let pruned: Vec<usize> = (0..images.len())
                .filter(|at| !removing.contains(at) && intermediate(*at))
                .filter(|at| children[*at].iter().all(|c| removing.contains(c)))
                .collect();
            if pruned.is_empty() {
                break;
            }
            removing.extend(pruned);
        }
        let mut held = HashSet::new();
        for at in (0..images.len()).filter(|at| !removing.contains(at)) {
            let mut stack = parents[at].clone();
            while let Some(parent) = stack.pop() {
                if held.insert(parent) {
                    stack.extend(parents[parent].iter().copied());
                }
            }
        }
        let mut changed = false;
        for verdict in verdicts.iter_mut().filter(|v| v.remove) {
            if held.contains(&verdict.index) {
                verdict.remove = false;
                verdict.reason = "an image that stays is built on it".into();
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }

    for verdict in &mut verdicts {
        verdict.reclaim = reclaim(images, &by_id, verdict.index);
    }
    verdicts.sort_by(|a, b| {
        a.class
            .cmp(&b.class)
            .then_with(|| b.remove.cmp(&a.remove))
            .then_with(|| display_name(&images[a.index]).cmp(&display_name(&images[b.index])))
    });
    verdicts
}

#[allow(clippy::too_many_arguments)]
fn judge(
    image: &Image,
    at: usize,
    containers: u64,
    shape: Shape,
    owner: Owner,
    k: &Knowledge,
    kept: &HashMap<usize, &str>,
    pinned: Option<usize>,
) -> (Class, bool, String) {
    let class = match shape {
        Shape::Base => Class::Base,
        Shape::Harness => Class::Legacy,
        Shape::Snapshot => Class::Snapshot,
        Shape::Repository(true) if k.bundled => Class::Legacy,
        Shape::Repository(_) => Class::Repository,
        Shape::Dangling => Class::Dangling,
    };
    let keep = |class: Class, why: &str| (class, false, why.to_string());
    let labelled = matches!(
        shape,
        Shape::Snapshot | Shape::Repository(_) | Shape::Dangling
    );
    if labelled && owner == Owner::Unlabelled && !k.adopt_unlabelled {
        return keep(
            Class::Unowned,
            "carries no owner label (made before labels, or by another node); \
             --adopt-unlabelled takes it as this node's",
        );
    }
    if let Some(foreign) = image.names.iter().find(|n| !claims(n, shape, k)) {
        return keep(
            class,
            &format!("also named {foreign}, which this node did not name"),
        );
    }
    if containers > 0 {
        return keep(class, "a container uses it");
    }
    if let Some(why) = kept.get(&at) {
        return keep(class, why);
    }
    if Some(at) == pinned {
        return keep(class, "the stock base this node pins");
    }
    if k.now_ms - image.created_ms < GRACE_MS {
        return keep(
            class,
            "made in the last hour; a build may not have recorded it yet",
        );
    }
    let remove = |why: &str| (class, true, why.to_string());
    match class {
        Class::Base if pinned.is_none() => {
            keep(class, "this node has not pinned a stock base to compare")
        }
        Class::Base if !k.bases_are_ours && !k.adopt_unlabelled => keep(
            class,
            "a pulled base carries no owner label, and this node shares the store under \
             a prefix of its own; --adopt-unlabelled takes it as this node's",
        ),
        Class::Base => remove("a superseded stock base"),
        Class::Snapshot => remove("a superseded prepared snapshot; no repository starts from it"),
        Class::Repository => remove("no current build, snapshot or open session uses it"),
        Class::Dangling => remove("a later build of this node's took its tag"),
        Class::Legacy if !k.bundled => keep(
            class,
            "harness images run sessions on a pod-hosted node off the sandbox protocol",
        ),
        Class::Legacy if !k.legacy => keep(
            class,
            "unused now sessions run from the bundle; removed only with --legacy",
        ),
        Class::Legacy => remove("unused now sessions run from the bundle"),
        Class::Unowned => keep(class, "not this node's"),
    }
}

/// What removing `at` frees, about: its size less the nearest named image it
/// stands on.
fn reclaim(images: &[Image], by_id: &HashMap<&str, usize>, at: usize) -> u64 {
    let mut seen = HashSet::new();
    let mut next = images[at].parent.as_deref();
    while let Some(parent) = next.and_then(|p| by_id.get(p)) {
        if !seen.insert(*parent) {
            break;
        }
        let parent = &images[*parent];
        if !parent.names.is_empty() {
            return images[at].size.saturating_sub(parent.size);
        }
        next = parent.parent.as_deref();
    }
    images[at].size
}

/// How a list shows an image: its first name, or `<none>` and its short id.
pub fn display_name(image: &Image) -> String {
    match image.names.first() {
        Some(name) => name.clone(),
        None => format!("<none> {}", &image.id[..image.id.len().min(12)]),
    }
}

/// How deep `at` stands on other images: removal goes deepest first, so a
/// child goes before the image it is built on.
pub fn depth(images: &[Image], at: usize) -> usize {
    let by_id: HashMap<&str, usize> = images
        .iter()
        .enumerate()
        .map(|(at, i)| (i.id.as_str(), at))
        .collect();
    let mut seen = HashSet::new();
    let mut next = images[at].parent.as_deref();
    while let Some(parent) = next.and_then(|p| by_id.get(p)) {
        if !seen.insert(*parent) {
            break;
        }
        next = images[*parent].parent.as_deref();
    }
    seen.len()
}

/// One image a sweep listed, and what came of it.
#[derive(Debug, Clone)]
pub struct Swept {
    pub name: String,
    pub class: Class,
    pub remove: bool,
    pub reason: String,
    pub reclaim: u64,
    pub removed: bool,
    pub error: Option<String>,
}

/// Podman's image store.
pub mod podman {
    use super::*;

    /// `podman images -a --format json`, intermediate images included so
    /// what an image stands on can be followed.
    pub async fn list() -> Result<Vec<Image>, String> {
        let out = crate::boundary::podman::podman(&["images", "-a", "--format", "json"])
            .await
            .map_err(|error| format!("podman images: {error}"))?;
        parse(&out)
    }

    pub fn parse(out: &str) -> Result<Vec<Image>, String> {
        let rows: Vec<serde_json::Value> = match out.trim() {
            "" => Vec::new(),
            out => serde_json::from_str(out).map_err(|e| format!("podman images: {e}"))?,
        };
        let strings = |value: &serde_json::Value| -> Vec<String> {
            value
                .as_array()
                .map(|items| {
                    items
                        .iter()
                        .filter_map(|v| v.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default()
        };
        let mut images: Vec<Image> = Vec::new();
        for row in rows {
            let id = row["Id"].as_str().unwrap_or_default().to_string();
            if id.is_empty() {
                continue;
            }
            let mut digests: Vec<String> = strings(&row["RepoDigests"])
                .iter()
                .filter_map(|d| d.split_once('@').map(|(_, d)| d.to_string()))
                .collect();
            if let Some(digest) = row["Digest"].as_str().filter(|d| !d.is_empty()) {
                digests.push(digest.to_string());
            }
            digests.sort();
            digests.dedup();
            let labels = row["Labels"]
                .as_object()
                .map(|labels| {
                    labels
                        .iter()
                        .filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_string())))
                        .collect()
                })
                .unwrap_or_default();
            // `-a` can list an image once per name; one entry per id.
            let names = strings(&row["Names"]);
            if let Some(seen) = images.iter_mut().find(|i| i.id == id) {
                for name in names {
                    if !seen.names.contains(&name) {
                        seen.names.push(name);
                    }
                }
                continue;
            }
            images.push(Image {
                id,
                parent: row["ParentId"]
                    .as_str()
                    .filter(|p| !p.is_empty())
                    .map(str::to_string),
                names,
                digests,
                labels,
                size: row["Size"].as_u64().unwrap_or(0),
                containers: row["Containers"].as_u64().unwrap_or(0),
                created_ms: row["Created"].as_i64().unwrap_or(0) * 1000,
            });
        }
        Ok(images)
    }

    /// The image id `reference` names, asked of Podman for one a list does
    /// not resolve (a base pinned by a digest only Podman maps).
    async fn inspect_id(reference: &str) -> Option<String> {
        crate::boundary::podman::podman(&["image", "inspect", "--format", "{{.Id}}", reference])
            .await
            .ok()
            .map(|id| id.trim().to_string())
            .filter(|id| !id.is_empty())
    }

    /// Remove one image by what this node named it, never forced: Podman
    /// refuses an image a container uses or another is built on. Every name
    /// but the last is untagged by name, then the image goes by id, so a tag
    /// that moved since the list is never followed to another image.
    async fn remove(image: &Image) -> Result<(), String> {
        use crate::boundary::podman::podman;
        if let Some((_, rest)) = image.names.split_last() {
            for name in rest {
                podman(&["untag", &image.id, name])
                    .await
                    .map_err(|e| e.to_string())?;
            }
        }
        podman(&["image", "rm", &image.id])
            .await
            .map(|_| ())
            .map_err(|e| e.to_string())
    }

    /// List this node's images in Podman's store and what each is, and with
    /// `apply`, remove those that go, pass after pass until one removes
    /// nothing: what a removal frees is removed in the same sweep.
    pub async fn sweep(k: &Knowledge, apply: bool) -> Result<Vec<Swept>, String> {
        let mut swept = Vec::new();
        let mut failed = HashMap::new();
        for _ in 0..MAX_PASSES {
            let images = list().await?;
            let mut k = k.clone();
            for reference in k
                .kept
                .iter_mut()
                .map(|kept| &mut kept.reference)
                .chain(k.pinned_base.as_mut())
            {
                if resolve_in(&images, reference).is_none() {
                    if let Some(id) = inspect_id(reference).await {
                        *reference = id;
                    }
                }
            }
            let verdicts = pass(&images, &k, &failed);
            let mut order: Vec<&Verdict> = verdicts.iter().filter(|v| v.remove).collect();
            order.sort_by_key(|v| std::cmp::Reverse(depth(&images, v.index)));
            let mut outcome: HashMap<usize, Result<(), String>> = HashMap::new();
            if apply {
                for verdict in order {
                    outcome.insert(verdict.index, remove(&images[verdict.index]).await);
                }
            }
            let settled = !outcome.values().any(Result::is_ok);
            for v in verdicts {
                let done = outcome.remove(&v.index);
                // A pass that removed something lists only what it did; the
                // pass that removes nothing lists what stays.
                if done.is_none() && !settled {
                    continue;
                }
                if let Some(Err(error)) = &done {
                    failed.insert(images[v.index].id.clone(), error.clone());
                }
                swept.push(Swept {
                    name: display_name(&images[v.index]),
                    class: v.class,
                    remove: v.remove,
                    reason: v.reason,
                    reclaim: v.reclaim,
                    removed: matches!(done, Some(Ok(()))),
                    error: done.and_then(Result::err),
                });
            }
            if settled {
                break;
            }
        }
        Ok(swept)
    }
}

/// An applied sweep stops after this many passes even if each removes
/// something; every pass removes at least one image, so a store needs as
/// many only for a chain this deep.
const MAX_PASSES: usize = 16;

/// What one pass of an applied sweep makes of `images`: their verdicts, less
/// the images an earlier pass failed to remove (already listed with why),
/// which stay and keep what they are built on.
fn pass(images: &[Image], k: &Knowledge, failed: &HashMap<String, String>) -> Vec<Verdict> {
    let mut k = k.clone();
    for id in failed.keys() {
        k.kept.push(Kept {
            reference: id.clone(),
            why: "could not be removed".into(),
            legacy: false,
        });
    }
    classify(images, &k)
        .into_iter()
        .filter(|v| !failed.contains_key(&images[v.index].id))
        .collect()
}

/// Apple's `container` image store. It lists one entry per name, so an image
/// is every entry with its id (the index digest). It records no parent: a
/// snapshot or a build there is a flat image of its own, sharing blobs by
/// content rather than standing on another image, so what an image is built
/// on is only what its snapshot label names. A Mac's VMs run only arm64, so
/// an image's size, and what removing it frees, is its arm64 manifests'.
pub mod apple {
    use super::*;
    use crate::provider::apple::{CliError, ContainerCli};

    /// One image, and its arm64 manifests with their sizes: what removing
    /// it frees is the manifests no image that stays also holds.
    #[derive(Debug, Clone, Default)]
    pub struct Listed {
        pub image: Image,
        pub manifests: Vec<(String, u64)>,
    }

    async fn container(cli: &dyn ContainerCli, args: &[&str]) -> Result<String, CliError> {
        let args: Vec<String> = args.iter().map(|a| a.to_string()).collect();
        cli.run(&args).await
    }

    pub async fn list(cli: &dyn ContainerCli) -> Result<Vec<Listed>, String> {
        let images = container(cli, &["image", "list", "--format", "json"])
            .await
            .map_err(|error| format!("container image list: {error}"))?;
        let containers = container(cli, &["list", "--all", "--format", "json"])
            .await
            .map_err(|error| format!("container list: {error}"))?;
        parse(&images, &containers)
    }

    /// `container image list --format json` and `container list --all
    /// --format json`.
    pub fn parse(images: &str, containers: &str) -> Result<Vec<Listed>, String> {
        let rows = |text: &str, what: &str| -> Result<Vec<serde_json::Value>, String> {
            match text.trim() {
                "" => Ok(Vec::new()),
                text => serde_json::from_str(text).map_err(|e| format!("{what}: {e}")),
            }
        };
        let mut listed: Vec<Listed> = Vec::new();
        for row in rows(images, "container image list")? {
            let configuration = &row["configuration"];
            let digest = configuration["descriptor"]["digest"]
                .as_str()
                .unwrap_or_default();
            let id = row["id"]
                .as_str()
                .filter(|id| !id.is_empty())
                .unwrap_or_else(|| digest.strip_prefix("sha256:").unwrap_or(digest))
                .to_string();
            let Some(name) = configuration["name"].as_str().filter(|n| !n.is_empty()) else {
                continue;
            };
            if id.is_empty() {
                continue;
            }
            if let Some(seen) = listed.iter_mut().find(|l| l.image.id == id) {
                if !seen.image.names.iter().any(|n| n == name) {
                    seen.image.names.push(name.to_string());
                }
                continue;
            }
            let variants: Vec<&serde_json::Value> = row["variants"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|v| {
                    v["platform"]["os"] == "linux"
                        && v["platform"]["architecture"] == crate::boundary::apple::VM_ARCH
                })
                .collect();
            let manifests: Vec<(String, u64)> = variants
                .iter()
                .filter_map(|v| Some((v["digest"].as_str()?.to_string(), v["size"].as_u64()?)))
                .collect();
            let mut digests = vec![format!("sha256:{id}")];
            digests.extend(manifests.iter().map(|(digest, _)| digest.clone()));
            digests.sort();
            digests.dedup();
            let labels = variants
                .iter()
                .find_map(|v| v["config"]["config"]["Labels"].as_object())
                .map(|labels| {
                    labels
                        .iter()
                        .filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_string())))
                        .collect()
                })
                .unwrap_or_default();
            let created_ms = configuration["creationDate"]
                .as_str()
                .or_else(|| {
                    variants
                        .iter()
                        .find_map(|v| v["config"]["created"].as_str())
                })
                .and_then(crate::session::exhaustion::rfc3339_ms)
                .unwrap_or(0);
            listed.push(Listed {
                image: Image {
                    id,
                    parent: None,
                    names: vec![name.to_string()],
                    digests,
                    labels,
                    size: manifests.iter().map(|(_, size)| size).sum(),
                    containers: 0,
                    created_ms,
                },
                manifests,
            });
        }
        for row in rows(containers, "container list")? {
            let image = &row["configuration"]["image"];
            let digest = image["descriptor"]["digest"].as_str().unwrap_or_default();
            let reference = image["reference"].as_str().unwrap_or_default();
            let used = listed.iter_mut().find(|l| {
                (!digest.is_empty() && l.image.digests.iter().any(|d| d == digest))
                    || l.image.names.iter().any(|n| n == reference)
            });
            if let Some(used) = used {
                used.image.containers += 1;
            }
        }
        // A tag first, so a list shows the name a person gave it.
        for l in &mut listed {
            l.image.names.sort_by_key(|n| n.contains('@'));
        }
        Ok(listed)
    }

    /// The digest `name` names now, as `sha256:…`.
    async fn digest_of(cli: &dyn ContainerCli, name: &str) -> Option<String> {
        let text = container(cli, &["image", "inspect", name]).await.ok()?;
        let inspected: Vec<serde_json::Value> = serde_json::from_str(&text).ok()?;
        inspected
            .first()?
            .pointer("/configuration/descriptor/digest")?
            .as_str()
            .map(str::to_string)
    }

    /// Remove one image by every name it has. `container` deletes by name
    /// only, so a tag is checked to still name this image first: a tag that
    /// moved since the list is left to the image it names now.
    async fn remove(cli: &dyn ContainerCli, image: &Image) -> Result<(), String> {
        let id = format!("sha256:{}", image.id);
        for name in &image.names {
            if !name.contains('@') {
                match digest_of(cli, name).await {
                    Some(digest) if digest == id => {}
                    Some(_) => return Err(format!("{name} now names another image; left it")),
                    None => continue,
                }
            }
            container(cli, &["image", "delete", name])
                .await
                .map_err(|e| format!("delete {name}: {e}"))?;
        }
        Ok(())
    }

    /// List this node's images in `container`'s store and what each is, and
    /// with `apply`, remove those that go.
    pub async fn sweep(
        cli: &dyn ContainerCli,
        k: &Knowledge,
        apply: bool,
    ) -> Result<Vec<Swept>, String> {
        let listed = list(cli).await?;
        let images: Vec<Image> = listed.iter().map(|l| l.image.clone()).collect();
        let mut k = k.clone();
        // The base is pinned by its arm64 manifest, which a base of another
        // tag can hold too (the same build released twice): every image
        // holding it is the pinned base.
        if let Some(pinned) = k.pinned_base.as_deref() {
            let digest = pinned
                .split_once('@')
                .map_or(pinned, |(_, d)| d)
                .to_string();
            for image in images.iter().filter(|i| i.digests.contains(&digest)) {
                k.kept.push(Kept {
                    reference: image.id.clone(),
                    why: "holds the stock base this node pins".into(),
                    legacy: false,
                });
            }
        }
        let mut verdicts = classify(&images, &k);
        // A manifest an image that stays also holds frees nothing.
        let removing: HashSet<usize> = verdicts
            .iter()
            .filter(|v| v.remove)
            .map(|v| v.index)
            .collect();
        let mut staying: HashMap<&str, Vec<usize>> = HashMap::new();
        for (at, l) in listed.iter().enumerate() {
            if !removing.contains(&at) {
                for (digest, _) in &l.manifests {
                    staying.entry(digest.as_str()).or_default().push(at);
                }
            }
        }
        for verdict in &mut verdicts {
            let at = verdict.index;
            verdict.reclaim = listed[at]
                .manifests
                .iter()
                .filter(|(digest, _)| {
                    !staying
                        .get(digest.as_str())
                        .is_some_and(|holders| holders.iter().any(|h| *h != at))
                })
                .map(|(_, size)| size)
                .sum();
        }
        let mut outcome: HashMap<usize, Result<(), String>> = HashMap::new();
        if apply {
            // A snapshot before the build its label names.
            let mut order: Vec<&Verdict> = verdicts.iter().filter(|v| v.remove).collect();
            order.sort_by_key(|v| {
                !images[v.index]
                    .labels
                    .contains_key(crate::sandbox_images::FROM_LABEL)
            });
            for verdict in order {
                outcome.insert(verdict.index, remove(cli, &images[verdict.index]).await);
            }
        }
        Ok(verdicts
            .into_iter()
            .map(|v| {
                let done = outcome.remove(&v.index);
                Swept {
                    name: display_name(&images[v.index]),
                    class: v.class,
                    remove: v.remove,
                    reason: v.reason,
                    reclaim: v.reclaim,
                    removed: matches!(done, Some(Ok(()))),
                    error: done.and_then(Result::err),
                }
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 100 * GRACE_MS;
    const OLD: i64 = 0;

    fn knowledge() -> Knowledge {
        Knowledge {
            prefix: "tracon".into(),
            node_id: "node-a".into(),
            base_repository: Some("ghcr.io/cosmicspork/tracon-sandbox-base".into()),
            pinned_base: Some("ghcr.io/cosmicspork/tracon-sandbox-base@sha256:b2".into()),
            bases_are_ours: true,
            kept: Vec::new(),
            bundled: true,
            legacy_repositories: vec![
                "localhost/tracon-harness-claude".into(),
                "localhost/tracon-harness-native".into(),
            ],
            now_ms: NOW,
            legacy: false,
            adopt_unlabelled: false,
            departing: Vec::new(),
        }
    }

    fn ours() -> BTreeMap<String, String> {
        [(PREFIX_LABEL, "tracon"), (NODE_LABEL, "node-a")]
            .into_iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    fn image(
        id: &str,
        parent: Option<&str>,
        names: &[&str],
        labels: BTreeMap<String, String>,
    ) -> Image {
        Image {
            id: id.into(),
            parent: parent.map(str::to_string),
            names: names.iter().map(|n| n.to_string()).collect(),
            digests: vec![format!("sha256:{id}")],
            labels,
            size: 100,
            containers: 0,
            created_ms: OLD,
        }
    }

    fn snapshot_labels(from: &str) -> BTreeMap<String, String> {
        let mut labels = ours();
        labels.insert(crate::sandbox_images::KEY_LABEL.into(), "k".into());
        labels.insert(crate::sandbox_images::FROM_LABEL.into(), from.into());
        labels
    }

    fn verdict<'a>(verdicts: &'a [Verdict], images: &[Image], id: &str) -> &'a Verdict {
        let at = images.iter().position(|i| i.id == id).unwrap();
        verdicts
            .iter()
            .find(|v| v.index == at)
            .unwrap_or_else(|| panic!("no verdict for {id}"))
    }

    fn unlisted(verdicts: &[Verdict], images: &[Image], id: &str) -> bool {
        let at = images.iter().position(|i| i.id == id).unwrap();
        !verdicts.iter().any(|v| v.index == at)
    }

    const BASE: &str = "ghcr.io/cosmicspork/tracon-sandbox-base";
    const REPO: &str = "localhost/tracon-repo-app-0123456789";
    const SNAP: &str = "localhost/tracon-snapshot-app-0123456789";

    /// The store as a node that moved from base b1 to b2 leaves it.
    fn store() -> Vec<Image> {
        vec![
            image("b0", None, &[&format!("{BASE}:0.44.0")], BTreeMap::new()),
            image("b1", None, &[&format!("{BASE}:0.45.0")], BTreeMap::new()),
            image("b2", None, &[&format!("{BASE}:0.46.0")], BTreeMap::new()),
            // The current build, on the old base b0, and a superseded one.
            image("r-cur", Some("b0"), &[&format!("{REPO}:cur")], ours()),
            image("r-old", Some("b2"), &[&format!("{REPO}:old")], ours()),
            image(
                "s-cur",
                Some("r-cur"),
                &[&format!("{SNAP}:cur")],
                snapshot_labels(&format!("{REPO}@sha256:r-cur")),
            ),
            image(
                "s-old",
                Some("r-cur"),
                &[&format!("{SNAP}:old")],
                snapshot_labels(&format!("{REPO}@sha256:r-cur")),
            ),
            image("dangling", Some("b2"), &[], ours()),
            image("theirs", Some("b2"), &[&format!("{REPO}:theirs")], {
                let mut l = ours();
                l.insert(NODE_LABEL.into(), "node-b".into());
                l
            }),
            image(
                "unlabelled",
                Some("b2"),
                &[&format!("{REPO}:unlabelled")],
                BTreeMap::new(),
            ),
            image(
                "other",
                None,
                &["docker.io/library/alpine:3"],
                BTreeMap::new(),
            ),
        ]
    }

    fn current() -> Vec<Kept> {
        vec![
            Kept {
                reference: format!("{REPO}@sha256:r-cur"),
                why: "the current build of app".into(),
                legacy: false,
            },
            Kept {
                reference: format!("{SNAP}@sha256:s-cur"),
                why: "app's current snapshot".into(),
                legacy: false,
            },
        ]
    }

    #[test]
    fn each_class_goes_only_when_nothing_of_this_node_uses_it() {
        let images = store();
        let mut k = knowledge();
        k.kept = current();
        let v = classify(&images, &k);
        let removes = |id| verdict(&v, &images, id).remove;
        let class = |id| verdict(&v, &images, id).class;

        assert_eq!(class("b1"), Class::Base);
        assert!(removes("b1"), "a superseded base nothing stands on");
        assert!(!removes("b2"), "the pinned base");
        assert!(!removes("b0"), "the current build stands on it");
        assert!(verdict(&v, &images, "b0").reason.contains("built on it"));

        assert_eq!(class("r-old"), Class::Repository);
        assert!(removes("r-old"));
        assert!(!removes("r-cur"));

        assert_eq!(class("s-old"), Class::Snapshot);
        assert!(removes("s-old"));
        assert!(!removes("s-cur"));

        assert_eq!(class("dangling"), Class::Dangling);
        assert!(removes("dangling"));

        assert!(unlisted(&v, &images, "theirs"), "another node's label");
        assert!(unlisted(&v, &images, "other"), "not a name this node makes");
    }

    #[test]
    fn an_unlabelled_image_is_unowned_until_adopted() {
        let images = store();
        let mut k = knowledge();
        k.kept = current();
        let v = classify(&images, &k);
        let unlabelled = verdict(&v, &images, "unlabelled");
        assert_eq!(unlabelled.class, Class::Unowned);
        assert!(!unlabelled.remove);
        k.adopt_unlabelled = true;
        let v = classify(&images, &k);
        let adopted = verdict(&v, &images, "unlabelled");
        assert_eq!(adopted.class, Class::Repository);
        assert!(adopted.remove);
        // A dangling image with no label is nobody's that can be told.
        let mut images = store();
        images.push(image("bare", Some("b2"), &[], BTreeMap::new()));
        assert!(unlisted(&classify(&images, &k), &images, "bare"));
    }

    #[test]
    fn nothing_in_use_or_new_or_named_by_another_goes() {
        let mut images = store();
        let mut k = knowledge();
        k.kept = current();
        images
            .iter_mut()
            .find(|i| i.id == "r-old")
            .unwrap()
            .containers = 1;
        images
            .iter_mut()
            .find(|i| i.id == "s-old")
            .unwrap()
            .created_ms = NOW - 1000;
        images
            .iter_mut()
            .find(|i| i.id == "dangling")
            .unwrap()
            .names
            .push("localhost/mine:1".into());
        let v = classify(&images, &k);
        let r = verdict(&v, &images, "r-old");
        assert!(!r.remove && r.reason.contains("container"), "{r:?}");
        let s = verdict(&v, &images, "s-old");
        assert!(!s.remove && s.reason.contains("last hour"), "{s:?}");
        // Named by someone else now, so not a dangling one, and not one of
        // this node's names.
        assert!(unlisted(&v, &images, "dangling"));
        // A container on the superseded build keeps the base it stands on.
        assert!(!verdict(&v, &images, "b2").remove);
    }

    #[test]
    fn an_image_that_stays_keeps_what_it_is_built_on() {
        // A snapshot kept by its label alone (no recorded parent) keeps the
        // build it was made from, and that build its base.
        let mut images = store();
        images.iter_mut().find(|i| i.id == "s-cur").unwrap().parent = None;
        images.iter_mut().find(|i| i.id == "s-old").unwrap().parent = None;
        let mut k = knowledge();
        k.kept = vec![current().remove(1)];
        let v = classify(&images, &k);
        assert!(!verdict(&v, &images, "r-cur").remove);
        assert!(!verdict(&v, &images, "b0").remove);
        // An unlisted image built on one of this node's keeps it too.
        let mut images = store();
        images.push(image(
            "child",
            Some("r-old"),
            &["localhost/someone:1"],
            BTreeMap::new(),
        ));
        let v = classify(&images, &k);
        assert!(!verdict(&v, &images, "r-old").remove);
    }

    /// A layered image built on a repository image stands on the build's
    /// unnamed layers, not on the repository image itself. They go with it
    /// (the store prunes them), so one list shows the whole chain a removal
    /// frees rather than the next layer down only once the first has gone.
    #[test]
    fn a_chain_through_unnamed_layers_goes_in_one_list() {
        let mut images = store();
        images.push(image("src", None, &[&format!("{REPO}-src:k")], ours()));
        images.push(image(
            "dockerfile",
            Some("src"),
            &[&format!("{REPO}:k")],
            ours(),
        ));
        images.push(image("step-1", Some("dockerfile"), &[], ours()));
        images.push(image("step-2", Some("step-1"), &[], ours()));
        images.push(image(
            "layer",
            Some("step-2"),
            &[&format!("{REPO}-claude:k")],
            ours(),
        ));
        let mut k = knowledge();
        k.kept = current();
        k.legacy = true;
        let v = classify(&images, &k);
        for id in ["layer", "dockerfile", "src"] {
            assert!(verdict(&v, &images, id).remove, "{id}");
        }
        assert!(unlisted(&v, &images, "step-1"));
        // An unnamed layer something that stays is built on still holds.
        k.legacy = false;
        let v = classify(&images, &k);
        let dockerfile = verdict(&v, &images, "dockerfile");
        assert!(!dockerfile.remove && dockerfile.reason.contains("built on it"));
        // As does one a container uses.
        k.legacy = true;
        images
            .iter_mut()
            .find(|i| i.id == "step-1")
            .unwrap()
            .containers = 1;
        assert!(!verdict(&classify(&images, &k), &images, "dockerfile").remove);
    }

    /// A container the sweep removes first (a leftover transfer's) no longer
    /// counts, so the dry run lists the image it held as going.
    #[test]
    fn a_container_the_sweep_removes_first_holds_nothing() {
        let mut images = store();
        images.push(image(
            "h",
            None,
            &["localhost/tracon-harness-claude:latest"],
            BTreeMap::new(),
        ));
        images.last_mut().unwrap().containers = 1;
        let mut k = knowledge();
        k.kept = current();
        k.legacy = true;
        let h = verdict(&classify(&images, &k), &images, "h").clone();
        assert!(!h.remove && h.reason.contains("container"), "{h:?}");
        k.departing = vec!["h".into()];
        assert!(verdict(&classify(&images, &k), &images, "h").remove);
        // Only the containers that leave: another still holds it.
        images.last_mut().unwrap().containers = 2;
        assert!(!verdict(&classify(&images, &k), &images, "h").remove);
    }

    /// Each pass of an applied sweep lists anew; an image an earlier pass
    /// could not remove is not offered again, and keeps what it stands on.
    #[test]
    fn a_pass_leaves_out_what_an_earlier_one_could_not_remove() {
        let mut images = store();
        images.push(image(
            "b9",
            None,
            &[&format!("{BASE}:0.40.0")],
            BTreeMap::new(),
        ));
        images.push(image("r-9", Some("b9"), &[&format!("{REPO}:9")], ours()));
        let mut k = knowledge();
        k.kept = current();
        let first = pass(&images, &k, &HashMap::new());
        assert!(verdict(&first, &images, "r-9").remove);
        assert!(verdict(&first, &images, "b9").remove);
        let failed = HashMap::from([("r-9".to_string(), "in use".to_string())]);
        let next = pass(&images, &k, &failed);
        assert!(unlisted(&next, &images, "r-9"));
        let b9 = verdict(&next, &images, "b9");
        assert!(!b9.remove && b9.reason.contains("built on it"), "{b9:?}");
        assert!(verdict(&next, &images, "r-old").remove);
    }

    #[test]
    fn legacy_images_go_only_with_sandbox_images_and_when_asked() {
        let mut images = store();
        images.push(image(
            "h",
            None,
            &["localhost/tracon-harness-claude:latest"],
            BTreeMap::new(),
        ));
        images.push(image(
            "layer",
            Some("r-cur"),
            &[&format!("{REPO}-claude:k")],
            ours(),
        ));
        images.push(image("src", None, &[&format!("{REPO}-src:k")], ours()));
        let mut k = knowledge();
        k.kept = current();
        k.kept.push(Kept {
            reference: format!("{REPO}-claude@sha256:layer"),
            why: "the current claude layer".into(),
            legacy: true,
        });
        // Off: the harness image and the layer run sessions.
        k.bundled = false;
        let v = classify(&images, &k);
        assert!(!verdict(&v, &images, "h").remove);
        assert!(!verdict(&v, &images, "layer").remove);
        let src = verdict(&v, &images, "src");
        assert_eq!(src.class, Class::Repository, "a superseded -src");
        assert!(src.remove);
        // On, without --legacy: listed, kept.
        k.bundled = true;
        let v = classify(&images, &k);
        for id in ["h", "layer", "src"] {
            let it = verdict(&v, &images, id);
            assert_eq!(it.class, Class::Legacy, "{id}");
            assert!(!it.remove && it.reason.contains("--legacy"), "{it:?}");
        }
        // On, with it.
        k.legacy = true;
        let v = classify(&images, &k);
        for id in ["h", "layer", "src"] {
            assert!(verdict(&v, &images, id).remove, "{id}");
        }
        // The gateway is never one unless the caller names it.
        assert!(!k.legacy_repositories.iter().any(|r| r.contains("gateway")));
    }

    #[test]
    fn a_node_with_its_own_prefix_leaves_bases_to_the_store_s_node() {
        let images = store();
        let mut k = knowledge();
        k.kept = current();
        k.bases_are_ours = false;
        let v = classify(&images, &k);
        let b1 = verdict(&v, &images, "b1");
        assert!(
            !b1.remove && b1.reason.contains("--adopt-unlabelled"),
            "{b1:?}"
        );
        k.adopt_unlabelled = true;
        assert!(verdict(&classify(&images, &k), &images, "b1").remove);
        // Unpinned, nothing is superseded.
        k.pinned_base = None;
        assert!(!verdict(&classify(&images, &k), &images, "b1").remove);
    }

    #[test]
    fn a_node_s_prefix_and_id_both_have_to_match() {
        let images = store();
        let mut k = knowledge();
        k.kept = current();
        k.node_id = "node-b".into();
        let v = classify(&images, &k);
        assert!(unlisted(&v, &images, "r-old"));
        assert!(!unlisted(&v, &images, "theirs"));
        let mut k = knowledge();
        k.prefix = "spike".into();
        let v = classify(&images, &k);
        // Its own names are `spike-repo-…`, and the labels say `tracon`.
        assert!(unlisted(&v, &images, "r-old"));
        assert!(unlisted(&v, &images, "unlabelled"));
    }

    #[test]
    fn reclaim_counts_what_an_image_adds_to_what_it_stands_on() {
        let mut images = store();
        images.iter_mut().find(|i| i.id == "r-old").unwrap().size = 350;
        let v = classify(&images, &knowledge());
        assert_eq!(verdict(&v, &images, "r-old").reclaim, 250);
        assert_eq!(verdict(&v, &images, "b1").reclaim, 100);
    }

    #[test]
    fn names_and_references_resolve() {
        assert_eq!(repository_of("localhost/a:1"), "localhost/a");
        assert_eq!(repository_of("localhost:5000/a"), "localhost:5000/a");
        assert_eq!(repository_of("localhost:5000/a:t"), "localhost:5000/a");
        assert_eq!(repository_of("ghcr.io/a/b@sha256:00"), "ghcr.io/a/b");
        let images = store();
        assert_eq!(resolve_in(&images, &format!("{REPO}:old")), Some(4));
        assert_eq!(
            resolve_in(&images, &format!("{REPO}@sha256:r-old")),
            Some(4)
        );
        assert_eq!(resolve_in(&images, "r-old"), Some(4));
        assert_eq!(resolve_in(&images, "nothing:1"), None);
    }

    #[test]
    fn podman_s_list_is_read_one_entry_per_image() {
        let out = r#"[
          {"Id":"aa","ParentId":"","Names":["localhost/a:1"],"RepoDigests":["localhost/a@sha256:d1"],
           "Digest":"sha256:d1","Labels":{"io.tracon.node.prefix":"tracon"},"Size":10,"Containers":2,"Created":5},
          {"Id":"aa","ParentId":"","Names":["localhost/a:2"],"Labels":null,"Size":10,"Containers":2,"Created":5},
          {"Id":"bb","ParentId":"aa","Names":null,"RepoDigests":null,"Digest":"","Labels":null,"Size":20,"Containers":0,"Created":6}
        ]"#;
        let images = podman::parse(out).unwrap();
        assert_eq!(images.len(), 2);
        assert_eq!(images[0].names, ["localhost/a:1", "localhost/a:2"]);
        assert_eq!(images[0].digests, ["sha256:d1"]);
        assert_eq!(images[0].containers, 2);
        assert_eq!(images[0].created_ms, 5000);
        assert_eq!(images[1].parent.as_deref(), Some("aa"));
        assert!(images[1].names.is_empty());
        assert!(podman::parse("").unwrap().is_empty());
    }

    mod apple_store {
        use std::sync::Mutex;

        use async_trait::async_trait;

        use super::*;
        use crate::provider::apple::{CliError, ContainerCli};

        /// One entry of `container image list`: a name, the image it names,
        /// that image's arm64 manifest and its labels.
        #[derive(Clone)]
        struct Entry {
            name: String,
            id: String,
            manifest: String,
            labels: BTreeMap<String, String>,
        }

        #[derive(Default)]
        struct Store {
            entries: Mutex<Vec<Entry>>,
            /// The image references containers were made from.
            containers: Vec<String>,
            deleted: Mutex<Vec<String>>,
        }

        const CREATED: &str = "2026-01-01T00:00:00Z";

        fn row(e: &Entry) -> serde_json::Value {
            let variant = |arch: &str, digest: String, size: u64| {
                serde_json::json!({
                    "digest": digest, "size": size,
                    "platform": {"os": "linux", "architecture": arch},
                    "config": {"config": {"Labels": e.labels}},
                })
            };
            serde_json::json!({
                "id": e.id,
                "configuration": {
                    "name": e.name,
                    "creationDate": CREATED,
                    "descriptor": {"digest": format!("sha256:{}", e.id)},
                },
                "variants": [
                    variant("arm64", e.manifest.clone(), 100),
                    variant("amd64", format!("{}-amd64", e.manifest), 7),
                ],
            })
        }

        #[async_trait]
        impl ContainerCli for Store {
            async fn run(&self, args: &[String]) -> Result<String, CliError> {
                let args: Vec<&str> = args.iter().map(String::as_str).collect();
                let entries = self.entries.lock().unwrap().clone();
                match args.as_slice() {
                    ["image", "list", "--format", "json"] => {
                        Ok(serde_json::Value::Array(entries.iter().map(row).collect()).to_string())
                    }
                    ["list", "--all", "--format", "json"] => Ok(serde_json::Value::Array(
                        self.containers
                            .iter()
                            .map(|reference| {
                                serde_json::json!({"configuration": {"image": {
                                    "reference": reference,
                                    "descriptor": {"digest": "sha256:unrelated"},
                                }}})
                            })
                            .collect(),
                    )
                    .to_string()),
                    ["image", "inspect", name] => match entries.iter().find(|e| e.name == *name) {
                        Some(e) => Ok(serde_json::json!([row(e)]).to_string()),
                        None => Err(CliError::Failed(format!("image not found: {name}"))),
                    },
                    ["image", "delete", name] => {
                        self.entries.lock().unwrap().retain(|e| e.name != *name);
                        self.deleted.lock().unwrap().push(name.to_string());
                        Ok(String::new())
                    }
                    other => panic!("unexpected container {other:?}"),
                }
            }
        }

        /// An image under its tag and, as `container` keeps a pinned one,
        /// its digest name.
        fn pinned(
            tag: &str,
            id: &str,
            manifest: &str,
            labels: &BTreeMap<String, String>,
        ) -> [Entry; 2] {
            let entry = |name: String| Entry {
                name,
                id: id.into(),
                manifest: format!("sha256:{manifest}"),
                labels: labels.clone(),
            };
            [
                entry(tag.to_string()),
                entry(format!("{}@sha256:{id}", repository_of(tag))),
            ]
        }

        fn store() -> Store {
            let none = BTreeMap::new();
            let snapshot = snapshot_labels(&format!("{REPO}@sha256:rcur"));
            let theirs = {
                let mut l = ours();
                l.insert(NODE_LABEL.into(), "node-b".into());
                l
            };
            let mut entries = Vec::new();
            // 0.45.0 and 0.46.0 are one build: the same arm64 manifest.
            entries.extend(pinned(&format!("{BASE}:0.45.0"), "i45", "m46", &none));
            entries.extend(pinned(&format!("{BASE}:0.46.0"), "i46", "m46", &none));
            entries.extend(pinned(&format!("{BASE}:0.44.0"), "i44", "m44", &none));
            entries.extend(pinned(&format!("{REPO}:cur"), "rcur", "mrcur", &ours()));
            entries.extend(pinned(&format!("{REPO}:old"), "rold", "mrold", &ours()));
            entries.extend(pinned(&format!("{SNAP}:cur"), "scur", "mscur", &snapshot));
            entries.extend(pinned(&format!("{SNAP}:old"), "sold", "msold", &snapshot));
            entries.extend(pinned(&format!("{REPO}:used"), "rused", "mrused", &ours()));
            entries.extend(pinned(
                &format!("{REPO}:unlabelled"),
                "runl",
                "mrunl",
                &none,
            ));
            entries.extend(pinned(
                &format!("{REPO}:theirs"),
                "rtheirs",
                "mrtheirs",
                &theirs,
            ));
            entries.extend(pinned(
                "docker.io/library/ubuntu:24.04",
                "ubuntu",
                "mu",
                &none,
            ));
            Store {
                entries: Mutex::new(entries),
                containers: vec![format!("{REPO}@sha256:rused")],
                deleted: Mutex::new(Vec::new()),
            }
        }

        fn knowledge() -> Knowledge {
            let mut k = super::knowledge();
            k.now_ms = crate::session::exhaustion::rfc3339_ms(CREATED).unwrap() + 2 * GRACE_MS;
            // Pinned by its arm64 manifest, as `converge_images` pins it.
            k.pinned_base = Some(format!("{BASE}@sha256:m46"));
            k.kept = vec![
                Kept {
                    reference: format!("{REPO}@sha256:rcur"),
                    why: "the current build of app".into(),
                    legacy: false,
                },
                Kept {
                    reference: format!("{SNAP}@sha256:scur"),
                    why: "app's current snapshot".into(),
                    legacy: false,
                },
            ];
            k
        }

        fn sorted(mut names: Vec<String>) -> Vec<String> {
            names.sort();
            names
        }

        #[test]
        fn an_image_is_every_name_it_has_and_only_its_arm64_manifests() {
            let store = store();
            let images: Vec<_> = store.entries.lock().unwrap().iter().map(row).collect();
            let containers = serde_json::json!([{"configuration": {"image": {
                "reference": "elsewhere", "descriptor": {"digest": "sha256:rused"}}}}]);
            let listed = apple::parse(
                &serde_json::Value::Array(images).to_string(),
                &containers.to_string(),
            )
            .unwrap();
            assert_eq!(listed.len(), 11);
            let base = &listed.iter().find(|l| l.image.id == "i46").unwrap().image;
            assert_eq!(
                base.names,
                [format!("{BASE}:0.46.0"), format!("{BASE}@sha256:i46")],
                "the tag first"
            );
            assert_eq!(base.size, 100, "amd64 is not a Mac's");
            assert!(base.digests.contains(&"sha256:m46".to_string()));
            assert!(base.digests.contains(&"sha256:i46".to_string()));
            assert_eq!(
                Some(base.created_ms),
                crate::session::exhaustion::rfc3339_ms(CREATED)
            );
            let repo = &listed.iter().find(|l| l.image.id == "rcur").unwrap().image;
            assert_eq!(
                repo.labels.get(NODE_LABEL).map(String::as_str),
                Some("node-a")
            );
            let used = &listed.iter().find(|l| l.image.id == "rused").unwrap().image;
            assert_eq!(used.containers, 1, "by the digest it was made from");
            assert!(apple::parse("", "").unwrap().is_empty());
        }

        #[tokio::test]
        async fn a_sweep_removes_what_goes_by_every_name_and_nothing_else() {
            let store = store();
            let swept = apple::sweep(&store, &knowledge(), false).await.unwrap();
            let get = |name: String| {
                swept
                    .iter()
                    .find(|s| s.name == name)
                    .unwrap_or_else(|| panic!("{name} not listed: {swept:#?}"))
                    .clone()
            };
            let going = |swept: &[Swept]| {
                sorted(
                    swept
                        .iter()
                        .filter(|s| s.remove)
                        .map(|s| s.name.clone())
                        .collect(),
                )
            };
            let goes = sorted(vec![
                format!("{BASE}:0.44.0"),
                format!("{REPO}:old"),
                format!("{SNAP}:old"),
            ]);
            assert_eq!(going(&swept), goes, "{swept:#?}");
            // Both tags of the pinned build stay.
            assert!(get(format!("{BASE}:0.45.0")).reason.contains("pins"));
            assert!(get(format!("{BASE}:0.46.0")).reason.contains("pins"));
            assert!(get(format!("{REPO}:used")).reason.contains("container"));
            assert_eq!(get(format!("{REPO}:unlabelled")).class, Class::Unowned);
            assert!(!swept
                .iter()
                .any(|s| s.name.contains("theirs") || s.name.contains("ubuntu")));
            assert_eq!(get(format!("{REPO}:old")).reclaim, 100);
            assert!(store.deleted.lock().unwrap().is_empty(), "a dry run");

            let swept = apple::sweep(&store, &knowledge(), true).await.unwrap();
            assert!(
                swept
                    .iter()
                    .filter(|s| s.remove)
                    .all(|s| s.removed && s.error.is_none()),
                "{swept:#?}"
            );
            let every_name = sorted(
                [
                    (format!("{BASE}:0.44.0"), "i44"),
                    (format!("{REPO}:old"), "rold"),
                    (format!("{SNAP}:old"), "sold"),
                ]
                .into_iter()
                .flat_map(|(tag, id)| {
                    let digest = format!("{}@sha256:{id}", repository_of(&tag));
                    [tag, digest]
                })
                .collect(),
            );
            assert_eq!(sorted(store.deleted.lock().unwrap().clone()), every_name);
            let left = apple::sweep(&store, &knowledge(), false).await.unwrap();
            assert!(going(&left).is_empty(), "{left:#?}");
        }

        #[tokio::test]
        async fn a_tag_that_moved_since_the_list_is_left_to_its_new_image() {
            /// Moves the old build's tag to the current build once the sweep
            /// has listed the store.
            struct Moving(Store);
            #[async_trait]
            impl ContainerCli for Moving {
                async fn run(&self, args: &[String]) -> Result<String, CliError> {
                    let out = self.0.run(args).await;
                    if args[0] == "list" {
                        for e in self.0.entries.lock().unwrap().iter_mut() {
                            if e.name == format!("{REPO}:old") {
                                e.id = "rcur".into();
                            }
                        }
                    }
                    out
                }
            }
            let store = Moving(store());
            let swept = apple::sweep(&store, &knowledge(), true).await.unwrap();
            let old = swept
                .iter()
                .find(|s| s.name == format!("{REPO}:old"))
                .unwrap();
            assert!(!old.removed, "{old:?}");
            assert!(
                old.error
                    .as_deref()
                    .unwrap_or_default()
                    .contains("another image"),
                "{old:?}"
            );
            assert!(!store
                .0
                .deleted
                .lock()
                .unwrap()
                .contains(&format!("{REPO}:old")));
        }
    }
}
