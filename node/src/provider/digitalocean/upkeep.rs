//! What the node removes from DigitalOcean on its own, because the runtime
//! keeps it and bills for it:
//!
//! - **Orphaned sessions**, at start: the node's sandbox identity is made
//!   per process, so a sandbox the previous process started can never
//!   connect to this one, and a kept-warm session would run until removed.
//! - **Leftover configs**: each create stores one that removing its session
//!   leaves behind (`do_harness::cleanup`).
//! - **Checkpoints**: one per prepared environment is kept to fork from, the
//!   newest ready; every older one (51 GB each, as recorded) goes.
//! - **Templates** for images the node no longer uses, once no session of
//!   its runs.
//!
//! Only names this node owns (`Owner::owns`, or its template stem) are ever
//! touched.

use serde::Serialize;

use super::templates::Templates;
use super::Owner;
use crate::do_harness::cleanup;
use crate::do_harness::{Client, DoError, Session, SessionFilter};

/// What one pass removed, for the log.
#[derive(Debug, Default, Serialize)]
pub struct Swept {
    pub sessions: Vec<String>,
    pub configs: Vec<String>,
    pub checkpoints: Vec<String>,
    pub freed_bytes: u64,
    pub templates: Vec<String>,
    pub failed: Vec<String>,
}

/// The environment a session's checkpoints belong to: its name without the
/// per-run parts the node adds (the process id of a capture), so the
/// sandboxes one preparation makes share one.
pub fn environment_of(owner: &Owner, session: &Session) -> Option<String> {
    owner.owns(&session.name).then(|| {
        let stem = session
            .name
            .strip_suffix(&format!("-n{}", owner.node))
            .unwrap_or(&session.name);
        match stem.rsplit_once('-') {
            Some((rest, pid)) if !pid.is_empty() && pid.bytes().all(|b| b.is_ascii_digit()) => {
                rest.to_string()
            }
            _ => stem.to_string(),
        }
    })
}

/// Remove every session of this node's that is still there: called once, at
/// start, before any sandbox of this process exists.
pub async fn remove_orphans(client: &Client, owner: &Owner) -> Result<Swept, DoError> {
    let mut swept = Swept::default();
    for session in client.list_sessions(&SessionFilter::default()).await? {
        if !owner.owns(&session.name) || session.status.is_terminal() {
            continue;
        }
        match client.delete_session(&session.session_id).await {
            Ok(()) => swept.sessions.push(session.name.clone()),
            Err(error) => swept
                .failed
                .push(format!("session {}: {error}", session.name)),
        }
        if let Some(config) = &session.config_id {
            match client.delete_config(config).await {
                Ok(()) => swept.configs.push(config.clone()),
                Err(error) => swept.failed.push(format!("config {config}: {error}")),
            }
        }
    }
    Ok(swept)
}

/// One pass of the periodic upkeep.
pub async fn sweep(
    client: &Client,
    owner: &Owner,
    templates: &Templates,
    now_ms: i64,
) -> Result<Swept, DoError> {
    let mut swept = Swept::default();
    let configs = cleanup::delete_leftover_configs(client, |c| owner.owns(&c.name), now_ms).await?;
    swept
        .configs
        .extend(configs.deleted.into_iter().map(|c| c.name));
    swept.failed.extend(
        configs
            .failed
            .into_iter()
            .map(|(id, e)| format!("config {id}: {e}")),
    );

    let sessions = client.list_sessions(&SessionFilter::default()).await?;
    let checkpoints =
        cleanup::prune_checkpoints(client, &sessions, |s| environment_of(owner, s)).await?;
    swept.freed_bytes = checkpoints.freed_bytes;
    swept
        .checkpoints
        .extend(checkpoints.deleted.into_iter().map(|c| c.checkpoint_id));
    swept.failed.extend(
        checkpoints
            .failed
            .into_iter()
            .map(|(id, e)| format!("checkpoint {id}: {e}")),
    );

    // Whether a running session breaks when the template it started from
    // goes is not known, so stale ones wait for a moment none runs.
    let running = sessions
        .iter()
        .any(|s| owner.owns(&s.name) && !s.status.is_terminal());
    if !running {
        for template in client.list_templates().await? {
            if !templates.is_stale(&template.name) {
                continue;
            }
            match client.delete_template(&template.template_id).await {
                Ok(()) => swept.templates.push(template.name),
                Err(error) => swept
                    .failed
                    .push(format!("template {}: {error}", template.name)),
            }
        }
    }
    Ok(swept)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_preparations_sandboxes_share_one_environment() {
        let owner = Owner::new("tracon", "abcd1234");
        let named = |name: &str| Session {
            name: name.into(),
            ..Session::default()
        };
        assert_eq!(
            environment_of(&owner, &named("tracon-prep-0f3a-4242-nabcd1234")).as_deref(),
            Some("tracon-prep-0f3a")
        );
        assert_eq!(
            environment_of(&owner, &named("tracon-prep-0f3a-nabcd1234")).as_deref(),
            Some("tracon-prep-0f3a")
        );
        assert_eq!(
            environment_of(&owner, &named("tracon-h-swift-otter-nabcd1234")).as_deref(),
            Some("tracon-h-swift-otter")
        );
        assert_eq!(environment_of(&owner, &named("tracon-h-x-nffff0000")), None);
    }
}
