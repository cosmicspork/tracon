//! `tracon gc`'s image sweep against real Podman: tiny images under a
//! scratch prefix, each standing for one thing the sweep has to tell apart
//! (a superseded snapshot, an unused repository image, one a container uses,
//! an unlabelled one, another node's, a dangling build, legacy images, a
//! superseded and a pinned base), then a dry run, an apply, and an apply
//! with `--adopt-unlabelled --legacy`, checking exactly the right ones went.
//!
//! Runs only when asked for, and pulls nothing (every image is `FROM
//! scratch` or one of the test's own):
//!
//! ```sh
//! TRACON_PODMAN_TESTS=1 cargo test -p tracon --test image_gc -- --test-threads 1
//! ```
//!
//! Everything it makes is named `localhost/spike-b4-…` (the test's resource
//! prefix) and removed again. It also checks that no image it did not make
//! lost an id or a name while it ran.

#[path = "support/mod.rs"]
mod support;

use std::collections::{BTreeSet, HashMap};

use tracon::config::Config;
use tracon::image_gc::{Class, Kept, Knowledge, Swept};

use support::podman::{podman, requested};

const PREFIX: &str = "spike-b4";
const NODE: &str = "spike-b4-node";
const BASE: &str = "localhost/spike-b4-base";
const REPO: &str = "localhost/spike-b4-repo-app-0123456789";
const SNAP: &str = "localhost/spike-b4-snapshot-app-0123456789";
const HARNESS: &str = "localhost/spike-b4-harness-claude";
const CONTAINER: &str = "spike-b4-ctr";

/// Every image id and name in the store, intermediate images included.
fn store_lines() -> BTreeSet<String> {
    podman(&[
        "images",
        "-a",
        "--no-trunc",
        "--format",
        "{{.ID}} {{.Repository}}:{{.Tag}}",
    ])
    .expect("podman images")
    .lines()
    .map(str::to_string)
    .collect()
}

/// Everything named for the test, and every image id it made.
fn clean(ids: &[String]) {
    let _ = podman(&["rm", "-f", CONTAINER]);
    if let Some(names) = podman(&["images", "--format", "{{.Repository}}:{{.Tag}}"]) {
        for name in names
            .lines()
            .filter(|n| n.starts_with(&format!("localhost/{PREFIX}-")))
        {
            let _ = podman(&["image", "rm", "-f", name]);
        }
    }
    for id in ids {
        let _ = podman(&["image", "rm", "-f", id]);
    }
}

struct Made {
    ids: Vec<String>,
}

impl Drop for Made {
    fn drop(&mut self) {
        clean(&self.ids);
    }
}

impl Made {
    /// `FROM from`, one file of its own, `labels`, tagged `name`. Old (made
    /// at the epoch) unless `new`, so the sweep's grace does not hold it.
    fn build(&mut self, name: &str, from: &str, labels: &[(String, String)], new: bool) -> String {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("f"), name).unwrap();
        std::fs::write(
            dir.path().join("Containerfile"),
            format!("FROM {from}\nCOPY f /f\n"),
        )
        .unwrap();
        let mut args: Vec<String> = vec!["build".into(), "--pull=never".into(), "-q".into()];
        if !new {
            args.push("--timestamp".into());
            args.push("0".into());
        }
        for (key, value) in labels {
            args.push("--label".into());
            args.push(format!("{key}={value}"));
        }
        args.extend(["-t".into(), name.into(), dir.path().display().to_string()]);
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        podman(&args).unwrap_or_else(|| panic!("build {name}"));
        let id = id_of(name).unwrap();
        self.ids.push(id.clone());
        id
    }
}

fn id_of(reference: &str) -> Option<String> {
    podman(&["image", "inspect", "--format", "{{.Id}}", reference])
}

fn digest_of(reference: &str) -> String {
    podman(&["image", "inspect", "--format", "{{.Digest}}", reference]).unwrap()
}

fn exists(id: &str) -> bool {
    podman(&["image", "exists", id]).is_some()
}

fn labels(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

/// The verdict on the image named `name` (or, for a dangling one, `id`).
fn find<'a>(swept: &'a [Swept], name: &str) -> &'a Swept {
    swept
        .iter()
        .find(|s| s.name == name)
        .unwrap_or_else(|| panic!("{name} not listed: {swept:#?}"))
}

/// Names the process's resources from the test prefix, as the first
/// configuration a process loads does.
fn claim_resource_prefix() {
    let path = Config::config_path();
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    if !path.exists() {
        std::fs::write(
            &path,
            format!("[boundary]\nresource_prefix = \"{PREFIX}\"\n"),
        )
        .unwrap();
    }
    Config::load();
    assert_eq!(tracon::config::resource_prefix(), PREFIX);
}

#[tokio::test]
async fn a_sweep_removes_exactly_this_node_s_unused_images() {
    if !requested() {
        return;
    }
    support::state::isolate();
    claim_resource_prefix();
    tracon::image_gc::set_node_id(NODE);
    let ours = tracon::image_gc::owner_labels();
    assert_eq!(
        ours,
        labels(&[
            (tracon::image_gc::PREFIX_LABEL, PREFIX),
            (tracon::image_gc::NODE_LABEL, NODE),
        ])
    );
    let theirs = labels(&[
        (tracon::image_gc::PREFIX_LABEL, PREFIX),
        (tracon::image_gc::NODE_LABEL, "another-node"),
    ]);
    let snapshot = |from: &str| -> Vec<(String, String)> {
        let mut l = ours.clone();
        l.extend(labels(&[
            (tracon::sandbox_images::KEY_LABEL, "k"),
            (tracon::sandbox_images::FROM_LABEL, from),
        ]));
        l
    };

    clean(&[]);
    let before = store_lines();
    let mut made = Made { ids: Vec::new() };
    let mut id: HashMap<&str, String> = HashMap::new();

    id.insert(
        "base0",
        made.build(&format!("{BASE}:0"), "scratch", &[], false),
    );
    id.insert(
        "base1",
        made.build(&format!("{BASE}:1"), "scratch", &[], false),
    );
    id.insert(
        "base2",
        made.build(&format!("{BASE}:2"), "scratch", &[], false),
    );
    id.insert(
        "cur",
        made.build(&format!("{REPO}:cur"), &format!("{BASE}:0"), &ours, false),
    );
    let cur = format!("{REPO}@{}", digest_of(&format!("{REPO}:cur")));
    let on2 = format!("{BASE}:2");
    id.insert(
        "old",
        made.build(&format!("{REPO}:old"), &on2, &ours, false),
    );
    id.insert(
        "inuse",
        made.build(&format!("{REPO}:inuse"), &on2, &ours, false),
    );
    id.insert("new", made.build(&format!("{REPO}:new"), &on2, &ours, true));
    id.insert(
        "unlabelled",
        made.build(&format!("{REPO}:unlabelled"), &on2, &[], false),
    );
    id.insert(
        "theirs",
        made.build(&format!("{REPO}:theirs"), &on2, &theirs, false),
    );
    id.insert(
        "moved",
        made.build(&format!("{REPO}:moved"), &on2, &ours, false),
    );
    // The same tag again, from other content: the first is left dangling, and
    // the second, just made, is in its grace.
    id.insert(
        "moved2",
        made.build(&format!("{REPO}:moved"), &format!("{BASE}:0"), &ours, true),
    );
    assert_ne!(id["moved"], id["moved2"]);
    id.insert(
        "layer",
        made.build(
            &format!("{REPO}-claude:k"),
            &format!("{REPO}:cur"),
            &ours,
            false,
        ),
    );
    id.insert(
        "src",
        made.build(&format!("{REPO}-src:k"), &on2, &ours, false),
    );
    id.insert(
        "snap-cur",
        made.build(
            &format!("{SNAP}:cur"),
            &format!("{REPO}:cur"),
            &snapshot(&cur),
            false,
        ),
    );
    id.insert(
        "snap-old",
        made.build(
            &format!("{SNAP}:old"),
            &format!("{REPO}:cur"),
            &snapshot(&cur),
            false,
        ),
    );
    // On an unlabelled image, as a snapshot from before the labels is: one on
    // a labelled image would inherit its labels.
    id.insert(
        "snap-unlabelled",
        made.build(
            &format!("{SNAP}:unlabelled"),
            &on2,
            &labels(&[(tracon::sandbox_images::KEY_LABEL, "k")]),
            false,
        ),
    );
    id.insert(
        "harness",
        made.build(&format!("{HARNESS}:latest"), "scratch", &[], false),
    );
    assert!(
        podman(&[
            "create",
            "--name",
            CONTAINER,
            &format!("{REPO}:inuse"),
            "/f"
        ])
        .is_some(),
        "create {CONTAINER}"
    );

    let knowledge = |legacy: bool, adopt: bool| Knowledge {
        prefix: PREFIX.into(),
        node_id: NODE.into(),
        base_repository: Some(BASE.into()),
        pinned_base: Some(format!("{BASE}@{}", digest_of(&format!("{BASE}:2")))),
        bases_are_ours: true,
        kept: vec![
            Kept {
                reference: cur.clone(),
                why: "the current base build of app".into(),
                legacy: false,
            },
            Kept {
                reference: format!("{SNAP}@{}", digest_of(&format!("{SNAP}:cur"))),
                why: "a repository's current prepared snapshot".into(),
                legacy: false,
            },
        ],
        bundled: true,
        legacy_repositories: vec![HARNESS.into()],
        now_ms: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64,
        legacy,
        adopt_unlabelled: adopt,
    };

    // The dry run.
    let dry = tracon::image_gc::podman::sweep(&knowledge(false, false), false)
        .await
        .unwrap();
    let dangling = format!("<none> {}", &id["moved"][..12]);
    let expect: &[(&str, Class, bool)] = &[
        ("base0", Class::Base, false),
        ("base1", Class::Base, true),
        ("base2", Class::Base, false),
        ("cur", Class::Repository, false),
        ("old", Class::Repository, true),
        ("inuse", Class::Repository, false),
        ("new", Class::Repository, false),
        ("unlabelled", Class::Unowned, false),
        ("moved", Class::Dangling, true),
        ("moved2", Class::Repository, false),
        ("layer", Class::Legacy, false),
        ("src", Class::Legacy, false),
        ("snap-cur", Class::Snapshot, false),
        ("snap-old", Class::Snapshot, true),
        ("snap-unlabelled", Class::Unowned, false),
        ("harness", Class::Legacy, false),
    ];
    let names: HashMap<&str, String> = [
        ("base0", format!("{BASE}:0")),
        ("base1", format!("{BASE}:1")),
        ("base2", format!("{BASE}:2")),
        ("cur", format!("{REPO}:cur")),
        ("old", format!("{REPO}:old")),
        ("inuse", format!("{REPO}:inuse")),
        ("new", format!("{REPO}:new")),
        ("unlabelled", format!("{REPO}:unlabelled")),
        ("moved", dangling.clone()),
        ("moved2", format!("{REPO}:moved")),
        ("layer", format!("{REPO}-claude:k")),
        ("src", format!("{REPO}-src:k")),
        ("snap-cur", format!("{SNAP}:cur")),
        ("snap-old", format!("{SNAP}:old")),
        ("snap-unlabelled", format!("{SNAP}:unlabelled")),
        ("harness", format!("{HARNESS}:latest")),
    ]
    .into_iter()
    .collect();
    for (key, class, remove) in expect {
        let it = find(&dry, &names[key]);
        assert_eq!((it.class, it.remove), (*class, *remove), "{key}: {it:?}");
        eprintln!(
            "{:<6} {:<10} {:>6} B  {}  — {}",
            if it.remove { "remove" } else { "keep" },
            it.class.as_str(),
            it.reclaim,
            it.name,
            it.reason
        );
    }
    assert!(find(&dry, &names["inuse"]).reason.contains("container"));
    assert!(find(&dry, &names["new"]).reason.contains("last hour"));
    assert!(find(&dry, &names["base0"]).reason.contains("built on it"));
    assert!(
        !dry.iter().any(|s| s.name == format!("{REPO}:theirs")),
        "another node's image is not listed"
    );
    assert!(dry.iter().all(|s| !s.removed), "a dry run removes nothing");
    let reclaim: u64 = dry.iter().filter(|s| s.remove).map(|s| s.reclaim).sum();
    assert!(reclaim > 0);
    for key in ["base1", "old", "moved", "snap-old"] {
        assert!(exists(&id[key]), "{key} is still there after the dry run");
    }

    // Apply: exactly the four go.
    let applied = tracon::image_gc::podman::sweep(&knowledge(false, false), true)
        .await
        .unwrap();
    for it in applied.iter().filter(|s| s.remove) {
        assert!(it.removed && it.error.is_none(), "{it:?}");
    }
    let gone = ["base1", "old", "moved", "snap-old"];
    for (key, image) in &id {
        assert_eq!(!exists(image), gone.contains(key), "{key} ({image})");
    }

    // Adopting the unlabelled and taking the legacy ones.
    let adopted = tracon::image_gc::podman::sweep(&knowledge(true, true), true)
        .await
        .unwrap();
    for it in adopted.iter().filter(|s| s.remove) {
        assert!(it.removed && it.error.is_none(), "{it:?}");
    }
    let gone = [
        "base1",
        "old",
        "moved",
        "snap-old",
        "unlabelled",
        "snap-unlabelled",
        "layer",
        "src",
        "harness",
    ];
    for (key, image) in &id {
        assert_eq!(!exists(image), gone.contains(key), "{key} ({image})");
    }

    // Nothing the test did not make lost an id or a name.
    let after = store_lines();
    let lost: Vec<&String> = before.difference(&after).collect();
    assert!(
        lost.is_empty(),
        "images the test did not make changed: {lost:?}"
    );

    drop(made);
}
