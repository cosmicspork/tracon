//! Per-session OpenCode state: quiesced backup, migration on a clone, and
//! generation-gated restore.
//!
//! The thing being proved here is narrow and worth saying plainly. Upstream's
//! harness database carries no version: the `migration` table is an applied-id
//! ledger with no `user_version`, no maximum-known check and no error, so an
//! older binary opening a newer database proceeds silently and can write to it
//! (`docs/reference/opencode-v1.18.30/config-state.md` §7.2, §9 row 7). There
//! is no backup command either. Every refusal asserted below is therefore a
//! refusal the node invented, and each one stands between an operator and a
//! silent, destructive write.
//!
//! Most cases run against a database this file builds — a `migration` ledger
//! and a `session` table, which is the shape upstream's schema has — because
//! the gates are about identity and fencing rather than about what OpenCode
//! puts in its rows. The last case runs the real pinned binary when the host
//! has it, and that one is about the rows: a real build makes a real
//! generation, and the restored database has to be one it will open.

#[path = "support/mod.rs"]
mod support;
use support::state;

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use rusqlite::Connection;
use tracon::adapter::opencode::OpenCodeAdapter;
use tracon::config::Config;
use tracon::runner::local::LocalBackend;
use tracon::session::materialize;
use tracon::session::opencode_state::{self as oc, Quiesce, StateError};
use tracon::store::{now_ms, NodeRow, OpenCodeStateRow, SessionRow, Store};

// ---------------------------------------------------------------------------
// A session whose state is on disk where the local backend keeps a volume
// ---------------------------------------------------------------------------

/// Upstream's migration ids are its own timestamped filenames. These stand in
/// for them: the gates compare ids, never their content.
const GENERATION: [&str; 3] = [
    "20260101000000_create_session",
    "20260201000000_add_permission",
    "20260301000000_add_part",
];

fn store_for(id: &str, live: bool) -> Store {
    let store = Store::open_in_memory().unwrap();
    store
        .put_node(&NodeRow {
            id: "n1".into(),
            name: "t".into(),
            state: "ready".into(),
            failed_check: None,
            failed_detail: None,
            harness_id: "opencode".into(),
            harness_pinned: OpenCodeAdapter::PINNED_VERSION.into(),
            harness_found: None,
            models_json: None,
            checked_at_ms: None,
            is_self: 1,
            x25519_pub: None,
            last_seen_ms: None,
            reachable: 1,
            providers_json: None,
        })
        .unwrap();
    store
        .insert_session(&SessionRow {
            id: id.into(),
            node_id: "n1".into(),
            channel: "personal".into(),
            work_item_id: None,
            repo_path: "/r".into(),
            worktree_path: None,
            branch: "b".into(),
            harness_id: "opencode".into(),
            harness_version: OpenCodeAdapter::PINNED_VERSION.into(),
            harness_agent: None,
            harness_found: Some(OpenCodeAdapter::PINNED_VERSION.into()),
            harness_protocol: None,
            harness_session_id: None,
            container_name: None,
            model: "m".into(),
            project_id: None,
            phase: "execute".into(),
            policy_version: None,
            review_id: None,
            budget_tokens: 0,
            tokens_used: 0,
            cost_usd: None,
            context_used: None,
            context_size: None,
            state: if live {
                "running".into()
            } else {
                "closed".into()
            },
            end_reason: None,
            last_error: None,
            turn_active: 0,
            draft: None,
            draft_updated_ms: None,
            created_ms: now_ms(),
            started_mono_ms: None,
            ended_mono_ms: None,
            updated_ms: now_ms(),
            archived_ms: None,
        })
        .unwrap();
    store
}

/// Where the local backend keeps this session's scratch volume, which is the
/// runtime-owned storage a container backend would hold in a named volume.
fn volume_root(id: &str) -> PathBuf {
    tracon::runner::local::local_runtime_path(&tracon::workspace::scratch_volume_name(id))
}

/// Build a database with the shape upstream's has: an applied-id `migration`
/// ledger and a `session` table, in WAL, with the log left uncheckpointed so a
/// backup has something real to fold in.
fn seed_state(id: &str, generation: &[&str], rows: &[&str]) -> PathBuf {
    let db = volume_root(id).join(materialize::OPENCODE_DB);
    std::fs::create_dir_all(db.parent().unwrap()).unwrap();
    let _ = std::fs::remove_file(&db);
    let conn = Connection::open(&db).unwrap();
    conn.pragma_update(None, "journal_mode", "WAL").unwrap();
    conn.execute_batch(
        "CREATE TABLE migration (id TEXT PRIMARY KEY, time_completed INTEGER NOT NULL);
         CREATE TABLE session (id TEXT PRIMARY KEY, title TEXT);",
    )
    .unwrap();
    for (n, m) in generation.iter().enumerate() {
        conn.execute(
            "INSERT INTO migration (id, time_completed) VALUES (?1, ?2)",
            rusqlite::params![m, n as i64],
        )
        .unwrap();
    }
    for row in rows {
        conn.execute(
            "INSERT INTO session (id, title) VALUES (?1, 'a session')",
            rusqlite::params![row],
        )
        .unwrap();
    }
    // Some non-database state, and a cache that must not travel with it.
    let run = volume_root(id).join(materialize::OPENCODE_RUN);
    std::fs::create_dir_all(run.join("data")).unwrap();
    std::fs::write(run.join("data").join("auth.json"), "{}\n").unwrap();
    std::fs::create_dir_all(volume_root(id).join(materialize::OPENCODE_CACHE)).unwrap();
    std::fs::write(
        volume_root(id).join(materialize::OPENCODE_CACHE).join("rg"),
        "a downloaded binary",
    )
    .unwrap();
    db
}

fn record_identity(store: &Store, id: &str, build: &str, generation: &[&str]) {
    store
        .opencode_state_put(&OpenCodeStateRow {
            session_id: id.into(),
            build_version: build.into(),
            build_pinned: OpenCodeAdapter::PINNED_VERSION.into(),
            generation: generation.iter().map(|s| s.to_string()).collect(),
            generation_digest: String::new(),
            manifest_digest: None,
            state_volume: tracon::workspace::scratch_volume_name(id),
            state_path: materialize::HARNESS_TREE.into(),
            recorded_ms: now_ms(),
            updated_ms: now_ms(),
        })
        .unwrap();
}

fn opencode_config() -> Config {
    let mut cfg = Config::default();
    cfg.harness.id = OpenCodeAdapter::ID.into();
    cfg.harness.version = OpenCodeAdapter::PINNED_VERSION.into();
    cfg
}

fn session_ids(db: &Path) -> Vec<String> {
    let conn = Connection::open(db).unwrap();
    let mut stmt = conn.prepare("SELECT id FROM session ORDER BY id").unwrap();
    let ids = stmt
        .query_map([], |r| r.get::<_, String>(0))
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap();
    ids
}

/// The quiesce the operator's `--quiesce` stands for, without a supervisor:
/// it records that it was asked and lets the fence go, which is exactly what a
/// stopped session leaves behind.
struct StopIt {
    asked: std::sync::Mutex<Vec<String>>,
}

#[async_trait]
impl Quiesce for StopIt {
    async fn quiesce(&self, session_id: &str) -> Result<(), String> {
        self.asked.lock().unwrap().push(session_id.to_string());
        materialize::release_state(session_id);
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Backup
// ---------------------------------------------------------------------------

/// A stopped session's state copies, verifies, and is named by the generation
/// the database itself holds — not by whatever the launch managed to guess.
#[tokio::test]
async fn a_stopped_session_backs_up_to_a_verified_copy_and_a_named_generation() {
    state::isolate();
    let id = "state-backup-stopped";
    materialize::release_state(id);
    let store = store_for(id, false);
    seed_state(id, &GENERATION, &["ses_one", "ses_two"]);
    // The launch could not read the generation; the backup settles it.
    record_identity(&store, id, OpenCodeAdapter::PINNED_VERSION, &[]);

    let report = oc::backup(&store, &LocalBackend, None, id)
        .await
        .expect("a stopped session is copyable");
    let manifest = &report.manifest;

    assert_eq!(
        manifest.integrity, "ok",
        "an unverified copy is not a backup"
    );
    assert_eq!(manifest.generation, GENERATION);
    assert_eq!(
        manifest.generation_digest,
        tracon::store::generation_digest(&manifest.generation.clone())
    );
    assert!(!manifest.quiesced, "nothing had to be stopped");
    assert_eq!(manifest.build_version, OpenCodeAdapter::PINNED_VERSION);

    // The copy is a database in its own right, with the rows in it.
    let copied = report.path.join(&manifest.db_file);
    assert_eq!(oc::integrity_of(&copied).unwrap(), "ok");
    assert_eq!(session_ids(&copied), ["ses_one", "ses_two"]);
    assert_eq!(oc::read_generation(&copied).unwrap(), GENERATION);
    assert!(
        !copied.with_extension("db-wal").exists(),
        "the log was folded in, not carried along"
    );

    // The rest of the tree travels; the cache does not.
    let tar = report.path.join(manifest.tar_file.as_deref().unwrap());
    let names: Vec<String> = tar::Archive::new(std::fs::File::open(&tar).unwrap())
        .entries()
        .unwrap()
        .map(|e| e.unwrap().path().unwrap().display().to_string())
        .collect();
    assert!(names.iter().any(|n| n.ends_with("data/auth.json")));
    assert!(
        !names.iter().any(|n| n.contains("run/cache")),
        "a re-downloadable cache is not part of a backup: {names:?}"
    );
    assert!(!names.iter().any(|n| n.ends_with("opencode.db")));

    // And the store now carries what the database actually said.
    assert_eq!(
        store.opencode_state_get(id).unwrap().unwrap().generation,
        GENERATION
    );
    materialize::release_state(id);
}

/// A live session is refused, and the refusal says what to do about it. With
/// `--quiesce` the harness is stopped first — through the supervisor, which is
/// what `StopIt` stands in for — and the same copy is taken.
#[tokio::test]
async fn a_live_session_is_refused_without_quiesce_and_copied_with_it() {
    state::isolate();
    let id = "state-backup-live";
    materialize::release_state(id);
    let store = store_for(id, true);
    seed_state(id, &GENERATION, &["ses_live"]);
    record_identity(&store, id, OpenCodeAdapter::PINNED_VERSION, &GENERATION);
    // A live session holds its own fence, exactly as a launch leaves it.
    materialize::claim_state(id).unwrap();

    let refused = oc::backup(&store, &LocalBackend, None, id)
        .await
        .expect_err("a running session must not be copied under its own writer");
    assert!(matches!(refused, StateError::Running { .. }), "{refused}");
    let said = refused.to_string();
    assert!(said.contains("--quiesce"), "{said}");
    assert!(said.contains("never a kill"), "{said}");

    let stopper = StopIt {
        asked: Default::default(),
    };
    let report = oc::backup(&store, &LocalBackend, Some(&stopper), id)
        .await
        .expect("--quiesce stops the harness and then copies");
    assert_eq!(stopper.asked.lock().unwrap().as_slice(), [id.to_string()]);
    assert!(report.manifest.quiesced);
    assert_eq!(report.manifest.integrity, "ok");
    assert_eq!(session_ids(&report.path.join(oc::DB_FILE)), ["ses_live"]);
    materialize::release_state(id);
}

// ---------------------------------------------------------------------------
// Restore
// ---------------------------------------------------------------------------

/// The gate that matters. A backup carrying migrations this node's OpenCode
/// has never applied is refused: handing it over would be handing an older
/// binary a newer schema, which upstream reads wrongly and writes to anyway.
#[tokio::test]
async fn a_newer_generation_is_refused_into_an_older_pin_and_the_reason_is_said() {
    state::isolate();
    let id = "state-restore-newer";
    materialize::release_state(id);
    let store = store_for(id, false);
    let ahead: Vec<&str> = GENERATION
        .iter()
        .copied()
        .chain(["20260701000000_reset_v2_session_state"])
        .collect();
    seed_state(id, &ahead, &["ses_ahead"]);
    record_identity(&store, id, OpenCodeAdapter::PINNED_VERSION, &ahead);
    let backup = oc::backup(&store, &LocalBackend, None, id).await.unwrap();
    materialize::release_state(id);
    assert_eq!(backup.manifest.generation, ahead);

    // The backup reaches a node whose OpenCode is older: what this runtime
    // actually produces is the narrower generation, and that is all it knows
    // how to read.
    store
        .opencode_state_set_generation(id, &GENERATION.map(String::from))
        .unwrap();

    let refused = oc::restore(
        &store,
        &LocalBackend,
        &opencode_config(),
        id,
        &backup.path,
        true,
    )
    .await
    .expect_err("a newer generation must not go into an older runtime");
    let said = refused.to_string();
    assert!(said.contains("never applied"), "{said}");
    assert!(said.contains("reset_v2_session_state"), "{said}");
    assert!(said.contains("still write to"), "{said}");
    materialize::release_state(id);
}

/// The other half of the same rule, one level up: a backup whose *build* is
/// newer than this node's pin is refused before the generation is even looked
/// at. An older binary opening a newer state proceeds silently, so the version
/// is a gate rather than a warning.
#[tokio::test]
async fn a_newer_build_is_refused_into_an_older_pin() {
    state::isolate();
    let id = "state-restore-newer-build";
    materialize::release_state(id);
    let store = store_for(id, false);
    seed_state(id, &GENERATION, &["ses_newbuild"]);
    record_identity(&store, id, OpenCodeAdapter::PINNED_VERSION, &GENERATION);
    let backup = oc::backup(&store, &LocalBackend, None, id).await.unwrap();
    materialize::release_state(id);

    // As if it had been taken on a node running a later release.
    let manifest_path = backup.path.join(oc::MANIFEST_FILE);
    let mut manifest: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&manifest_path).unwrap()).unwrap();
    manifest["build_version"] = serde_json::json!("1.19.0");
    std::fs::write(&manifest_path, serde_json::to_vec(&manifest).unwrap()).unwrap();

    let refused = oc::restore(
        &store,
        &LocalBackend,
        &opencode_config(),
        id,
        &backup.path,
        true,
    )
    .await
    .expect_err("a newer build's state must not be handed to an older one");
    let said = refused.to_string();
    assert!(said.contains("no downgrade path"), "{said}");
    assert!(said.contains("1.19.0"), "{said}");
    materialize::release_state(id);
}

/// A claimed fence means somebody is writing the very file a restore would
/// overwrite. It is refused before anything is opened.
#[tokio::test]
async fn a_restore_is_refused_while_the_state_is_claimed() {
    state::isolate();
    let id = "state-restore-claimed";
    materialize::release_state(id);
    let store = store_for(id, false);
    seed_state(id, &GENERATION, &["ses_claimed"]);
    record_identity(&store, id, OpenCodeAdapter::PINNED_VERSION, &GENERATION);
    let backup = oc::backup(&store, &LocalBackend, None, id).await.unwrap();
    materialize::release_state(id);

    materialize::claim_state(id).expect("somebody else takes the fence");
    let refused = oc::restore(
        &store,
        &LocalBackend,
        &opencode_config(),
        id,
        &backup.path,
        true,
    )
    .await
    .expect_err("a claimed state must not be overwritten");
    assert!(matches!(refused, StateError::Claimed(_)), "{refused}");
    materialize::release_state(id);

    // Released, the same restore goes through.
    oc::restore(
        &store,
        &LocalBackend,
        &opencode_config(),
        id,
        &backup.path,
        true,
    )
    .await
    .expect("a released state restores");
    materialize::release_state(id);
}

/// A restore puts the database back where the next launch will find it, says
/// what it did not bring back, and leaves the state stageable again. And when
/// the workspace has moved on since the checkpoint, it says so and asks: the
/// database comes back, the files it describes do not.
#[tokio::test]
async fn a_restore_puts_the_database_back_and_says_what_it_does_not() {
    state::isolate();
    let id = "state-restore-roundtrip";
    materialize::release_state(id);
    let store = store_for(id, false);
    let live = seed_state(id, &GENERATION, &["ses_before"]);
    let workspace = seed_workspace(id);
    record_identity(&store, id, OpenCodeAdapter::PINNED_VERSION, &GENERATION);
    let backup = oc::backup(&store, &LocalBackend, None, id).await.unwrap();
    materialize::release_state(id);
    let at_backup = backup.manifest.workspace.commit.clone();
    assert!(
        at_backup.is_some(),
        "the workspace was checkpointed at the same instant: {:?}",
        backup.manifest.workspace
    );

    // The session goes on, and then the operator wants the earlier state.
    Connection::open(&live)
        .unwrap()
        .execute("DELETE FROM session", [])
        .unwrap();
    assert!(session_ids(&live).is_empty());

    let report = oc::restore(
        &store,
        &LocalBackend,
        &opencode_config(),
        id,
        &backup.path,
        false,
    )
    .await
    .expect("a backup of this node's own generation restores");
    materialize::release_state(id);

    assert_eq!(
        session_ids(&live),
        ["ses_before"],
        "the restored database is the one the next launch opens"
    );
    assert_eq!(oc::integrity_of(&live).unwrap(), "ok");
    assert!(report
        .preserves
        .iter()
        .any(|s| s.contains("harness database")));
    assert!(report.loses.iter().any(|s| s.contains("workspace")));
    assert!(report.loses.iter().any(|s| s.contains("side effect")));

    // And the state can be staged for a relaunch: nothing is left holding it.
    let scratch = materialize::scratch_for(
        id,
        Path::new("/ignored"),
        Path::new("/ignored"),
        materialize::PODMAN_HARNESS_HOME,
        &OpenCodeAdapter::new(OpenCodeAdapter::PINNED_VERSION),
        &Default::default(),
        "# Orientation",
    )
    .expect("the restored session relaunches");
    assert!(scratch.dir.join(materialize::OPENCODE_RUN).is_dir());
    materialize::remove(id);

    // Now the workspace moves on, and the same restore stops to ask.
    commit_in(&workspace, "later.txt");
    let refused = oc::restore(
        &store,
        &LocalBackend,
        &opencode_config(),
        id,
        &backup.path,
        false,
    )
    .await
    .expect_err("a workspace that has moved is the operator's call, not the node's");
    let said = refused.to_string();
    assert!(said.contains("--confirm"), "{said}");
    assert!(said.contains("not the workspace"), "{said}");
    materialize::release_state(id);

    let report = oc::restore(
        &store,
        &LocalBackend,
        &opencode_config(),
        id,
        &backup.path,
        true,
    )
    .await
    .expect("--confirm accepts it");
    assert!(report.workspace_moved);
    assert!(
        report
            .loses
            .iter()
            .any(|s| s.contains("every edit in between remains")),
        "{:?}",
        report.loses
    );
    materialize::release_state(id);
}

/// A workspace in runtime-owned storage, so the checkpoint has a real commit
/// to name rather than an absence to explain.
fn seed_workspace(id: &str) -> PathBuf {
    let root = tracon::runner::local::local_runtime_path(&tracon::workspace::volume_name(id));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    run_git(&root, &["init", "-q", "-b", "main"]);
    run_git(&root, &["config", "user.email", "tracon@localhost"]);
    run_git(&root, &["config", "user.name", "tracon"]);
    commit_in(&root, "first.txt");
    root
}

fn commit_in(root: &Path, name: &str) {
    std::fs::write(root.join(name), name).unwrap();
    run_git(root, &["add", "-A"]);
    run_git(root, &["commit", "-q", "-m", name]);
}

fn run_git(root: &Path, args: &[&str]) {
    let out = std::process::Command::new("git")
        .current_dir(root)
        .args(args)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .output()
        .expect("git runs");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// An altered backup is refused: the manifest names the bytes, and a copy that
/// no longer matches its own manifest is not the thing that was verified.
#[tokio::test]
async fn a_backup_that_no_longer_matches_its_manifest_is_refused() {
    state::isolate();
    let id = "state-restore-tampered";
    materialize::release_state(id);
    let store = store_for(id, false);
    seed_state(id, &GENERATION, &["ses_x"]);
    record_identity(&store, id, OpenCodeAdapter::PINNED_VERSION, &GENERATION);
    let backup = oc::backup(&store, &LocalBackend, None, id).await.unwrap();
    materialize::release_state(id);

    Connection::open(backup.path.join(oc::DB_FILE))
        .unwrap()
        .execute(
            "INSERT INTO session (id, title) VALUES ('ses_y','later')",
            [],
        )
        .unwrap();

    let refused = oc::restore(
        &store,
        &LocalBackend,
        &opencode_config(),
        id,
        &backup.path,
        true,
    )
    .await
    .expect_err("a backup that changed under the manifest is not a backup");
    assert!(
        refused.to_string().contains("altered or truncated"),
        "{refused}"
    );
    materialize::release_state(id);
}

// ---------------------------------------------------------------------------
// Upgrade on a clone
// ---------------------------------------------------------------------------

/// A target this host does not have is said out loud, and nothing happens.
#[tokio::test]
async fn an_absent_target_binary_is_a_no_op_with_a_message() {
    state::isolate();
    let id = "state-upgrade-absent";
    materialize::release_state(id);
    let store = store_for(id, false);
    let live = seed_state(id, &GENERATION, &["ses_here"]);
    record_identity(&store, id, OpenCodeAdapter::PINNED_VERSION, &GENERATION);
    oc::backup(&store, &LocalBackend, None, id).await.unwrap();
    materialize::release_state(id);
    let before = std::fs::read(&live).unwrap();

    let refused = oc::upgrade_state(&store, &LocalBackend, id, "0.0.1-nowhere-on-this-host")
        .await
        .expect_err("a build this host does not have cannot migrate anything");
    assert!(matches!(refused, StateError::NoBinary(_)), "{refused}");
    let said = refused.to_string();
    assert!(said.contains("nothing was changed"), "{said}");
    assert!(said.contains("TRACON_OPENCODE_BINARY"), "{said}");
    assert_eq!(std::fs::read(&live).unwrap(), before, "byte for byte");
    assert_eq!(
        store.opencode_state_get(id).unwrap().unwrap().build_version,
        OpenCodeAdapter::PINNED_VERSION,
        "the recorded identity did not move either"
    );
    materialize::release_state(id);
}

/// A migration with no backup of the generation it starts from is a migration
/// with no way back, so it is refused before a binary is even looked for.
#[tokio::test]
async fn an_upgrade_without_a_backup_of_the_current_generation_is_refused() {
    state::isolate();
    let id = "state-upgrade-unbacked";
    materialize::release_state(id);
    let store = store_for(id, false);
    seed_state(id, &GENERATION, &["ses_unbacked"]);
    record_identity(&store, id, OpenCodeAdapter::PINNED_VERSION, &GENERATION);

    let refused = oc::upgrade_state(&store, &LocalBackend, id, "9.9.9")
        .await
        .expect_err("no backup, no migration");
    assert!(matches!(refused, StateError::NoBackup(_)), "{refused}");
    assert!(refused.to_string().contains("tracon session backup"));
    materialize::release_state(id);
}

/// The one that decides whether "on a clone" means anything: a target binary
/// that ruins the database it is pointed at, and a session whose own state is
/// exactly as it was afterwards.
///
/// The stand-in reports the asked-for version, corrupts the database it is
/// given, and then serves health — so the run succeeds and the *integrity
/// check on the clone* is the only thing left standing between that corruption
/// and the session's real state.
#[tokio::test]
async fn an_upgrade_whose_clone_fails_integrity_leaves_the_original_untouched() {
    state::isolate();
    let id = "state-upgrade-corrupt";
    materialize::release_state(id);
    let store = store_for(id, false);
    let live = seed_state(id, &GENERATION, &["ses_survivor"]);
    record_identity(&store, id, OpenCodeAdapter::PINNED_VERSION, &GENERATION);
    oc::backup(&store, &LocalBackend, None, id).await.unwrap();
    materialize::release_state(id);
    let before = std::fs::read(&live).unwrap();

    let Some(_stand_in) = install_wrecker("9.9.9") else {
        eprintln!("skipped: no python3 to stand in for a target OpenCode build");
        return;
    };

    let refused = oc::upgrade_state(&store, &LocalBackend, id, "9.9.9")
        .await
        .expect_err("a clone that did not survive its own migration is not swapped in");
    let said = refused.to_string();
    assert!(said.contains("integrity check"), "{said}");
    assert!(said.contains("was not touched"), "{said}");

    assert_eq!(
        std::fs::read(&live).unwrap(),
        before,
        "the session's own database is byte for byte what it was"
    );
    assert_eq!(session_ids(&live), ["ses_survivor"]);
    assert_eq!(oc::integrity_of(&live).unwrap(), "ok");
    let identity = store.opencode_state_get(id).unwrap().unwrap();
    assert_eq!(identity.build_version, OpenCodeAdapter::PINNED_VERSION);
    assert_eq!(identity.generation, GENERATION);
    materialize::release_state(id);
}

/// A stand-in for a target OpenCode build, installed where `host_binary` looks
/// for a version this host does not otherwise have. It answers `--version`, and
/// on `serve` it wrecks the database at `OPENCODE_DB` and then answers health,
/// which is the sequence that makes the clone's integrity check the last line.
fn install_wrecker(version: &str) -> Option<PathBuf> {
    if std::process::Command::new("python3")
        .arg("--version")
        .output()
        .is_err()
    {
        return None;
    }
    let path = Config::state_dir()
        .join("opencode")
        .join(version)
        .join("opencode");
    std::fs::create_dir_all(path.parent().unwrap()).ok()?;
    std::fs::write(
        &path,
        format!(
            r#"#!/usr/bin/env python3
import os, sys, http.server, threading
if "--version" in sys.argv:
    print("{version}")
    raise SystemExit(0)
db = os.environ["OPENCODE_DB"]
with open(db, "r+b") as f:
    f.seek(4096)
    f.write(b"\xde\xad\xbe\xef" * 512)
port = int(sys.argv[sys.argv.index("--port") + 1])
class H(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        self.send_response(200)
        self.send_header("content-type", "application/json")
        self.end_headers()
        self.wfile.write(b'{{"version":"{version}"}}')
    def log_message(self, *a):
        pass
http.server.HTTPServer(("127.0.0.1", port), H).serve_forever()
"#
        ),
    )
    .ok()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).ok()?;
    }
    Some(path)
}

// ---------------------------------------------------------------------------
// The real binary
// ---------------------------------------------------------------------------

/// The pinned binary, when this machine has it. A build at another version
/// proves nothing about the release this was written against, so it is skipped
/// rather than run.
fn pinned_binary() -> Option<PathBuf> {
    let path = oc::host_binary(OpenCodeAdapter::PINNED_VERSION);
    if path.is_none() {
        eprintln!(
            "skipped: no OpenCode {} on this host. Put it on PATH as `opencode`, \
             or name it in TRACON_OPENCODE_BINARY.",
            OpenCodeAdapter::PINNED_VERSION
        );
    }
    path
}

/// Write one row into whatever shape the pinned release's `session` table has:
/// every column that is `NOT NULL` with no default gets a value of its declared
/// type. Reading the shape rather than writing it down keeps this honest across
/// a pin bump.
fn insert_upstream_session(db: &Path, id: &str) {
    let conn = Connection::open(db).unwrap();
    // The row stands in for harness data; what is being proved is that these
    // bytes survive a copy and a restore, not that upstream's own writer would
    // have produced them, so its references are not satisfied here.
    conn.pragma_update(None, "foreign_keys", false).unwrap();
    let mut stmt = conn.prepare("PRAGMA table_info(session)").unwrap();
    let columns: Vec<(String, String, i64, Option<String>)> = stmt
        .query_map([], |r| {
            Ok((
                r.get("name")?,
                r.get("type")?,
                r.get("notnull")?,
                r.get("dflt_value")?,
            ))
        })
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap();
    let mut names = Vec::new();
    let mut values: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
    for (name, kind, notnull, default) in columns {
        let required = notnull == 1 && default.is_none();
        if name != "id" && !required {
            continue;
        }
        let kind = kind.to_uppercase();
        let value: Box<dyn rusqlite::ToSql> = if name == "id" {
            Box::new(id.to_string())
        } else if kind.contains("INT") || kind.contains("REAL") || kind.contains("NUM") {
            Box::new(0i64)
        } else {
            Box::new(String::new())
        };
        names.push(name);
        values.push(value);
    }
    let placeholders = (1..=names.len())
        .map(|n| format!("?{n}"))
        .collect::<Vec<_>>()
        .join(",");
    let sql = format!(
        "INSERT INTO session ({}) VALUES ({placeholders})",
        names.join(",")
    );
    let refs: Vec<&dyn rusqlite::ToSql> = values.iter().map(|v| v.as_ref()).collect();
    conn.execute(&sql, refs.as_slice()).unwrap();
}

/// End to end against the real release: a real build makes a real generation,
/// the backup records exactly the migration ids that build applied, and the
/// restored database is one the same build opens and serves — with the session
/// row that existed before the backup still in it.
///
/// This is the case the synthetic ones cannot reach. Everything above asserts
/// that tracon's gates hold; this asserts that what they are gating is a file
/// OpenCode itself will still accept.
#[tokio::test]
async fn the_real_binary_makes_the_generation_and_reopens_the_restored_database() {
    state::isolate();
    let Some(binary) = pinned_binary() else {
        return;
    };
    let id = "state-real-roundtrip";
    materialize::release_state(id);
    let store = store_for(id, false);

    // Let the real build create the database, so the generation is upstream's
    // own ledger rather than one this test wrote.
    let db = volume_root(id).join(materialize::OPENCODE_DB);
    std::fs::create_dir_all(db.parent().unwrap()).unwrap();
    let home = state::scratch("state-real-roundtrip-home");
    oc::apply_migrations_with(&binary, &home, &db)
        .await
        .expect("the pinned build opens a fresh database and migrates it");
    let generation = oc::read_generation(&db).unwrap();
    assert!(
        generation.len() > 10,
        "the pinned release applies its own ledger: {generation:?}"
    );

    // A row in the real schema's own `session` table. Its required columns are
    // read off the table rather than spelled out here, so this stays true when
    // the pin moves and the release adds another one.
    insert_upstream_session(&db, "ses_realbackup");

    record_identity(&store, id, OpenCodeAdapter::PINNED_VERSION, &[]);
    let backup = oc::backup(&store, &LocalBackend, None, id)
        .await
        .expect("a stopped session copies");
    materialize::release_state(id);
    assert_eq!(backup.manifest.integrity, "ok");
    assert_eq!(
        backup.manifest.generation, generation,
        "the manifest names the generation the real build produced"
    );

    // Lose it, put it back.
    Connection::open(&db)
        .unwrap()
        .execute("DELETE FROM session WHERE id='ses_realbackup'", [])
        .unwrap();
    oc::restore(
        &store,
        &LocalBackend,
        &opencode_config(),
        id,
        &backup.path,
        true,
    )
    .await
    .expect("this node's own pin covers its own generation");
    materialize::release_state(id);

    // The real build opens the restored file, migrates nothing new, and the
    // row that existed before the backup is there.
    let home = state::scratch("state-real-roundtrip-home-2");
    oc::apply_migrations_with(&binary, &home, &db)
        .await
        .expect("the pinned build relaunches on the restored database");
    assert_eq!(oc::read_generation(&db).unwrap(), generation);
    assert!(session_ids(&db).iter().any(|s| s == "ses_realbackup"));
    materialize::release_state(id);
}
