use super::*;

/// An archive entry written with raw header bytes, so it can say what the
/// `tar` crate's own path setters refuse to: what a hostile sandbox would
/// send.
fn raw_entry(
    builder: &mut tar::Builder<Vec<u8>>,
    kind: tar::EntryType,
    name: &[u8],
    link: Option<&[u8]>,
    data: &[u8],
) {
    let mut header = tar::Header::new_gnu();
    {
        let gnu = header.as_gnu_mut().unwrap();
        gnu.name[..name.len()].copy_from_slice(name);
        if let Some(link) = link {
            gnu.linkname[..link.len()].copy_from_slice(link);
        }
    }
    header.set_entry_type(kind);
    header.set_mode(0o644);
    header.set_size(data.len() as u64);
    header.set_cksum();
    builder.append(&header, data).unwrap();
}

fn archive(build: impl FnOnce(&mut tar::Builder<Vec<u8>>)) -> Vec<u8> {
    let mut builder = tar::Builder::new(Vec::new());
    build(&mut builder);
    builder.into_inner().unwrap()
}

fn file(builder: &mut tar::Builder<Vec<u8>>, name: &str, data: &[u8]) {
    raw_entry(
        builder,
        tar::EntryType::Regular,
        name.as_bytes(),
        None,
        data,
    );
}

fn git(dir: &Path, args: &[&str]) {
    let out = std::process::Command::new("git")
        .args(["-c", "user.name=t", "-c", "user.email=t@example.com"])
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// A store holding one volume with a single file in it.
fn store_with(volume: &str) -> (tempfile::TempDir, FilesStore) {
    let dir = tempfile::tempdir().unwrap();
    let store = FilesStore::new(dir.path().join("store"));
    let seed = dir.path().join("seed");
    std::fs::create_dir_all(&seed).unwrap();
    std::fs::write(seed.join("kept.txt"), "before").unwrap();
    store.import(volume, &seed).unwrap();
    (dir, store)
}

fn listing(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

/// The sandbox is not trusted: whatever it sends that `validate_tree` would
/// refuse is refused, and the node's copy is left exactly as it was, with
/// no staging left behind.
#[tokio::test]
async fn a_hostile_archive_changes_nothing() {
    let small = Limits {
        max_bytes: 16,
        max_files: 3,
        max_dirs: 3,
        max_path_bytes: 64,
        inert_links: false,
    };
    let cases: Vec<(&str, Vec<u8>, Limits)> = vec![
        (
            "a link out of the tree",
            archive(|b| raw_entry(b, tar::EntryType::Symlink, b"out", Some(b"../../etc"), b"")),
            checkout_limits(),
        ),
        (
            "an absolute link",
            archive(|b| {
                raw_entry(
                    b,
                    tar::EntryType::Symlink,
                    b"passwd",
                    Some(b"/etc/passwd"),
                    b"",
                )
            }),
            checkout_limits(),
        ),
        (
            "a climbing path",
            archive(|b| file(b, "../escaped", b"x")),
            checkout_limits(),
        ),
        (
            "an absolute path",
            archive(|b| file(b, "/tmp/tracon-escaped", b"x")),
            checkout_limits(),
        ),
        (
            "a write through a link",
            archive(|b| {
                raw_entry(b, tar::EntryType::Directory, b"sub", None, b"");
                raw_entry(b, tar::EntryType::Symlink, b"via", Some(b"sub"), b"");
                file(b, "via/f", b"x");
            }),
            checkout_limits(),
        ),
        (
            "a character device",
            archive(|b| raw_entry(b, tar::EntryType::Char, b"tty", None, b"")),
            checkout_limits(),
        ),
        (
            "a block device",
            archive(|b| raw_entry(b, tar::EntryType::Block, b"sda", None, b"")),
            checkout_limits(),
        ),
        (
            "a FIFO",
            archive(|b| raw_entry(b, tar::EntryType::Fifo, b"pipe", None, b"")),
            checkout_limits(),
        ),
        (
            "a hard link",
            archive(|b| {
                file(b, "a", b"x");
                raw_entry(b, tar::EntryType::Link, b"b", Some(b"a"), b"");
            }),
            checkout_limits(),
        ),
        (
            "too many bytes",
            archive(|b| file(b, "big", &[b'x'; 17])),
            small,
        ),
        (
            "too many files",
            archive(|b| {
                for name in ["a", "b", "c", "d"] {
                    file(b, name, b"x");
                }
            }),
            small,
        ),
        (
            "too long a path",
            archive(|b| file(b, &"p".repeat(65), b"x")),
            small,
        ),
    ];
    for (what, bytes, limits) in cases {
        let volume = "tracon-workspace-hostile";
        let (dir, store) = store_with(volume);
        let received = store
            .receive(
                volume,
                "",
                "/work",
                std::io::Cursor::new(bytes),
                Mode::Whole,
                limits,
                &[],
            )
            .await;
        assert!(received.is_err(), "{what} was accepted");
        let tree = store.tree(volume).unwrap();
        assert_eq!(listing(&tree), vec!["kept.txt"], "{what}");
        assert_eq!(
            std::fs::read_to_string(tree.join("kept.txt")).unwrap(),
            "before",
            "{what}"
        );
        assert_eq!(listing(store.root()), vec![volume], "{what} left staging");
        assert!(!dir.path().join("escaped").exists(), "{what}");
        assert!(!Path::new("/tmp/tracon-escaped").exists(), "{what}");
    }
}

/// A sync through a link the node's copy holds is refused rather than
/// written wherever the link leads.
#[tokio::test]
async fn a_sync_is_never_written_through_a_link_in_the_store() {
    let volume = "tracon-scratch-linked";
    let (dir, store) = store_with(volume);
    let elsewhere = dir.path().join("elsewhere");
    std::fs::create_dir_all(&elsewhere).unwrap();
    std::os::unix::fs::symlink(&elsewhere, store.tree(volume).unwrap().join("harness")).unwrap();
    let refused = store
        .receive(
            volume,
            "harness/run",
            "/home/harness/run",
            std::io::Cursor::new(archive(|b| file(b, "journal", b"x"))),
            Mode::Whole,
            checkout_limits(),
            &[],
        )
        .await;
    assert!(refused.is_err());
    assert!(listing(&elsewhere).is_empty());
}

/// An alias names a tree that is someone else's (a candidate's snapshot, a
/// preparation's tree): nothing is ever written through it.
#[tokio::test]
async fn an_alias_is_never_written() {
    let dir = tempfile::tempdir().unwrap();
    let store = FilesStore::new(dir.path().join("store"));
    let snapshot = dir.path().join("snapshot");
    std::fs::create_dir_all(&snapshot).unwrap();
    std::fs::write(snapshot.join("a"), "candidate").unwrap();
    store.alias("tracon-check-1", &snapshot).unwrap();
    assert_eq!(store.tree("tracon-check-1").unwrap(), snapshot);
    let refused = store
        .receive(
            "tracon-check-1",
            "",
            "/work",
            std::io::Cursor::new(archive(|b| file(b, "a", b"changed"))),
            Mode::Whole,
            checkout_limits(),
            &[],
        )
        .await;
    assert!(refused.is_err());
    assert_eq!(
        std::fs::read_to_string(snapshot.join("a")).unwrap(),
        "candidate"
    );
    // Removing the alias leaves what it named.
    store.remove("tracon-check-1").unwrap();
    assert!(!store.holds("tracon-check-1"));
    assert!(snapshot.join("a").exists());
}

/// A turn's checkout replaces the node's copy (an edit lands, a deletion
/// stays deleted), while what the node held that Git ignores, such as a
/// preparation's install, is kept rather than sent back every turn.
#[tokio::test]
async fn a_checkout_comes_back_and_the_ignored_install_stays() {
    let dir = tempfile::tempdir().unwrap();
    let store = FilesStore::new(dir.path().join("store"));
    let seed = dir.path().join("seed");
    std::fs::create_dir_all(seed.join("node_modules/dep")).unwrap();
    std::fs::write(seed.join(".gitignore"), "node_modules/\n").unwrap();
    std::fs::write(seed.join("a.txt"), "one").unwrap();
    std::fs::write(seed.join("gone.txt"), "soon").unwrap();
    git(&seed, &["init", "-q", "-b", "main"]);
    git(&seed, &["add", "."]);
    git(&seed, &["commit", "-q", "-m", "seed"]);
    std::fs::write(seed.join("node_modules/dep/index.js"), "installed").unwrap();
    let volume = "tracon-workspace-merge";
    store.import(volume, &seed).unwrap();

    // What the sandbox's checkout is after a turn.
    let sandbox = dir.path().join("sandbox");
    crate::workspace::copy_tree(&seed, &sandbox, false).unwrap();
    std::fs::remove_dir_all(sandbox.join("node_modules")).unwrap();
    std::fs::write(sandbox.join("a.txt"), "two").unwrap();
    std::fs::write(sandbox.join("b.txt"), "new").unwrap();
    git(&sandbox, &["rm", "-q", "gone.txt"]);
    let mut bytes = Vec::new();
    tracon_sandbox::files::pack(&sandbox, Exclusions::Checkout, &mut bytes).unwrap();

    store
        .receive(
            volume,
            "",
            "/work",
            std::io::Cursor::new(bytes),
            Mode::Checkout,
            checkout_limits(),
            &[],
        )
        .await
        .unwrap();
    let tree = store.tree(volume).unwrap();
    assert_eq!(std::fs::read_to_string(tree.join("a.txt")).unwrap(), "two");
    assert_eq!(std::fs::read_to_string(tree.join("b.txt")).unwrap(), "new");
    assert!(!tree.join("gone.txt").exists());
    assert_eq!(
        std::fs::read_to_string(tree.join("node_modules/dep/index.js")).unwrap(),
        "installed"
    );
    assert_eq!(listing(store.root()), vec![volume]);
}

/// What another mount provides inside a tree (a sealed configuration
/// directory, the orientation) is the node's to send, never the sandbox's
/// to send back.
#[tokio::test]
async fn nested_mounts_are_never_taken_back() {
    let volume = "tracon-scratch-nested";
    let (_dir, store) = store_with(volume);
    let bytes = archive(|b| {
        file(b, "projects/t.jsonl", b"{}");
        raw_entry(b, tar::EntryType::Directory, b"plugin", None, b"");
        file(b, "plugin/skill.md", b"planted");
        file(b, "orientation.md", b"planted");
    });
    let own = crate::session::materialize::OWN_STATE;
    store
        .receive(
            volume,
            own,
            "/home/harness/.agent",
            std::io::Cursor::new(bytes),
            Mode::Whole,
            checkout_limits(),
            &[PathBuf::from("plugin"), PathBuf::from("orientation.md")],
        )
        .await
        .unwrap();
    let state = store.tree(volume).unwrap().join(own);
    assert_eq!(listing(&state), vec!["projects"]);
    // The rest of the volume is untouched.
    assert!(store.tree(volume).unwrap().join("kept.txt").exists());
}

/// The provider mounts only what the store does not keep; each directory
/// the store sends lands on storage of the sandbox's own unless it is inside
/// another, and the trees go in parents first.
#[test]
fn a_plan_mounts_only_what_the_store_does_not_keep() {
    let dir = tempfile::tempdir().unwrap();
    let store = FilesStore::new(dir.path().join("store"));
    let seed = dir.path().join("seed");
    std::fs::create_dir_all(seed.join("harness/plugin")).unwrap();
    std::fs::write(seed.join("orientation.md"), "# o").unwrap();
    std::fs::write(seed.join("gitconfig"), "[user]").unwrap();
    store.import("tracon-scratch-s", &seed).unwrap();
    let work = dir.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    store.import("tracon-workspace-w", &work).unwrap();

    let mounts = vec![
        Mount::at(
            "tracon-scratch-s",
            "harness/plugin",
            "/root/.claude/plugin",
            true,
        ),
        Mount::at("tracon-scratch-s", "state", "/root/.claude", false),
        Mount::at(
            "tracon-scratch-s",
            "orientation.md",
            "/root/.claude/orientation.md",
            true,
        ),
        Mount::at("tracon-scratch-s", "gitconfig", "/root/.gitconfig", true),
        Mount::volume("tracon-workspace-w", "/work", false),
        Mount::volume("tracon-cache-r-1", "/cache", false),
    ];
    let (provided, plan) = store.plan(&mounts).unwrap();
    assert_eq!(
        provided,
        vec![
            Mount::volume("tracon-cache-r-1", "/cache", false),
            Mount::local("/work"),
            Mount::local("/root"),
        ]
    );
    let plan = plan.unwrap();
    let targets = plan.targets();
    let at = |t: &str| targets.iter().position(|x| *x == t).unwrap();
    assert!(at("/root/.claude") < at("/root/.claude/plugin"));
    assert!(at("/root/.claude") < at("/root/.claude/orientation.md"));
    assert_eq!(targets.len(), 5);
    // Taken back after a node restart, a sandbox is sent again what the node
    // writes for each launch, and keeps what it writes itself.
    let mut resent = plan.resent();
    resent.sort_unstable();
    assert_eq!(
        resent,
        vec![
            "/root/.claude/orientation.md",
            "/root/.claude/plugin",
            "/root/.gitconfig",
        ]
    );

    // Nothing kept: everything is mounted as before.
    let (provided, plan) = FilesStore::new(dir.path().join("empty"))
        .plan(&mounts)
        .unwrap();
    assert_eq!(provided, mounts);
    assert!(plan.is_none());
}

/// Which trees come back from which runs: a session's workspace as its
/// checkout and its state whole, a preparation's whole, a fetch's workspace
/// as its checkout, and nothing from a check or a probe, nor from a check's
/// copy whatever runs in it.
#[test]
fn what_comes_back_from_which_run() {
    let mode = |volume: &str, kind| sync_mode(volume, kind).map(|(mode, _)| mode);
    assert_eq!(
        mode("tracon-workspace-a", RunKind::Session),
        Some(Mode::Checkout)
    );
    assert_eq!(
        mode("tracon-scratch-a", RunKind::Session),
        Some(Mode::Whole)
    );
    assert_eq!(
        mode("tracon-workspace-a", RunKind::Prepare),
        Some(Mode::Whole)
    );
    assert_eq!(
        sync_mode("tracon-prep-a", RunKind::Prepare).map(|(_, l)| l),
        Some(PREPARED_LIMITS)
    );
    // A fetch's workspace comes back as its checkout, within a checkout's
    // limits, never whole with a preparation's; nothing else comes back.
    assert_eq!(
        sync_mode("tracon-workspace-a", RunKind::Fetch),
        Some((Mode::Checkout, checkout_limits()))
    );
    assert_eq!(mode("tracon-scratch-a", RunKind::Fetch), None);
    assert_eq!(mode("tracon-check-a", RunKind::Fetch), None);
    assert_eq!(mode("tracon-workspace-a", RunKind::Check), None);
    assert_eq!(mode("tracon-scratch-a", RunKind::Probe), None);
    assert_eq!(mode("tracon-check-a", RunKind::Session), None);
    assert_eq!(mode("tracon-check-a", RunKind::Prepare), None);
    // A cache a session is sent never comes back from it; a preparation's
    // does, since filling it is what the preparation is for.
    assert_eq!(mode("tracon-cache-r-a", RunKind::Session), None);
    assert_eq!(mode("tracon-prep-cache-a", RunKind::Session), None);
    assert_eq!(
        mode("tracon-prep-cache-a", RunKind::Prepare),
        Some(Mode::Whole)
    );

    assert!(backs("tracon-workspace-a"));
    assert!(backs("tracon-scratch-a"));
    assert!(backs("tracon-check-a"));
    assert!(backs("tracon-prep-a"));
    assert!(!backs("tracon-prep-cache-a"));
    assert!(!backs("tracon-cache-r-a"));
    assert!(!backs(&crate::session::materialize::harness_state_volume()));
}

/// A check's copy of something the store keeps is the tree itself, and a
/// copy of anything else is a copy.
#[tokio::test]
async fn a_checks_copy_is_an_alias_and_any_other_is_a_copy() {
    let (_dir, store) = store_with("tracon-prep-1");
    store
        .clone_volume("tracon-prep-1", "tracon-check-1")
        .await
        .unwrap();
    assert_eq!(
        store.tree("tracon-check-1").unwrap(),
        store.tree("tracon-prep-1").unwrap()
    );
    store
        .clone_volume("tracon-prep-1", "tracon-try-1-c0")
        .await
        .unwrap();
    let copy = store.tree("tracon-try-1-c0").unwrap();
    assert_ne!(copy, store.tree("tracon-prep-1").unwrap());
    assert!(copy.join("kept.txt").is_file());
    let mut names: Vec<_> = store.list().unwrap().into_iter().map(|v| v.name).collect();
    names.sort();
    assert_eq!(
        names,
        vec!["tracon-check-1", "tracon-prep-1", "tracon-try-1-c0"]
    );
}

/// Names that could leave the store are refused.
#[test]
fn a_volume_name_cannot_leave_the_store() {
    let dir = tempfile::tempdir().unwrap();
    let store = FilesStore::new(dir.path().join("store"));
    for name in ["", "../x", "a/b", "tracon-workspace-a.staging-1"] {
        assert!(store.import(name, dir.path()).is_err(), "{name}");
        assert!(!store.holds(name), "{name}");
    }
    assert!(relative("../x").is_err());
    assert!(relative("/etc").is_err());
    assert_eq!(
        relative("harness/run").unwrap(),
        PathBuf::from("harness/run")
    );
}

/// A session's state mount becomes its own subtree of its scratch volume.
#[test]
fn a_session_gets_its_own_state() {
    let mut mounts = vec![
        Mount::volume(
            crate::session::materialize::harness_state_volume(),
            "/root/.claude",
            false,
        ),
        Mount::at(
            "tracon-scratch-s",
            "harness/plugin",
            "/root/.claude/plugin",
            true,
        ),
    ];
    crate::session::materialize::own_state(&mut mounts, "tracon-scratch-s");
    assert_eq!(
        mounts[0],
        Mount::at("tracon-scratch-s", "state", "/root/.claude", false)
    );
    assert_eq!(mounts[1].sub_path, "harness/plugin");
}

/// A file is sent into a directory of the sandbox's own: its parent, where
/// no directory sent in already holds it. The gitconfig beside the harness's
/// state makes the home local, which then holds the state too.
#[test]
fn a_files_parent_is_the_sandboxs_own() {
    let dir = tempfile::tempdir().unwrap();
    let store = FilesStore::new(dir.path().join("store"));
    let seed = dir.path().join("seed");
    std::fs::create_dir_all(seed.join("state")).unwrap();
    std::fs::write(seed.join("gitconfig"), "[user]").unwrap();
    std::fs::write(seed.join("orientation.md"), "# o").unwrap();
    store.import("tracon-scratch-s", &seed).unwrap();
    let home = crate::sandbox_images::HOME;
    let mounts = vec![
        Mount::at(
            "tracon-scratch-s",
            "state",
            format!("{home}/.claude"),
            false,
        ),
        Mount::at(
            "tracon-scratch-s",
            "orientation.md",
            format!("{home}/.claude/orientation.md"),
            true,
        ),
        Mount::at(
            "tracon-scratch-s",
            "gitconfig",
            format!("{home}/.gitconfig"),
            true,
        ),
    ];
    let (provided, plan) = store.plan(&mounts).unwrap();
    assert_eq!(provided, vec![Mount::local(home)]);
    assert_eq!(plan.unwrap().targets().len(), 3);

    // Inside a directory sent in, a file needs nothing more; at the root, it
    // is never given the root.
    let (provided, _) = store
        .plan(&[
            Mount::at(
                "tracon-scratch-s",
                "state",
                format!("{home}/.claude"),
                false,
            ),
            Mount::at(
                "tracon-scratch-s",
                "orientation.md",
                format!("{home}/.claude/orientation.md"),
                true,
            ),
            Mount::at("tracon-scratch-s", "gitconfig", "/gitconfig", true),
        ])
        .unwrap();
    assert_eq!(provided, vec![Mount::local(format!("{home}/.claude"))]);
}

/// A withheld volume is never sent, even though the store keeps it: the run
/// is handed it as a mount, for the provider to give empty storage of its own.
#[test]
fn a_withheld_volume_is_provided_not_sent() {
    let (_dir, store) = store_with("tracon-harness-state");
    let store = store.withholding("tracon-harness-state");
    let mounts = vec![Mount::volume(
        "tracon-harness-state",
        "/home/h/.claude",
        false,
    )];
    let (provided, plan) = store.plan(&mounts).unwrap();
    assert_eq!(provided, mounts);
    assert!(plan.is_none());
}

/// A store that keeps every volume makes the one a run names before it is
/// kept, and sends it in like any other.
#[test]
fn a_store_keeping_every_volume_makes_what_it_does_not_hold_yet() {
    let dir = tempfile::tempdir().unwrap();
    let store = FilesStore::new(dir.path().join("store")).keeping_every_volume();
    let mounts = vec![Mount::volume("tracon-cache-r-a", "/cache", false)];
    let (provided, plan) = store.plan(&mounts).unwrap();
    assert_eq!(provided, vec![Mount::local("/cache")]);
    assert_eq!(plan.unwrap().targets(), ["/cache"]);
    assert!(store.holds("tracon-cache-r-a"));
    assert!(FilesStore::new(dir.path().join("store"))
        .keeping_every_volume()
        .plan(&[Mount::volume("../x", "/x", false)])
        .is_err());
}

/// A session's sandbox from a prepared snapshot is sent its workspace and
/// state but not the node's copy of its cache: the snapshot's own `/cache` is
/// under it. Anything else from a snapshot (a check, a preparation, which
/// read or carry a cache between steps), and a session from any other
/// image, is sent the cache as before.
#[test]
fn a_session_from_a_snapshot_is_not_sent_the_cache() {
    let dir = tempfile::tempdir().unwrap();
    let store = FilesStore::new(dir.path().join("store"));
    let seed = dir.path().join("seed");
    std::fs::create_dir_all(seed.join("npm")).unwrap();
    std::fs::write(seed.join("npm/blob"), "cached").unwrap();
    let work = crate::config::resource_name("workspace-w");
    let cache = crate::config::resource_name("cache-r-1");
    let run_cache = crate::config::resource_name("prep-cache-run");
    for volume in [&work, &cache, &run_cache] {
        store.import(volume, &seed).unwrap();
    }
    let snapshot = format!(
        "{}-app-0123456789ab@sha256:{}",
        crate::sandbox_images::snapshot_repo(),
        "a".repeat(64)
    );
    let session = vec![
        Mount::volume(work.clone(), "/work", false),
        Mount::volume(cache.clone(), "/cache", false),
    ];
    let sent = |image: &str, kind, mounts: &[Mount]| {
        let (provided, plan) = store.plan_from(mounts, image, kind).unwrap();
        let targets: Vec<String> = plan
            .map(|plan| plan.targets().into_iter().map(str::to_string).collect())
            .unwrap_or_default();
        (provided, targets)
    };

    // Its workspace lands on the snapshot's own `/work`, in place.
    let (provided, targets) = sent(&snapshot, Some(RunKind::Session), &session);
    assert_eq!(targets, ["/work"]);
    assert_eq!(provided, vec![]);

    for (image, kind) in [
        (snapshot.as_str(), Some(RunKind::Prepare)),
        (snapshot.as_str(), Some(RunKind::Check)),
        (snapshot.as_str(), None),
    ] {
        let (provided, targets) = sent(image, kind, &session);
        assert_eq!(targets, ["/work", "/cache"], "{image} {kind:?}");
        assert_eq!(provided, vec![Mount::local("/cache")], "{image} {kind:?}");
    }
    let (provided, targets) = sent(
        "localhost/tracon-repo-app:1",
        Some(RunKind::Session),
        &session,
    );
    assert_eq!(targets, ["/work", "/cache"]);
    assert_eq!(
        provided,
        vec![Mount::local("/work"), Mount::local("/cache")]
    );
    let check = vec![
        Mount::volume(crate::config::resource_name("check-1"), "/work", false),
        Mount::volume(run_cache.clone(), "/cache", true),
    ];
    store
        .import(&crate::config::resource_name("check-1"), &seed)
        .unwrap();
    let (_, targets) = sent(&snapshot, Some(RunKind::Check), &check);
    assert_eq!(targets, ["/work", "/cache"]);

    // A cache the node does not keep is the runtime's volume, mounted as
    // ever (Podman's, seeded from the snapshot).
    let podman_cache = vec![
        Mount::volume(work.clone(), "/work", false),
        Mount::volume(crate::config::resource_name("cache-r-2"), "/cache", false),
    ];
    let (provided, targets) = sent(&snapshot, Some(RunKind::Session), &podman_cache);
    assert_eq!(targets, ["/work"]);
    assert!(provided.contains(&podman_cache[1]), "{provided:?}");
}

/// Wait, a bounded while, for `done`.
async fn eventually(what: &str, mut done: impl FnMut() -> bool) {
    for _ in 0..500 {
        if done() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("never: {what}");
}

/// Whether the other end of `writer` was dropped: a write fails, rather
/// than waiting for room.
async fn closed(writer: &mut tokio::io::DuplexStream) -> bool {
    use tokio::io::AsyncWriteExt as _;
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if writer.write_all(&[0u8; 512]).await.is_err() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .is_ok()
}

fn eight_files() -> Vec<u8> {
    archive(|b| {
        for i in 0..8 {
            file(b, &format!("f{i}.txt"), b"after");
        }
    })
}

/// A sync given up mid-unpack (its timeout, a caller that went away) stops
/// reading the sandbox's archive, leaves no staging copy behind, and never
/// commits what arrives after.
#[tokio::test]
async fn a_receive_given_up_leaves_nothing_and_stops_its_transfer() {
    use tokio::io::AsyncWriteExt as _;
    let volume = "vol";
    let (_dir, store) = store_with(volume);
    let whole = eight_files();
    let (mut writer, reader) = tokio::io::duplex(64 * 1024);
    // Three of the eight entries (a header and a block each), and the
    // stream left open.
    writer.write_all(&whole[..3 * 1024]).await.unwrap();
    let received = tokio::time::timeout(
        Duration::from_millis(50),
        store.receive(
            volume,
            "",
            "/work",
            reader,
            Mode::Whole,
            checkout_limits(),
            &[],
        ),
    )
    .await;
    assert!(received.is_err(), "the receive did not time out");
    eventually("the staging copy goes", || {
        listing(store.root()) == [volume]
    })
    .await;
    assert!(closed(&mut writer).await, "the transfer was left reading");

    let _ = writer.write_all(&whole[3 * 1024..]).await;
    let _ = writer.shutdown().await;
    drop(writer);
    tokio::time::sleep(Duration::from_millis(100)).await;
    let tree = store.tree(volume).unwrap();
    assert_eq!(listing(&tree), vec!["kept.txt"]);
    assert_eq!(
        std::fs::read_to_string(tree.join("kept.txt")).unwrap(),
        "before"
    );
    assert_eq!(listing(store.root()), vec![volume], "staging came back");
}

/// What a sync holds (the sandbox's sync lock, the volume's lock) is held
/// until the work of a receive it gave up on has ended: the next sync never
/// starts beside it.
#[tokio::test]
async fn a_given_up_receive_holds_its_locks_until_its_work_ends() {
    use tokio::io::AsyncWriteExt as _;
    let volume = "vol";
    let (_dir, store) = store_with(volume);
    let lock = Arc::new(tokio::sync::Mutex::new(()));
    let held = lock.clone().lock_owned().await;
    let whole = eight_files();
    let (mut writer, reader) = tokio::io::duplex(64 * 1024);
    writer.write_all(&whole[..3 * 1024]).await.unwrap();
    let received = tokio::time::timeout(
        Duration::from_millis(50),
        store.receive_holding(
            volume,
            "",
            "/work",
            reader,
            Mode::Whole,
            checkout_limits(),
            &[],
            vec![Box::new(held)],
        ),
    )
    .await;
    assert!(received.is_err(), "the receive did not time out");
    let next = tokio::time::timeout(Duration::from_secs(5), lock.lock())
        .await
        .expect("the lock was never released");
    // Released only once its staging copy was gone.
    assert_eq!(listing(store.root()), vec![volume]);
    drop(next);
}

/// A copy given up leaves no staging copy behind once its `cp` is stopped.
#[tokio::test]
async fn a_clone_given_up_leaves_nothing() {
    let source = crate::config::resource_name("prep-a");
    let (_dir, store) = store_with(&source);
    let dest = crate::config::resource_name("warm-a");
    let cloned = tokio::time::timeout(Duration::ZERO, store.clone_volume(&source, &dest)).await;
    assert!(cloned.is_err(), "the clone did not time out");
    eventually("the staging copy goes", || {
        listing(store.root()) == [source.clone()]
    })
    .await;
}

fn failed(error: &str) -> Result<TreeSummary, (SyncOutcome, String)> {
    Err((SyncOutcome::Failed, error.to_string()))
}

fn taken(files: u64, bytes: u64) -> Result<TreeSummary, (SyncOutcome, String)> {
    Ok(TreeSummary {
        files,
        bytes,
        ..Default::default()
    })
}

#[test]
fn failures_in_a_row_are_reported_once_and_the_recovery_counts_them() {
    let mut t = Tracker {
        last_ok_ms: Some(10),
        ..Default::default()
    };
    let first = t.observe("tracon-h-x", "during a turn", failed("refused"), 5, 100);
    assert!(first.record);
    assert_eq!(first.outcome, SyncOutcome::Failed);
    assert_eq!(first.failures, 1);
    assert_eq!(first.error.as_deref(), Some("refused"));
    assert_eq!(first.last_ok_ms, Some(10));
    assert_eq!(first.sandbox, "tracon-h-x");

    let second = t.observe("tracon-h-x", "turn end", failed("refused again"), 5, 200);
    assert!(!second.record, "a repeated failure is not recorded again");
    assert_eq!(second.failures, 2);
    assert_eq!(
        t.status(true),
        SyncStatus {
            last_ok_ms: Some(10),
            failures: 2,
            last_error: Some("refused again".into()),
            in_flight: false,
            live: true,
        }
    );

    let ok = t.observe("tracon-h-x", "during a turn", taken(3, 40), 7, 300);
    assert!(ok.record);
    assert_eq!(ok.outcome, SyncOutcome::Ok);
    assert_eq!((ok.recovered, ok.failures), (2, 0));
    assert_eq!((ok.files, ok.bytes, ok.ms), (3, 40, 7));
    assert_eq!(ok.last_ok_ms, Some(300));
    assert_eq!(ok.error, None);

    // A success after a success is not worth an event.
    let again = t.observe("tracon-h-x", "during a turn", taken(3, 40), 7, 400);
    assert!(!again.record);
    assert_eq!(t.status(true).last_error, None);
}

#[tokio::test]
async fn a_take_back_past_its_bound_is_reported_timed_out() {
    let late = bounded(
        Duration::from_millis(20),
        std::future::pending::<Result<TreeSummary, FilesError>>(),
    )
    .await;
    let Err((outcome, error)) = late else {
        panic!("a take-back that never ends must time out")
    };
    assert_eq!(outcome, SyncOutcome::TimedOut);
    assert_eq!(
        error.to_string(),
        "taking the trees back took longer than 20ms"
    );

    let mut t = Tracker::default();
    let report = t.observe(
        "tracon-h-x",
        "turn end",
        Err((outcome, error.to_string())),
        20,
        1,
    );
    assert_eq!(report.outcome, SyncOutcome::TimedOut);
    assert_eq!(report.outcome.as_str(), "timed_out");
    assert!(report.record);
}

#[test]
fn the_last_take_back_is_reported_whatever_came_before() {
    // A failure at a turn's end was reported; the exit's, a second failure
    // in a row, is reported too.
    let mut t = Tracker::default();
    assert!(t.observe("s", "turn end", failed("no"), 1, 1).record);
    let exit = t.observe("s", WHY_EXIT, failed("no"), 1, 2);
    assert!(exit.record && exit.is_exit());
    assert_eq!(exit.failures, 2);
    assert!(!t.status(true).live, "nothing is taken back after the last");

    // A success at exit after a reported success is reported as well.
    let mut t = Tracker::default();
    assert!(!t.observe("s", "turn end", taken(1, 1), 1, 1).record);
    let exit = t.observe("s", WHY_EXIT, taken(1, 1), 1, 2);
    assert!(exit.record);
    assert_eq!(exit.outcome, SyncOutcome::Ok);
}
