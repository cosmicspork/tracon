//! What the operator sent a session while its turn was running.
//!
//! A prompt cannot start a second turn under one that is still going, so it
//! is held here until it reaches the agent: appended to the next node tool
//! result the session receives, or sent as the next prompt once the turn ends.
//! Both paths claim a message through [`Store::claim_queued_prompts`], which
//! marks it delivered in the same transaction it reads it, so it reaches the
//! agent once whichever path gets there first.

use rusqlite::params;
use serde::Serialize;

use super::{now_ms, Result, Store};

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct QueuedPrompt {
    pub id: String,
    pub session_id: String,
    pub text: String,
    pub created_ms: i64,
}

impl Store {
    /// Hold `text` for `session_id` until it can be delivered.
    pub fn queue_prompt(&self, session_id: &str, text: &str) -> Result<QueuedPrompt> {
        let queued = QueuedPrompt {
            id: uuid::Uuid::now_v7().to_string(),
            session_id: session_id.to_string(),
            text: text.to_string(),
            created_ms: now_ms(),
        };
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO queued_prompt (id, session_id, text, created_ms) VALUES (?1, ?2, ?3, ?4)",
            params![queued.id, queued.session_id, queued.text, queued.created_ms],
        )?;
        Ok(queued)
    }

    /// Every message still undelivered for `session_id`, oldest first, now
    /// marked delivered by `via`. A message is returned by exactly one call.
    pub fn claim_queued_prompts(&self, session_id: &str, via: &str) -> Result<Vec<QueuedPrompt>> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        let claimed = {
            let mut stmt = tx.prepare(
                "SELECT id, session_id, text, created_ms FROM queued_prompt
                  WHERE session_id = ?1 AND delivered_ms IS NULL
                  ORDER BY created_ms, id",
            )?;
            let rows = stmt.query_map(params![session_id], |r| {
                Ok(QueuedPrompt {
                    id: r.get(0)?,
                    session_id: r.get(1)?,
                    text: r.get(2)?,
                    created_ms: r.get(3)?,
                })
            })?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
        };
        if !claimed.is_empty() {
            tx.execute(
                "UPDATE queued_prompt SET delivered_ms = ?2, delivered_via = ?3
                  WHERE session_id = ?1 AND delivered_ms IS NULL",
                params![session_id, now_ms(), via],
            )?;
        }
        tx.commit()?;
        Ok(claimed)
    }

    /// Put claimed messages back: what was to carry them was refused, so they
    /// never reached the agent.
    pub fn release_queued_prompts(&self, ids: &[String]) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        for id in ids {
            conn.execute(
                "UPDATE queued_prompt SET delivered_ms = NULL, delivered_via = NULL WHERE id = ?1",
                params![id],
            )?;
        }
        Ok(())
    }

    /// How many messages `session_id` still holds for its agent.
    pub fn queued_prompt_count(&self, session_id: &str) -> Result<i64> {
        let conn = self.conn.lock().unwrap();
        Ok(conn.query_row(
            "SELECT COUNT(*) FROM queued_prompt WHERE session_id = ?1 AND delivered_ms IS NULL",
            params![session_id],
            |r| r.get(0),
        )?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_message_is_claimed_once_and_can_be_put_back() {
        let store = Store::open_in_memory().unwrap();
        let first = store.queue_prompt("s1", "one").unwrap();
        let second = store.queue_prompt("s1", "two").unwrap();
        store.queue_prompt("s2", "elsewhere").unwrap();
        assert_eq!(store.queued_prompt_count("s1").unwrap(), 2);

        let claimed = store
            .claim_queued_prompts("s1", "tool:review_status")
            .unwrap();
        assert_eq!(claimed, vec![first.clone(), second.clone()]);
        assert!(store
            .claim_queued_prompts("s1", "prompt")
            .unwrap()
            .is_empty());
        assert_eq!(store.queued_prompt_count("s1").unwrap(), 0);
        assert_eq!(store.queued_prompt_count("s2").unwrap(), 1);

        store
            .release_queued_prompts(&[first.id.clone(), second.id.clone()])
            .unwrap();
        assert_eq!(
            store.claim_queued_prompts("s1", "prompt").unwrap(),
            vec![first, second]
        );
    }
}
