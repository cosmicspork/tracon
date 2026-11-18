//! A session whose subscription sign-in ran out waits for a new one.
//!
//! A model call the provider refuses for its credential (`cause = auth`)
//! while the sign-in that made it can no longer be renewed — `needs_reconnect`,
//! or past its expiry — will be refused again however often the harness
//! retries: the turn would end as if the model had answered, and the session
//! would sit idle. So the node fences the session instead, `paused` and
//! waiting for a sign-in, and wakes it once a new copy of the sign-in is here:
//! the operator signed in again on this node, or another node's sign-in was
//! handed off to it. The session then carries on from the call that failed.
//!
//! A refusal while the sign-in still looks renewable is remembered for a
//! while: if its renewal then fails, the session waits as well. Waiting ends
//! after `[session] auth_wait_secs` with the session ended and saying why.
//!
//! Kept in memory: a session does not outlive the process.

use std::collections::HashMap;

use serde_json::{json, Value};
use tokio::sync::oneshot;

use super::{
    state::{event_kind as ek, EndReason, SessionState},
    supervisor::{Command, PauseSource},
    Manager,
};
use crate::{broker::KIND_OAUTH, oauth::Flow, store::now_ms, store::SessionRow};

/// How long a refusal of a sign-in that still looked renewable is kept: long
/// enough for the renewal it is due (the refresh loop's tick) to have run.
const SUSPECT_MS: i64 = 15 * 60 * 1000;

/// The copy of a sign-in a call was made with: its credential and grant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Copy {
    credential: String,
    grant_id: Option<String>,
    grant_version: u64,
}

/// The sign-in serving a session's provider on its channel here.
#[derive(Debug, Clone)]
pub(crate) struct Serving {
    copy: Copy,
    /// Known not to renew, or past its expiry: only a new sign-in serves.
    stalled: bool,
}

#[derive(Debug, Clone)]
struct Waiting {
    provider: String,
    copy: Copy,
    since_ms: i64,
    deadline_ms: Option<i64>,
}

#[derive(Debug, Clone)]
struct Suspect {
    provider: String,
    copy: Copy,
    at_ms: i64,
}

/// The sessions waiting for a sign-in, and those refused by one that may
/// yet turn out not to renew.
#[derive(Default)]
pub struct SignInWaits {
    waiting: parking_lot::Mutex<HashMap<String, Waiting>>,
    suspects: parking_lot::Mutex<HashMap<String, Suspect>>,
}

/// What the sweep does with one waiting session.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Next {
    /// Nothing yet.
    Wait,
    /// A new copy of the sign-in is here: carry on.
    Wake,
    /// Waited past the limit.
    End,
}

/// Decide for a session waiting on `waited`, given what serves it now.
pub(crate) fn next(
    waited: &Copy,
    now_serving: Option<&Serving>,
    deadline_ms: Option<i64>,
    now: i64,
) -> Next {
    match now_serving {
        Some(serving) if !serving.stalled && serving.copy != *waited => Next::Wake,
        _ if deadline_ms.is_some_and(|deadline| now >= deadline) => Next::End,
        _ => Next::Wait,
    }
}

/// A provider as a sentence names it.
pub fn provider_label(name: &str) -> &str {
    match name {
        "anthropic" => "Anthropic",
        "openai-codex" => "OpenAI Codex",
        "openai" => "OpenAI",
        other => other,
    }
}

fn duration(secs: u64) -> String {
    match secs {
        s if s % 3600 == 0 => format!("{} h", s / 3600),
        s if s % 60 == 0 => format!("{} min", s / 60),
        s => format!("{s} s"),
    }
}

impl Manager {
    /// The subscription sign-in of `provider` that serves `row`'s channel
    /// here, if the provider signs in at all.
    fn sign_in_serving(&self, row: &SessionRow, provider: &str, now: i64) -> Option<Serving> {
        let providers = self.providers()?;
        let entry = self.cfg.providers.get(provider)?;
        entry.login.as_deref().and_then(Flow::for_login)?;
        let broker = self.tools.broker.read().ok()?;
        let (credential, held) =
            broker.model_credential(provider, &entry.credential, &row.channel, &self.node_id)?;
        if held.kind != KIND_OAUTH {
            return None;
        }
        Some(Serving {
            stalled: providers.needs_reconnect(credential)
                || held.expires_ms.is_some_and(|expires| expires <= now),
            copy: Copy {
                credential: credential.to_string(),
                grant_id: held.grant_id.clone(),
                grant_version: held.grant_version,
            },
        })
    }

    /// The gateway saw `provider` refuse this session's call for its
    /// credential. A sign-in that can no longer be renewed puts the session
    /// to wait for a new one now; one that may still renew is remembered,
    /// and the session waits if its renewal fails.
    pub async fn provider_auth_refused(&self, id: &str, provider: &str) {
        let Ok(Some(row)) = self.store.get_session(id) else {
            return;
        };
        if row.node_id != self.node_id {
            return;
        }
        let now = now_ms();
        let Some(serving) = self.sign_in_serving(&row, provider, now) else {
            return;
        };
        if serving.stalled {
            self.wait_for_sign_in(&row, provider, serving.copy).await;
        } else {
            self.sign_in_waits.suspects.lock().insert(
                id.to_string(),
                Suspect {
                    provider: provider.to_string(),
                    copy: serving.copy,
                    at_ms: now,
                },
            );
        }
    }

    async fn wait_for_sign_in(&self, row: &SessionRow, provider: &str, copy: Copy) {
        if !matches!(
            SessionState::from_stored(&row.state),
            SessionState::Running | SessionState::WaitingOnYou
        ) {
            return;
        }
        let now = now_ms();
        let limit = self.cfg.live().session.auth_wait_secs;
        let deadline_ms = (limit > 0).then(|| now.saturating_add(limit as i64 * 1000));
        {
            let mut waiting = self.sign_in_waits.waiting.lock();
            if waiting.contains_key(&row.id) {
                return;
            }
            waiting.insert(
                row.id.clone(),
                Waiting {
                    provider: provider.to_string(),
                    copy: copy.clone(),
                    since_ms: now,
                    deadline_ms,
                },
            );
        }
        self.sign_in_waits.suspects.lock().remove(&row.id);
        let reason = format!("waiting for a sign-in to {}", provider_label(provider));
        let (ack, wait) = oneshot::channel();
        let sent = self
            .send(
                &row.id,
                Command::Pause {
                    source: PauseSource::SignIn,
                    reason,
                    ack,
                },
            )
            .await;
        let paused = match sent {
            Ok(()) => wait.await.unwrap_or_else(|_| Err("session stopped".into())),
            Err(e) => Err(e.to_string()),
        };
        if let Err(error) = paused {
            tracing::info!(session = %row.id, %error, "session not put to wait for a sign-in");
            self.sign_in_waits.waiting.lock().remove(&row.id);
            return;
        }
        tracing::info!(session = %row.id, provider, credential = %copy.credential, "session waits for a sign-in");
        self.record_event(
            &row.id,
            ek::SIGN_IN_WAIT,
            json!({
                "outcome": "waiting",
                "provider": provider,
                "credential": copy.credential,
                "deadline_ms": deadline_ms,
            }),
        );
    }

    /// What this session waits for, if it waits for a sign-in.
    pub fn sign_in_wait(&self, id: &str) -> Option<Value> {
        let waiting = self.sign_in_waits.waiting.lock();
        let wait = waiting.get(id)?;
        Some(json!({
            "provider": wait.provider,
            "credential": wait.copy.credential,
            "node_id": self.node_id,
            "since_ms": wait.since_ms,
            "deadline_ms": wait.deadline_ms,
        }))
    }

    /// Put to wait each session refused by a sign-in that has since turned
    /// out not to renew; wake each waiting session a new sign-in has come
    /// for; end each that waited past `[session] auth_wait_secs`. Run from
    /// the node's tick, and whenever a sign-in arrives.
    pub async fn wake_signed_in(&self) {
        if self.restart.draining() {
            return;
        }
        let now = now_ms();
        let suspects: Vec<(String, Suspect)> = {
            let mut suspects = self.sign_in_waits.suspects.lock();
            suspects.retain(|_, s| now - s.at_ms < SUSPECT_MS);
            suspects
                .iter()
                .map(|(id, s)| (id.clone(), s.clone()))
                .collect()
        };
        for (id, suspect) in suspects {
            let Ok(Some(row)) = self.store.get_session(&id) else {
                self.sign_in_waits.suspects.lock().remove(&id);
                continue;
            };
            match self.sign_in_serving(&row, &suspect.provider, now) {
                // The same copy, now known not to renew.
                Some(serving) if serving.copy == suspect.copy && serving.stalled => {
                    self.wait_for_sign_in(&row, &suspect.provider, suspect.copy)
                        .await;
                }
                Some(serving) if serving.copy == suspect.copy => {}
                // Renewed, or gone: nothing to wait for.
                _ => {
                    self.sign_in_waits.suspects.lock().remove(&id);
                }
            }
        }

        let waiting: Vec<(String, Waiting)> = self
            .sign_in_waits
            .waiting
            .lock()
            .iter()
            .map(|(id, w)| (id.clone(), w.clone()))
            .collect();
        for (id, wait) in waiting {
            let row = match self.store.get_session(&id) {
                Ok(Some(row)) if row.state == SessionState::Paused.as_str() => row,
                // Resumed or stopped by the operator, or gone: theirs now.
                Ok(_) => {
                    self.sign_in_waits.waiting.lock().remove(&id);
                    continue;
                }
                Err(_) => continue,
            };
            let serving = self.sign_in_serving(&row, &wait.provider, now);
            match next(&wait.copy, serving.as_ref(), wait.deadline_ms, now) {
                Next::Wait => {}
                Next::Wake => self.wake_for_sign_in(&id, &wait).await,
                Next::End => self.end_sign_in_wait(&id, &wait).await,
            }
        }
    }

    async fn wake_for_sign_in(&self, id: &str, wait: &Waiting) {
        let label = provider_label(&wait.provider);
        let (ack, resumed) = oneshot::channel();
        let sent = self
            .send(
                id,
                Command::Resume {
                    source: PauseSource::Wake,
                    reason: format!("a new sign-in to {label} is here"),
                    ack,
                },
            )
            .await;
        let resumed = match sent {
            Ok(()) => resumed
                .await
                .unwrap_or_else(|_| Err("session stopped".into())),
            Err(e) => Err(e.to_string()),
        };
        if let Err(error) = resumed {
            // A fenced turn still settling: the next sweep tries again.
            tracing::info!(session = %id, %error, "session waiting for a sign-in not resumed yet");
            return;
        }
        self.sign_in_waits.waiting.lock().remove(id);
        let _ = self.store.update_session(
            id,
            crate::store::SessionPatch {
                last_error: Some(String::new()),
                ..Default::default()
            },
        );
        if let Ok(Some(row)) = self.store.get_session(id) {
            self.bus
                .publish(crate::stream::Frame::Session(Box::new(row)));
        }
        self.record_event(
            id,
            ek::SIGN_IN_WAIT,
            json!({ "outcome": "resumed", "provider": wait.provider }),
        );
        let _ = self
            .prompt(
                id,
                format!(
                    "A new sign-in to {label} arrived and the node resumed this session. \
                     Your last model call was refused because the sign-in had expired, \
                     which cut your turn off; check what of it landed, then carry on \
                     from where you were."
                ),
            )
            .await;
    }

    async fn end_sign_in_wait(&self, id: &str, wait: &Waiting) {
        self.sign_in_waits.waiting.lock().remove(id);
        let limit = self.cfg.live().session.auth_wait_secs;
        let reason = format!(
            "no new sign-in to {} within {}, so the session ended; sign in again and carry the work on",
            provider_label(&wait.provider),
            duration(limit),
        );
        let _ = self.store.update_session(
            id,
            crate::store::SessionPatch {
                last_error: Some(reason.clone()),
                ..Default::default()
            },
        );
        self.record_event(
            id,
            ek::SIGN_IN_WAIT,
            json!({ "outcome": "ended", "provider": wait.provider, "note": reason }),
        );
        let _ = self.send(id, Command::End(EndReason::Error)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn copy(credential: &str, grant: &str, version: u64) -> Copy {
        Copy {
            credential: credential.into(),
            grant_id: Some(grant.into()),
            grant_version: version,
        }
    }

    fn serving(copy: Copy, stalled: bool) -> Serving {
        Serving { copy, stalled }
    }

    /// A waiting session wakes for a new copy of its sign-in that renews
    /// (a new sign-in, a renewal, or a handoff), never for the copy that
    /// refused it, and ends only past its limit.
    #[test]
    fn a_waiting_session_wakes_only_for_a_new_copy_that_renews() {
        let waited = copy("anthropic", "g1", 3);
        assert_eq!(next(&waited, None, None, 10), Next::Wait);
        assert_eq!(
            next(&waited, Some(&serving(waited.clone(), true)), None, 10),
            Next::Wait
        );
        // A note forgotten (a restart of the refresh loop's knowledge) is
        // not a new sign-in.
        assert_eq!(
            next(&waited, Some(&serving(waited.clone(), false)), None, 10),
            Next::Wait
        );
        // A handoff of a renewed copy, a sign-in again, a sign-in under
        // another name for the channel: each wakes it.
        for renewed in [
            copy("anthropic", "g1", 4),
            copy("anthropic", "g2", 0),
            copy("anthropic@work", "g3", 0),
        ] {
            assert_eq!(
                next(&waited, Some(&serving(renewed.clone(), false)), Some(5), 10),
                Next::Wake
            );
            // Arrived, but no better.
            assert_eq!(
                next(&waited, Some(&serving(renewed, true)), Some(50), 10),
                Next::Wait
            );
        }
        assert_eq!(next(&waited, None, Some(10), 10), Next::End);
        assert_eq!(
            next(&waited, Some(&serving(waited.clone(), true)), Some(9), 10),
            Next::End
        );
    }

    #[test]
    fn the_limit_reads_in_its_own_unit() {
        assert_eq!(duration(86_400), "24 h");
        assert_eq!(duration(5400), "90 min");
        assert_eq!(duration(45), "45 s");
        assert_eq!(provider_label("anthropic"), "Anthropic");
        assert_eq!(provider_label("custom"), "custom");
    }
}
