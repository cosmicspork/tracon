//! The reads a work-level continuation view is assembled from: a session's
//! whole lineage, and what was decided and shown across a set of attempts.
//! Everything here is recorded state; nothing is summarized. The one write is
//! a continuation taking over the reviews its attempt left open.

use rusqlite::params_from_iter;
use serde::Serialize;

use super::{Result, ReviewRow, SessionRow, Store};

/// One thing the operator decided for an attempt: a permission they answered
/// or a question they replied to.
#[derive(Debug, Clone, Serialize)]
pub struct Decided {
    pub session_id: String,
    /// `permission` or `question`.
    pub kind: &'static str,
    /// What was asked, as the operator saw it.
    pub asked: String,
    /// The option they picked, or their answer.
    pub answer: String,
    pub at_ms: i64,
}

/// The review states a continuation takes over: `new`, `claimed` and
/// `revising`.
const CARRIED: &str = "'new','claimed','revising'";

fn placeholders(n: usize) -> String {
    vec!["?"; n].join(",")
}

impl Store {
    /// Every session linked to `id` by lineage (`continued_from` or
    /// `parent_session`, either direction, transitively), `id` included,
    /// oldest first. A session with no lineage is a lineage of one.
    pub fn session_lineage(&self, id: &str) -> Result<Vec<SessionRow>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "WITH RECURSIVE linked(id) AS (
                 SELECT ?1
                 UNION
                 SELECT s.id FROM session s JOIN linked l
                   ON s.continued_from = l.id OR s.parent_session = l.id
                 UNION
                 SELECT p.id FROM session s JOIN linked l ON s.id = l.id
                   JOIN session p ON p.id = s.continued_from OR p.id = s.parent_session
             )
             SELECT * FROM session WHERE id IN (SELECT id FROM linked) ORDER BY created_ms, id",
        )?;
        let rows = stmt
            .query_map([id], SessionRow::from_row)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// The reviews these sessions submitted, oldest first.
    pub fn reviews_of_sessions(&self, ids: &[String]) -> Result<Vec<ReviewRow>> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(&format!(
            "SELECT * FROM review WHERE session_id IN ({}) ORDER BY created_ms",
            placeholders(ids.len())
        ))?;
        let rows = stmt
            .query_map(params_from_iter(ids), ReviewRow::from_row)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// The reviews and reports `session_id` submitted that are still the
    /// work's to answer for, oldest first: waiting on the operator, or sent
    /// back for a revision.
    pub fn carried_reviews(&self, session_id: &str) -> Result<Vec<ReviewRow>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(&format!(
            "SELECT * FROM review WHERE session_id=?1 AND state IN ({CARRIED})
              ORDER BY created_ms, id"
        ))?;
        let rows = stmt
            .query_map([session_id], ReviewRow::from_row)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Hand `from`'s [carried](Self::carried_reviews) reviews to `to`, the
    /// session continuing it, and return them as they now stand. One write:
    /// every read that follows a review's session (its waits, its decision's
    /// delivery, who may resubmit it) then follows the live attempt. A review
    /// being published, decided or gone stays with the attempt it was decided
    /// for.
    pub fn hand_over_reviews(&self, from: &str, to: &str) -> Result<Vec<ReviewRow>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(&format!(
            "UPDATE review SET session_id=?2 WHERE session_id=?1 AND state IN ({CARRIED})
             RETURNING *"
        ))?;
        let mut rows = stmt
            .query_map([from, to], ReviewRow::from_row)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        rows.sort_by(|a, b| (a.created_ms, &a.id).cmp(&(b.created_ms, &b.id)));
        Ok(rows)
    }

    /// What the operator decided across these sessions, oldest first: the
    /// permissions they answered (not the ones that expired or the policy
    /// settled) and the questions they replied to.
    pub fn decisions_of_sessions(&self, ids: &[String]) -> Result<Vec<Decided>> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let conn = self.conn.lock().unwrap();
        let mut out = Vec::new();
        let mut stmt = conn.prepare(&format!(
            "SELECT session_id, title, answer_option_id, created_ms FROM permission_request
              WHERE session_id IN ({}) AND state='answered' AND answer_option_id IS NOT NULL",
            placeholders(ids.len())
        ))?;
        let permissions = stmt.query_map(params_from_iter(ids), |r| {
            Ok(Decided {
                session_id: r.get(0)?,
                kind: "permission",
                asked: r.get(1)?,
                answer: r.get(2)?,
                at_ms: r.get(3)?,
            })
        })?;
        for row in permissions {
            out.push(row?);
        }
        let mut stmt = conn.prepare(&format!(
            "SELECT session_id, prompt, answer_json, COALESCE(answered_ms, created_ms)
               FROM operator_question
              WHERE session_id IN ({}) AND state='answered'",
            placeholders(ids.len())
        ))?;
        let questions = stmt.query_map(params_from_iter(ids), |r| {
            let answer: Option<String> = r.get(2)?;
            Ok(Decided {
                session_id: r.get(0)?,
                kind: "question",
                asked: r.get(1)?,
                answer: answer.map(|a| answer_text(&a)).unwrap_or_default(),
                at_ms: r.get(3)?,
            })
        })?;
        for row in questions {
            out.push(row?);
        }
        out.sort_by_key(|d| d.at_ms);
        Ok(out)
    }

    /// How many permission requests each of these sessions has open.
    pub fn open_permissions_of_sessions(&self, ids: &[String]) -> Result<i64> {
        if ids.is_empty() {
            return Ok(0);
        }
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            &format!(
                "SELECT COUNT(*) FROM permission_request WHERE session_id IN ({}) AND state='new'",
                placeholders(ids.len())
            ),
            params_from_iter(ids),
            |r| r.get(0),
        )
        .map_err(Into::into)
    }

    /// How many questions to the operator these sessions have unanswered.
    pub fn open_questions_of_sessions(&self, ids: &[String]) -> Result<i64> {
        if ids.is_empty() {
            return Ok(0);
        }
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            &format!(
                "SELECT COUNT(*) FROM operator_question WHERE session_id IN ({}) AND state='unanswered'",
                placeholders(ids.len())
            ),
            params_from_iter(ids),
            |r| r.get(0),
        )
        .map_err(Into::into)
    }
}

/// An operator's answer as stored (`{"choice": …}`, `{"text": …}`, a bare
/// string, or anything else), as one readable line.
fn answer_text(json: &str) -> String {
    match serde_json::from_str::<serde_json::Value>(json) {
        Ok(serde_json::Value::String(s)) => s,
        // A choice with a note says both.
        Ok(v) => match (v["choice"].as_str(), v["text"].as_str()) {
            (Some(choice), Some(text)) => format!("{choice}: {text}"),
            _ => ["choice", "text", "answer", "value"]
                .iter()
                .find_map(|k| v[*k].as_str().map(str::to_string))
                .unwrap_or_else(|| v.to_string()),
        },
        Err(_) => json.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::answer_text;

    #[test]
    fn a_question_answer_reads_as_its_choice_text_or_both() {
        assert_eq!(answer_text(r#"{"choice":"skip"}"#), "skip");
        assert_eq!(answer_text(r#"{"text":"ask Sam"}"#), "ask Sam");
        assert_eq!(
            answer_text(r#"{"choice":"skip","text":"the key lands Monday"}"#),
            "skip: the key lands Monday"
        );
    }
}
