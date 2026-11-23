//! Repository images pushed to a registry, as the signed statements nodes
//! make of them (`crate::repo_image_share`): this node's own, which it sends
//! its channels, and those its channel co-members sent it. Node-local and
//! never replicated.

use proto::image::ImageStatement;
use rusqlite::params;

use super::{Result, Store};

/// How long a statement is kept after it was signed. A node says its own
/// again with every snapshot, so one still standing is refreshed long
/// before; one for a recipe nobody builds any more lapses.
pub const KEEP_MS: i64 = 30 * 24 * 3600 * 1000;

/// The most statements one co-member may have here at once: past it, its
/// oldest go.
pub const MAX_PER_SIGNER: i64 = 256;

/// One statement and the channel it came on; empty for this node's own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatementRow {
    pub channel: String,
    pub statement: ImageStatement,
    pub received_ms: i64,
}

fn rows(
    stmt: &mut rusqlite::Statement<'_>,
    args: impl rusqlite::Params,
) -> Result<Vec<StatementRow>> {
    let rows = stmt
        .query_map(args, |r| {
            let json: String = r.get(1)?;
            Ok((r.get::<_, String>(0)?, json, r.get::<_, i64>(2)?))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows
        .into_iter()
        .filter_map(|(channel, json, received_ms)| {
            serde_json::from_str(&json)
                .ok()
                .map(|statement| StatementRow {
                    channel,
                    statement,
                    received_ms,
                })
        })
        .collect())
}

impl Store {
    /// Keep `statement` as signed by its signer, received on `channel`
    /// (empty for this node's own). A newer statement for the same recipe
    /// from the same signer on the same channel replaces an older one; an
    /// older one changes nothing. Returns whether it was kept.
    pub fn repo_image_statement_put(
        &self,
        channel: &str,
        statement: &ImageStatement,
        now_ms: i64,
    ) -> Result<bool> {
        let json = serde_json::to_string(statement).unwrap_or_default();
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        tx.execute(
            "DELETE FROM repo_image_statement WHERE signed_ms < ?1",
            [now_ms - KEEP_MS],
        )?;
        let changed = tx.execute(
            "INSERT INTO repo_image_statement
                 (signer, channel, repo, recipe_hash, platform, statement_json, signed_ms, received_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
             ON CONFLICT (signer, channel, repo, recipe_hash, platform) DO UPDATE SET
                 statement_json = excluded.statement_json,
                 signed_ms = excluded.signed_ms,
                 received_ms = excluded.received_ms
             WHERE excluded.signed_ms > repo_image_statement.signed_ms",
            params![
                statement.signer,
                channel,
                statement.repo,
                statement.recipe,
                statement.platform,
                json,
                statement.signed_ms,
                now_ms
            ],
        )?;
        tx.execute(
            "DELETE FROM repo_image_statement WHERE signer = ?1 AND rowid NOT IN (
                 SELECT rowid FROM repo_image_statement WHERE signer = ?1
                 ORDER BY signed_ms DESC LIMIT ?2)",
            params![statement.signer, MAX_PER_SIGNER],
        )?;
        tx.commit()?;
        Ok(changed > 0)
    }

    /// The statements `signer` made itself (`channel` empty), newest first.
    pub fn repo_image_statements_own(&self, signer: &str) -> Result<Vec<StatementRow>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT channel, statement_json, received_ms FROM repo_image_statement
             WHERE signer = ?1 AND channel = '' AND signed_ms >= ?2
             ORDER BY signed_ms DESC",
        )?;
        rows(&mut stmt, params![signer, super::now_ms() - KEEP_MS])
    }

    /// Statements other nodes than `self_id` made for `recipe_hash` of
    /// `repo` on `platform`, newest first.
    pub fn repo_image_statements_for(
        &self,
        self_id: &str,
        repo: &str,
        recipe_hash: &str,
        platform: &str,
    ) -> Result<Vec<StatementRow>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT channel, statement_json, received_ms FROM repo_image_statement
             WHERE repo = ?1 AND recipe_hash = ?2 AND platform = ?3
               AND signer != ?4 AND channel != '' AND signed_ms >= ?5
             ORDER BY signed_ms DESC",
        )?;
        rows(
            &mut stmt,
            params![
                repo,
                recipe_hash,
                platform,
                self_id,
                super::now_ms() - KEEP_MS
            ],
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proto::keys::Identity;

    fn statement(id: &Identity, recipe: &str, signed_ms: i64) -> ImageStatement {
        ImageStatement::sign(
            id,
            "github.com/acme/app",
            recipe,
            "linux/amd64",
            &format!(
                "registry.digitalocean.com/acme/r@sha256:{}",
                "cd".repeat(32)
            ),
            signed_ms,
        )
    }

    #[test]
    fn statements_are_kept_per_signer_channel_and_recipe_newest_winning() {
        let store = Store::open_in_memory().unwrap();
        let me = Identity::from_seed(&[1; 32]);
        let peer = Identity::from_seed(&[2; 32]);
        let now = crate::store::now_ms();
        let recipe = "ab".repeat(32);
        assert!(store
            .repo_image_statement_put("", &statement(&me, &recipe, now), now)
            .unwrap());
        assert!(store
            .repo_image_statement_put("team", &statement(&peer, &recipe, now - 10), now)
            .unwrap());
        // An older word for the same recipe changes nothing; a newer one wins.
        assert!(!store
            .repo_image_statement_put("team", &statement(&peer, &recipe, now - 20), now)
            .unwrap());
        assert!(store
            .repo_image_statement_put("team", &statement(&peer, &recipe, now - 5), now)
            .unwrap());

        let own = store.repo_image_statements_own(&me.node_id()).unwrap();
        assert_eq!(own.len(), 1);
        assert_eq!(own[0].statement.signer, me.node_id());

        let theirs = store
            .repo_image_statements_for(&me.node_id(), "github.com/acme/app", &recipe, "linux/amd64")
            .unwrap();
        assert_eq!(theirs.len(), 1);
        assert_eq!(theirs[0].channel, "team");
        assert_eq!(theirs[0].statement.signed_ms, now - 5);
        assert!(store
            .repo_image_statements_for(&me.node_id(), "github.com/acme/app", &recipe, "linux/arm64")
            .unwrap()
            .is_empty());

        // One signed past what is kept is gone.
        let old = "ef".repeat(32);
        store
            .repo_image_statement_put("team", &statement(&peer, &old, now - KEEP_MS - 1), now)
            .unwrap();
        assert!(store
            .repo_image_statements_for(&me.node_id(), "github.com/acme/app", &old, "linux/amd64")
            .unwrap()
            .is_empty());
    }
}
