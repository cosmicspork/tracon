//! Plugins: an executable, optionally with a static UI, released on GitHub
//! in a fixed shape (`manifest`), which the node installs on the operator's
//! approval and keeps under its state directory (`install`).
//!
//! Installing is two steps. `offer` stages a checked build and raises an
//! install card naming it by digest; nothing is active until the operator
//! allows that card (`decide`), which makes the build the plugin's active
//! one and keeps the one before it for `rollback`. An update is the same
//! card, showing what changed and the release's notes, while the active
//! build keeps serving. `supervisor` runs the active build, one process per
//! plugin and channel, and stops a plugin's processes when it is updated,
//! rolled back or removed.

pub mod context;
pub mod host;
pub mod install;
pub mod lending;
pub mod manifest;
pub mod supervisor;

use serde_json::{json, Value};

use crate::store::approvals::{ApprovalRow, PENDING as APPROVAL_PENDING, PLUGIN_INSTALL_TOOL};
use crate::store::plugins::{Build, Pending, PluginRow, REMOVED};
use crate::store::Store;
use install::{InstallError, Staged};

/// Whether a channel that may use the credentials `available` is offered
/// the plugin's tools: it binds one the manifest declares, by name or
/// pattern, or the plugin declares none.
pub fn offered(manifest: &manifest::Manifest, available: &[&str]) -> bool {
    manifest.credentials.is_empty()
        || available
            .iter()
            .any(|binding| manifest.credentials.iter().any(|c| c.matches(binding)))
}

/// What the plugin is called to a person or a session: its `[ui]` title,
/// the one name a manifest gives besides `name`, when it has one.
pub fn display_name(manifest: &manifest::Manifest) -> Option<&str> {
    manifest
        .ui
        .as_ref()
        .map(|ui| ui.title.trim())
        .filter(|title| !title.is_empty())
}

/// The longest release body an install card carries.
const CHANGELOG_CHARS: usize = 8_000;

/// Held across reading what is installed and writing what is pending, so
/// two offers for one plugin do not interleave.
static OFFERING: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn offering() -> std::sync::MutexGuard<'static, ()> {
    OFFERING.lock().unwrap_or_else(|p| p.into_inner())
}

/// Where an install card goes, and how long it waits.
pub struct Ask<'a> {
    pub node_id: &'a str,
    pub channel: &'a str,
    pub expiry_secs: u64,
    /// The `[[mcp_server]]` names, whose tools share the `<name>__` space.
    pub mcp_servers: Vec<String>,
}

/// What offering a build came to.
#[derive(Debug)]
pub enum Offered {
    /// A card waits on the operator: new, or the one already raised for
    /// this same build. `superseded` is the card of a build that waited
    /// before it, now expired.
    Asked {
        card: ApprovalRow,
        new: bool,
        superseded: Option<String>,
    },
    /// This build is the active one already.
    UpToDate(Build),
}

fn is_local(repo: &str) -> bool {
    repo.starts_with("local:")
}

/// The build's plugin, for the same source as `row`, or a refusal naming
/// the source it is installed from.
fn same_source(row: &PluginRow, source: &str) -> Result<(), InstallError> {
    if row.state == REMOVED || row.repo == source || (is_local(&row.repo) && is_local(source)) {
        return Ok(());
    }
    Err(InstallError::Refused(format!(
        "plugin {} is installed from {}; `tracon plugin rm {}` before adding it from {source}",
        row.name, row.repo, row.name
    )))
}

/// A format `manifest` declares that another plugin owns, as a refusal.
fn format_conflict(
    store: &Store,
    manifest: &manifest::Manifest,
) -> Result<Option<String>, InstallError> {
    Ok(store
        .plugin_formats_owned()?
        .into_iter()
        .find(|(format, owner)| *owner != manifest.name && manifest.formats.contains(format))
        .map(|(format, owner)| format!("format {format} is owned by plugin {owner}")))
}

fn request_key(name: &str, digest: &str) -> String {
    crate::corpus::hash_body(&format!("{PLUGIN_INSTALL_TOOL}\u{1f}{name}\u{1f}{digest}"))
}

/// What the card says, in Markdown.
fn card_summary(staged: &Staged, from: Option<&Build>) -> String {
    let build = &staged.build;
    let manifest = &build.manifest;
    let mut out = match from {
        Some(was) => format!(
            "Update plugin **{}** {} → {}.\n\n",
            manifest.name, was.version, build.version
        ),
        None => format!(
            "Install plugin **{}** {}.\n\n",
            manifest.name, build.version
        ),
    };
    out.push_str(&manifest.summary());
    out.push_str("\n**Source**\n\n");
    match &staged.tag {
        Some(tag) => out.push_str(&format!(
            "- {} release {tag}, sha256 `{}` from its checksums.txt\n",
            staged.source, build.digest
        )),
        None => out.push_str(&format!(
            "- local archive {}, sha256 `{}`; no release checksum\n",
            staged.source.trim_start_matches("local:"),
            build.digest
        )),
    }
    if let Some(was) = from {
        out.push_str("\n**Changes**\n\n");
        let lines = manifest::diff(&was.manifest, manifest);
        if lines.is_empty() {
            out.push_str("- the manifest is unchanged\n");
        }
        for line in lines {
            out.push_str(&format!("- {line}\n"));
        }
        if let Some(changelog) = &staged.changelog {
            let mut shown: String = changelog.chars().take(CHANGELOG_CHARS).collect();
            if shown.len() < changelog.len() {
                shown.push_str("\n\n…");
            }
            out.push_str(&format!("\n**Changelog**\n\n{shown}\n"));
        }
    }
    out
}

/// Drop a staged build that will not be offered, unless the plugin of its
/// name holds it already.
pub fn discard(
    store: &Store,
    state_dir: &std::path::Path,
    mut staged: Staged,
) -> Result<(), InstallError> {
    let _offering = offering();
    drop(staged.hold.take());
    let name = &staged.build.manifest.name;
    let row = store.plugin_get(name)?;
    let keep: Vec<&Build> = row
        .iter()
        .flat_map(|r| {
            r.active
                .iter()
                .chain(r.previous.iter())
                .chain(r.pending.iter().map(|p| &p.build))
        })
        .collect();
    install::prune(state_dir, name, &keep)?;
    Ok(())
}

/// Put a staged build to the operator: raise its install card, or return
/// the one already waiting for it, and hold it as the plugin's pending
/// build. A build waiting before it is superseded, its card expired.
pub fn offer(
    store: &Store,
    state_dir: &std::path::Path,
    ask: &Ask<'_>,
    mut staged: Staged,
) -> Result<Offered, InstallError> {
    let _offering = offering();
    // From here the lock keeps other offers off it, and whatever this one
    // decides to keep or prune applies to it too.
    drop(staged.hold.take());
    let name = staged.build.manifest.name.clone();
    let row = store.plugin_get(&name)?;
    let keep = |row: Option<&PluginRow>, extra: Option<&Build>| -> std::io::Result<()> {
        let mut keep: Vec<&Build> = extra.into_iter().collect();
        if let Some(row) = row {
            keep.extend(row.active.iter().chain(row.previous.iter()));
            keep.extend(row.pending.iter().map(|p| &p.build));
        }
        install::prune(state_dir, &name, &keep)
    };
    // Whatever is refused below leaves nothing of this build behind that
    // the plugin does not already hold.
    let refuse = |why: String| -> Result<Offered, InstallError> {
        keep(row.as_ref(), None)?;
        Err(InstallError::Refused(why))
    };
    if ask.mcp_servers.contains(&name) {
        return refuse(format!(
            "plugin {name} has the name of an [[mcp_server]]; their tools would share the {name}__ prefix"
        ));
    }
    if let Some(row) = &row {
        if let Err(error) = same_source(row, &staged.source) {
            return refuse(error.to_string());
        }
        if let Some(active) = &row.active {
            if active.digest == staged.build.digest && row.pending.is_none() {
                return Ok(Offered::UpToDate(active.clone()));
            }
        }
        if let Some(pending) = &row.pending {
            if pending.build.digest == staged.build.digest {
                if let Some(card) = store.get_approval(&pending.approval_id)? {
                    if card.state == APPROVAL_PENDING && card.expires_ms > crate::store::now_ms() {
                        return Ok(Offered::Asked {
                            card,
                            new: false,
                            superseded: None,
                        });
                    }
                }
            }
        }
    }
    if let Some(conflict) = format_conflict(store, &staged.build.manifest)? {
        return refuse(conflict);
    }
    let from = row.as_ref().and_then(|r| r.active.as_ref());
    let summary = card_summary(&staged, from);
    let build = &staged.build;
    let arguments = json!({
        "name": name,
        "repo": staged.source,
        "version": build.version,
        "digest": build.digest,
        "from_version": from.map(|b| b.version.clone()),
        "summary": summary,
    });
    let title = match from {
        Some(was) => format!(
            "update plugin {name} {} → {} from {}",
            was.version, build.version, staged.source
        ),
        None => format!(
            "install plugin {name} {} from {}",
            build.version, staged.source
        ),
    };
    let now = crate::store::now_ms();
    let card = ApprovalRow {
        id: uuid::Uuid::now_v7().to_string(),
        channel: ask.channel.to_string(),
        session_id: None,
        node_id: ask.node_id.to_string(),
        lane: None,
        tool: PLUGIN_INSTALL_TOOL.to_string(),
        arguments: arguments.to_string(),
        request_key: request_key(&name, &build.digest),
        title,
        state: APPROVAL_PENDING.into(),
        answer_option_id: None,
        edited_arguments: None,
        result: None,
        reason: None,
        operator_note: None,
        claimed_ms: None,
        created_ms: now,
        decided_ms: None,
        finished_ms: None,
        expires_ms: now + (ask.expiry_secs.max(1) as i64) * 1000,
    };
    let mut superseded = None;
    if let Some(older) = row.as_ref().and_then(|r| r.pending.as_ref()) {
        if store.expire_approval(
            &older.approval_id,
            &format!("superseded by {name} {}", build.version),
        )? {
            superseded = Some(older.approval_id.clone());
        }
    }
    store.insert_approval(&card)?;
    store.plugin_stage_pending(
        &name,
        &staged.source,
        &Pending {
            build: staged.build.clone(),
            approval_id: card.id.clone(),
            tag: staged.tag.clone(),
            changelog: staged.changelog.clone(),
        },
    )?;
    keep(store.plugin_get(&name)?.as_ref(), None)?;
    Ok(Offered::Asked {
        card,
        new: true,
        superseded,
    })
}

/// The plugin an install card names.
pub fn card_plugin(approval: &ApprovalRow) -> Result<String, String> {
    let args: Value = serde_json::from_str(&approval.arguments).map_err(|e| e.to_string())?;
    args["name"]
        .as_str()
        .map(str::to_string)
        .ok_or_else(|| "this card names no plugin".into())
}

/// The operator's answer to an install card. Allowing it makes the build
/// it names the active one, once the build is still the one pending, still
/// what was checked on disk, and owns no other plugin's format; the result
/// is what the card records. Refusing it drops the pending build. Either
/// way, builds nothing refers to any more are removed.
pub fn decide(
    store: &Store,
    state_dir: &std::path::Path,
    approval: &ApprovalRow,
    allow: bool,
) -> Result<Value, String> {
    let _offering = offering();
    let name = card_plugin(approval)?;
    let args: Value = serde_json::from_str(&approval.arguments).map_err(|e| e.to_string())?;
    let prune = || -> Result<(), String> {
        let row = store.plugin_get(&name).map_err(|e| e.to_string())?;
        let keep: Vec<&Build> = row
            .iter()
            .flat_map(|r| {
                r.active
                    .iter()
                    .chain(r.previous.iter())
                    .chain(r.pending.iter().map(|p| &p.build))
            })
            .collect();
        install::prune(state_dir, &name, &keep).map_err(|e| e.to_string())
    };
    let row = store.plugin_get(&name).map_err(|e| e.to_string())?;
    let pending = row
        .as_ref()
        .and_then(|r| r.pending.clone())
        .filter(|p| p.approval_id == approval.id && args["digest"] == p.build.digest);
    let Some(pending) = pending else {
        return Err(format!(
            "{name}'s build on this card is no longer the one waiting; add it again"
        ));
    };
    let drop_pending = || -> Result<(), String> {
        store
            .plugin_clear_pending(&name, &approval.id)
            .map_err(|e| e.to_string())?;
        prune()
    };
    if !allow {
        drop_pending()?;
        return Ok(json!({ "rejected": pending.build.version }));
    }
    let checked = format_conflict(store, &pending.build.manifest)
        .map_err(|e| e.to_string())?
        .map_or_else(|| install::verify(state_dir, &name, &pending.build), Err);
    if let Err(why) = checked {
        drop_pending()?;
        return Err(format!(
            "{name} {} was not installed: {why}",
            pending.build.version
        ));
    }
    let was = row.and_then(|r| r.active).map(|b| b.version);
    let installed = store
        .plugin_activate_pending(&name, &approval.id)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("{name}'s pending build moved while it was installed"))?;
    prune()?;
    tracing::info!(plugin = %name, version = %installed.version, digest = %installed.digest, "installed a plugin");
    Ok(json!({ "installed": installed.version, "previous": was }))
}

/// Go back to the build before the active one, which the operator already
/// approved: no card. `(now active, was active)`.
pub fn rollback(
    store: &Store,
    state_dir: &std::path::Path,
    name: &str,
) -> Result<(Build, Build), InstallError> {
    let _offering = offering();
    let row = store
        .plugin_get(name)?
        .filter(|r| r.state != REMOVED)
        .ok_or_else(|| InstallError::Refused(format!("no plugin {name} is installed")))?;
    let previous = row.previous.as_ref().ok_or_else(|| {
        InstallError::Refused(format!("{name} has no previous version to go back to"))
    })?;
    install::verify(state_dir, name, previous).map_err(InstallError::Refused)?;
    let (now, was) = store.plugin_rollback(name)?.ok_or_else(|| {
        InstallError::Refused(format!("{name} has no previous version to go back to"))
    })?;
    tracing::info!(plugin = %name, from = %was.version, to = %now.version, "rolled a plugin back");
    Ok((now, was))
}

/// Remove `name`: its builds go, and its card if one waits (whose id is
/// returned, expired); its data stays unless `purge`. Its running
/// instances are the caller's to stop (`supervisor::Supervisor::stop_plugin`).
pub fn remove(
    store: &Store,
    state_dir: &std::path::Path,
    name: &str,
    purge: bool,
) -> Result<Option<String>, InstallError> {
    let _offering = offering();
    let row = store
        .plugin_get(name)?
        .filter(|r| purge || r.state != REMOVED)
        .ok_or_else(|| InstallError::Refused(format!("no plugin {name} is installed")))?;
    let mut expired = None;
    if let Some(pending) = &row.pending {
        if store.expire_approval(&pending.approval_id, &format!("plugin {name} was removed"))? {
            expired = Some(pending.approval_id.clone());
        }
    }
    store.plugin_remove(name, purge)?;
    install::prune(state_dir, name, &[])?;
    if purge {
        match std::fs::remove_dir_all(install::data_dir(state_dir, name)) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e.into()),
            _ => {}
        }
    }
    tracing::info!(plugin = %name, purge, "removed a plugin");
    Ok(expired)
}

#[cfg(test)]
mod tests {
    use super::install::tests::{archive, asset, manifest, owners, release, serve, Published};
    use super::*;
    use crate::store::approvals::{EXPIRED, PENDING};
    use std::sync::{Arc, Mutex};

    fn ask() -> Ask<'static> {
        Ask {
            node_id: "n1",
            channel: "personal",
            expiry_secs: 600,
            mcp_servers: vec!["linear".into()],
        }
    }

    fn local(state: &std::path::Path, name: &str, version: &str, formats: &[&str]) -> Staged {
        let bytes = archive(&manifest(name, version, formats), &[]);
        install::stage_local(state, &bytes, &asset(name, version)).unwrap()
    }

    fn asked(offered: Offered) -> ApprovalRow {
        match offered {
            Offered::Asked { card, .. } => card,
            other => panic!("{other:?}"),
        }
    }

    #[tokio::test]
    async fn a_release_is_asked_then_installed_and_updated_with_its_changes_shown() {
        if install::target().is_none() {
            return;
        }
        let state = tempfile::tempdir().unwrap();
        let store = Store::open_in_memory().unwrap();
        let v1 = archive(&manifest("sql-review", "0.1.0", &[]), &[]);
        let published = Arc::new(Mutex::new(Published::from([(
            "cosmicspork/sql-review".to_string(),
            (
                "v0.1.0".into(),
                "First.".into(),
                release("sql-review", "0.1.0", v1),
            ),
        )])));
        let (api, _) = serve(published.clone()).await;
        let releases = install::Releases::at(&api).unwrap();
        let owners = owners();
        let fetch = || install::fetch(&releases, state.path(), &owners, "cosmicspork/sql-review");

        let card = asked(offer(&store, state.path(), &ask(), fetch().await.unwrap()).unwrap());
        assert_eq!(card.tool, PLUGIN_INSTALL_TOOL);
        assert_eq!(card.session_id, None);
        assert_eq!(
            card.title,
            "install plugin sql-review 0.1.0 from cosmicspork/sql-review"
        );
        let args: Value = serde_json::from_str(&card.arguments).unwrap();
        let row = store.plugin_get("sql-review").unwrap().unwrap();
        assert_eq!(row.state, "pending");
        assert!(row.active.is_none(), "nothing is active before approval");
        let pending = row.pending.unwrap();
        assert_eq!(args["digest"], pending.build.digest);
        assert_eq!(pending.build.digest.len(), 64);
        assert!(args["summary"]
            .as_str()
            .unwrap()
            .contains("release v0.1.0, sha256"));
        // Asked again for the same build, the same card.
        let again = offer(&store, state.path(), &ask(), fetch().await.unwrap()).unwrap();
        assert!(
            matches!(again, Offered::Asked { ref card, new: false, .. } if card.id == pending.approval_id)
        );

        let result = decide(&store, state.path(), &card, true).unwrap();
        assert_eq!(result["installed"], "0.1.0");
        let row = store.plugin_get("sql-review").unwrap().unwrap();
        assert_eq!(row.state, "active");
        let v1_build = row.active.clone().unwrap();
        // The card is used once.
        assert!(decide(&store, state.path(), &card, true).is_err());
        assert!(matches!(
            offer(&store, state.path(), &ask(), fetch().await.unwrap()).unwrap(),
            Offered::UpToDate(_)
        ));

        // v2 adds a tool and lends a credential only on approval.
        let v2_text = manifest("sql-review", "0.2.0", &[])
            .replace(
                "pattern = \"sql-read-*\"",
                "pattern = \"sql-read-*\"\nlend = \"approved\"",
            )
            .replace(
                "[ui]",
                "[[tool]]\nname = \"explain\"\nhint = \"read\"\n\n[ui]",
            );
        published.lock().unwrap().insert(
            "cosmicspork/sql-review".into(),
            (
                "v0.2.0".into(),
                "Adds explain.".into(),
                release("sql-review", "0.2.0", archive(&v2_text, &[])),
            ),
        );
        let card2 = asked(offer(&store, state.path(), &ask(), fetch().await.unwrap()).unwrap());
        assert_eq!(
            card2.title,
            "update plugin sql-review 0.1.0 → 0.2.0 from cosmicspork/sql-review"
        );
        let summary = serde_json::from_str::<Value>(&card2.arguments).unwrap()["summary"]
            .as_str()
            .unwrap()
            .to_string();
        for says in [
            "**Changes**",
            "tool `explain` added (read)",
            "credential `sql-read`: lent always → approved",
            "**Changelog**\n\nAdds explain.",
        ] {
            assert!(summary.contains(says), "{says}: {summary}");
        }
        // v1 keeps serving until the update is allowed.
        let row = store.plugin_get("sql-review").unwrap().unwrap();
        assert_eq!(row.active.as_ref().unwrap().version, "0.1.0");
        decide(&store, state.path(), &card2, true).unwrap();
        let row = store.plugin_get("sql-review").unwrap().unwrap();
        assert_eq!(row.active.as_ref().unwrap().version, "0.2.0");
        assert_eq!(row.previous.as_ref().unwrap().version, "0.1.0");

        // Rollback swaps them, keeping both builds on disk.
        let (now, was) = rollback(&store, state.path(), "sql-review").unwrap();
        assert_eq!(
            (now.version.as_str(), was.version.as_str()),
            ("0.1.0", "0.2.0")
        );
        assert_eq!(now, v1_build);
        let dirs = std::fs::read_dir(state.path().join("plugins/bin/sql-review"))
            .unwrap()
            .count();
        assert_eq!(dirs, 2);
    }

    #[test]
    fn a_build_staged_before_another_is_offered_is_not_pruned_by_that_offer() {
        if install::target().is_none() {
            return;
        }
        let state = tempfile::tempdir().unwrap();
        let store = Store::open_in_memory().unwrap();
        // Two adds of one plugin, both staged before either is offered.
        let one = local(state.path(), "a", "1.0.0", &[]);
        let two = local(state.path(), "a", "1.0.1", &[]);
        let two_dir = two.dir.clone();
        let first = asked(offer(&store, state.path(), &ask(), one).unwrap());
        assert!(
            two_dir.exists(),
            "the first offer pruned the second's build"
        );
        let second = asked(offer(&store, state.path(), &ask(), two).unwrap());
        let first = store.get_approval(&first.id).unwrap().unwrap();
        assert_eq!(first.state, EXPIRED);
        decide(&store, state.path(), &second, true).unwrap();
        let active = store.plugin_get("a").unwrap().unwrap().active.unwrap();
        assert_eq!(active.version, "1.0.1");
        // Offered, a build is pruned as any other is once nothing holds it.
        let dirs: Vec<_> = std::fs::read_dir(state.path().join("plugins/bin/a"))
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(dirs.len(), 1, "{dirs:?}");
        assert!(dirs[0].starts_with("1.0.1-"), "{dirs:?}");
    }

    #[test]
    fn a_staged_build_that_is_not_offered_is_discarded() {
        if install::target().is_none() {
            return;
        }
        let state = tempfile::tempdir().unwrap();
        let store = Store::open_in_memory().unwrap();
        let staged = local(state.path(), "a", "1.0.0", &[]);
        let dir = staged.dir.clone();
        // Pruning leaves it while it is staged.
        install::prune(state.path(), "a", &[]).unwrap();
        assert!(dir.exists());
        discard(&store, state.path(), staged).unwrap();
        assert!(!dir.exists());
        assert!(!state.path().join("plugins/bin/a").exists());
    }

    #[test]
    fn a_newer_build_supersedes_the_card_and_a_rejection_leaves_nothing() {
        if install::target().is_none() {
            return;
        }
        let state = tempfile::tempdir().unwrap();
        let store = Store::open_in_memory().unwrap();
        let first = asked(
            offer(
                &store,
                state.path(),
                &ask(),
                local(state.path(), "a", "1.0.0", &[]),
            )
            .unwrap(),
        );
        let second = asked(
            offer(
                &store,
                state.path(),
                &ask(),
                local(state.path(), "a", "1.0.1", &[]),
            )
            .unwrap(),
        );
        let first = store.get_approval(&first.id).unwrap().unwrap();
        assert_eq!(first.state, EXPIRED);
        assert_eq!(first.reason.as_deref(), Some("superseded by a 1.0.1"));
        assert!(decide(&store, state.path(), &first, true).is_err());
        // Only the waiting build is kept.
        let dirs: Vec<_> = std::fs::read_dir(state.path().join("plugins/bin/a"))
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(dirs.len(), 1);
        assert!(dirs[0].starts_with("1.0.1-"), "{dirs:?}");

        assert_eq!(
            store.get_approval(&second.id).unwrap().unwrap().state,
            PENDING
        );
        decide(&store, state.path(), &second, false).unwrap();
        assert!(store.plugin_get("a").unwrap().is_none());
        assert!(!state.path().join("plugins/bin/a").exists());
    }

    #[test]
    fn a_format_another_plugin_owns_is_refused_naming_it() {
        if install::target().is_none() {
            return;
        }
        let state = tempfile::tempdir().unwrap();
        let store = Store::open_in_memory().unwrap();
        asked(
            offer(
                &store,
                state.path(),
                &ask(),
                local(state.path(), "jira-review", "1.0.0", &["jira_wiki"]),
            )
            .unwrap(),
        );
        let refused = offer(
            &store,
            state.path(),
            &ask(),
            local(state.path(), "b", "1.0.0", &["jira_wiki"]),
        )
        .unwrap_err();
        assert_eq!(
            refused.to_string(),
            "format jira_wiki is owned by plugin jira-review"
        );
        assert!(!state.path().join("plugins/bin/b").exists());
        // A plugin updating itself keeps its own formats.
        asked(
            offer(
                &store,
                state.path(),
                &ask(),
                local(state.path(), "jira-review", "1.1.0", &["jira_wiki"]),
            )
            .unwrap(),
        );
        // And an MCP server's name is not a plugin's.
        let refused = offer(
            &store,
            state.path(),
            &ask(),
            local(state.path(), "linear", "1.0.0", &[]),
        )
        .unwrap_err();
        assert!(refused.to_string().contains("[[mcp_server]]"), "{refused}");
    }

    #[test]
    fn a_plugin_is_offered_to_a_channel_binding_a_credential_it_declares() {
        let m = manifest::Manifest::parse(manifest::tests::EXAMPLE).unwrap();
        assert!(offered(&m, &["github", "sql-read"]));
        assert!(offered(&m, &["sql-write-prod"]));
        assert!(!offered(&m, &["github", "sql-readx"]));
        assert!(!offered(&m, &[]));
        let mut open = m.clone();
        open.credentials.clear();
        assert!(offered(&open, &[]));
    }

    #[test]
    fn a_tampered_build_is_not_installed() {
        if install::target().is_none() {
            return;
        }
        let state = tempfile::tempdir().unwrap();
        let store = Store::open_in_memory().unwrap();
        let staged = local(state.path(), "a", "1.0.0", &[]);
        let dir = staged.dir.clone();
        let card = asked(offer(&store, state.path(), &ask(), staged).unwrap());
        std::fs::write(dir.join("bin/a"), b"#!/bin/sh\nevil\n").unwrap();
        let refused = decide(&store, state.path(), &card, true).unwrap_err();
        assert!(refused.contains("changed on disk"), "{refused}");
        assert!(store.plugin_get("a").unwrap().is_none());
        assert!(!dir.exists());
    }

    #[test]
    fn removal_keeps_data_unless_purged_and_rollback_needs_a_previous() {
        if install::target().is_none() {
            return;
        }
        let state = tempfile::tempdir().unwrap();
        let store = Store::open_in_memory().unwrap();
        let card = asked(
            offer(
                &store,
                state.path(),
                &ask(),
                local(state.path(), "a", "1.0.0", &[]),
            )
            .unwrap(),
        );
        decide(&store, state.path(), &card, true).unwrap();
        let refused = rollback(&store, state.path(), "a").unwrap_err();
        assert!(
            refused.to_string().contains("no previous version"),
            "{refused}"
        );
        let data = install::data_dir(state.path(), "a").join("personal");
        std::fs::create_dir_all(&data).unwrap();
        std::fs::write(data.join("db"), b"x").unwrap();

        // An update waiting when it is removed has its card expired.
        let card = asked(
            offer(
                &store,
                state.path(),
                &ask(),
                local(state.path(), "a", "1.1.0", &[]),
            )
            .unwrap(),
        );
        remove(&store, state.path(), "a", false).unwrap();
        assert_eq!(
            store.get_approval(&card.id).unwrap().unwrap().state,
            EXPIRED
        );
        assert_eq!(store.plugin_get("a").unwrap().unwrap().state, REMOVED);
        assert!(!state.path().join("plugins/bin/a").exists());
        assert!(data.join("db").exists());
        assert!(remove(&store, state.path(), "a", false).is_err());

        // Added again, it is a first install.
        let card = asked(
            offer(
                &store,
                state.path(),
                &ask(),
                local(state.path(), "a", "1.0.0", &[]),
            )
            .unwrap(),
        );
        assert_eq!(
            card.title,
            format!("install plugin a 1.0.0 from local:{}", asset("a", "1.0.0"))
        );
        remove(&store, state.path(), "a", true).unwrap();
        assert!(store.plugin_get("a").unwrap().is_none());
        assert!(!install::data_dir(state.path(), "a").exists());
    }

    #[test]
    fn a_plugin_from_one_source_is_not_replaced_from_another() {
        if install::target().is_none() {
            return;
        }
        let state = tempfile::tempdir().unwrap();
        let store = Store::open_in_memory().unwrap();
        let mut staged = local(state.path(), "a", "1.0.0", &[]);
        staged.source = "cosmicspork/a".into();
        let card = asked(offer(&store, state.path(), &ask(), staged).unwrap());
        decide(&store, state.path(), &card, true).unwrap();
        let refused = offer(
            &store,
            state.path(),
            &ask(),
            local(state.path(), "a", "1.1.0", &[]),
        )
        .unwrap_err();
        assert!(
            refused
                .to_string()
                .contains("is installed from cosmicspork/a"),
            "{refused}"
        );
        // The refused build is not left behind.
        let dirs = std::fs::read_dir(state.path().join("plugins/bin/a"))
            .unwrap()
            .count();
        assert_eq!(dirs, 1);
    }
}
