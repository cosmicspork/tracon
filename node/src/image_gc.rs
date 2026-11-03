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
//! nothing any remaining image is built on. The classification is pure, over
//! an image list a provider reads ([`Image`]); [`podman`] reads and removes
//! Podman's.

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
    /// Sessions run from the bundle on the base (`sandbox_images`), so harness
    /// images and layers over repository images serve nothing.
    pub bundled: bool,
    /// The harness images' repositories (and, under tunnels, the gateway's).
    pub legacy_repositories: Vec<String>,
    pub now_ms: i64,
    /// Remove legacy images too.
    pub legacy: bool,
    /// Take unlabelled images named as this node's for its own.
    pub adopt_unlabelled: bool,
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
    let mut children = vec![0usize; images.len()];
    for image in images {
        if let Some(parent) = image.parent.as_deref().and_then(|p| by_id.get(p)) {
            children[*parent] += 1;
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

    let mut verdicts = Vec::new();
    for (at, image) in images.iter().enumerate() {
        let owner = owner(image, k);
        if owner == Owner::Theirs {
            continue;
        }
        let Some(shape) = shape(image, k, owner, children[at] == 0) else {
            continue;
        };
        let (class, remove, reason) = judge(image, at, shape, owner, k, &kept, pinned);
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
    // keeps what it stands on in turn.
    loop {
        let removing: HashSet<usize> = verdicts
            .iter()
            .filter(|v| v.remove)
            .map(|v| v.index)
            .collect();
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

fn judge(
    image: &Image,
    at: usize,
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
    if image.containers > 0 {
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
            "harness images run sessions while [runtime.podman] sandbox_images is off",
        ),
        Class::Legacy if !k.legacy => keep(
            class,
            "unused with sandbox_images; removed only with --legacy",
        ),
        Class::Legacy => remove("unused with sandbox_images"),
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
    /// `apply`, remove those that go.
    pub async fn sweep(k: &Knowledge, apply: bool) -> Result<Vec<Swept>, String> {
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
        let verdicts = classify(&images, &k);
        let mut order: Vec<&Verdict> = verdicts.iter().filter(|v| v.remove).collect();
        order.sort_by_key(|v| std::cmp::Reverse(depth(&images, v.index)));
        let mut outcome: HashMap<usize, Result<(), String>> = HashMap::new();
        if apply {
            for verdict in order {
                outcome.insert(verdict.index, remove(&images[verdict.index]).await);
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
}
