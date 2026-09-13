//! The launch manifest's two tables: what the operator imported, and the
//! revisions that were built from it.
//!
//! The split is the whole point. A session records a digest, and that digest
//! has to still mean something after the operator has edited the manifest —
//! so a revision is written once, never rewritten, and looked up by digest.
//! What the operator edits is the other table, where a name exists at most
//! once per channel and kind, which is how "reject duplicate skill names"
//! becomes a property of the store rather than a check somebody has to
//! remember to run.

use rusqlite::{params, OptionalExtension};

use super::{now_ms, Result, Store, StoreError};
use crate::manifest::{LaunchManifest, ManifestFile, SkillEntry, TextEntry};

pub const KIND_SKILL: &str = "skill";
pub const KIND_INSTRUCTION: &str = "instruction";
pub const KIND_AGENT: &str = "agent";

/// One row of the editable half, as the API and the CLI hand it around.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ManifestItemRow {
    pub channel: String,
    pub kind: String,
    pub name: String,
    pub source: String,
    pub digest: String,
    /// A skill's files as JSON; an instruction's or agent's body as text.
    pub body: String,
    #[serde(default)]
    pub warnings: Vec<String>,
    pub imported_ms: i64,
}

impl ManifestItemRow {
    fn from_row(r: &rusqlite::Row) -> rusqlite::Result<Self> {
        let warnings: String = r.get("warnings")?;
        Ok(Self {
            channel: r.get("channel")?,
            kind: r.get("kind")?,
            name: r.get("name")?,
            source: r.get("source")?,
            digest: r.get("digest")?,
            body: r.get("body")?,
            warnings: serde_json::from_str(&warnings).unwrap_or_default(),
            imported_ms: r.get("imported_ms")?,
        })
    }
}

impl Store {
    /// Store a skill package. Replacing an existing name is how an operator
    /// re-imports a package they changed; two *different* skills with one name
    /// cannot coexist, because the primary key will not hold both.
    pub fn manifest_put_skill(&self, channel: &str, entry: &SkillEntry) -> Result<()> {
        let body = serde_json::to_string(&entry.files)?;
        let warnings = serde_json::to_string(&entry.warnings)?;
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO manifest_item
                (channel, kind, name, source, digest, body, warnings, imported_ms)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8)
             ON CONFLICT(channel, kind, name) DO UPDATE SET
                source=?4, digest=?5, body=?6, warnings=?7, imported_ms=?8",
            params![
                channel,
                KIND_SKILL,
                entry.name,
                entry.source,
                entry.digest,
                body,
                warnings,
                now_ms(),
            ],
        )?;
        Ok(())
    }

    /// Store an instruction or an agent: text the node owns, named by the
    /// operator.
    pub fn manifest_put_text(
        &self,
        channel: &str,
        kind: &str,
        name: &str,
        body: &str,
    ) -> Result<()> {
        if kind != KIND_INSTRUCTION && kind != KIND_AGENT {
            return Err(StoreError::Invalid(format!(
                "`{kind}` is not a manifest text kind"
            )));
        }
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO manifest_item
                (channel, kind, name, source, digest, body, warnings, imported_ms)
             VALUES (?1,?2,?3,'operator','',?4,'[]',?5)
             ON CONFLICT(channel, kind, name) DO UPDATE SET body=?4, imported_ms=?5",
            params![channel, kind, name, body, now_ms()],
        )?;
        Ok(())
    }

    pub fn manifest_items(&self, channel: &str) -> Result<Vec<ManifestItemRow>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt =
            conn.prepare("SELECT * FROM manifest_item WHERE channel=?1 ORDER BY kind, name")?;
        let rows = stmt
            .query_map([channel], ManifestItemRow::from_row)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn manifest_remove(&self, channel: &str, kind: &str, name: &str) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        let n = conn.execute(
            "DELETE FROM manifest_item WHERE channel=?1 AND kind=?2 AND name=?3",
            params![channel, kind, name],
        )?;
        Ok(n > 0)
    }

    /// The imported half of a channel's manifest, in the shapes the builder
    /// takes.
    pub fn manifest_contents(
        &self,
        channel: &str,
    ) -> Result<(Vec<SkillEntry>, Vec<TextEntry>, Vec<TextEntry>)> {
        let mut skills = Vec::new();
        let mut instructions = Vec::new();
        let mut agents = Vec::new();
        for row in self.manifest_items(channel)? {
            match row.kind.as_str() {
                KIND_SKILL => {
                    let files: Vec<ManifestFile> =
                        serde_json::from_str(&row.body).unwrap_or_default();
                    let description = files
                        .iter()
                        .find(|f| f.path == "SKILL.md")
                        .map(|f| description_of(&f.text))
                        .unwrap_or_default();
                    skills.push(SkillEntry {
                        name: row.name,
                        description,
                        source: row.source,
                        digest: row.digest,
                        warnings: row.warnings,
                        files,
                    });
                }
                KIND_INSTRUCTION => instructions.push(TextEntry {
                    name: row.name,
                    body: row.body,
                }),
                KIND_AGENT => agents.push(TextEntry {
                    name: row.name,
                    body: row.body,
                }),
                _ => {}
            }
        }
        Ok((skills, instructions, agents))
    }

    /// Record a built manifest, minting a revision only when its digest is
    /// new. Returns the manifest as it is now identified — the caller's copy
    /// with `revision` filled in — so a launch records the revision it
    /// actually ran, not the one it would have minted.
    pub fn manifest_record(&self, manifest: &LaunchManifest) -> Result<LaunchManifest> {
        let conn = self.conn.lock().unwrap();
        let existing: Option<i64> = conn
            .query_row(
                "SELECT revision FROM launch_manifest WHERE channel=?1 AND digest=?2",
                params![manifest.channel, manifest.digest],
                |r| r.get(0),
            )
            .optional()?;
        let mut recorded = manifest.clone();
        if let Some(revision) = existing {
            recorded.revision = revision;
            return Ok(recorded);
        }
        let next: i64 = conn.query_row(
            "SELECT COALESCE(MAX(revision), 0) + 1 FROM launch_manifest WHERE channel=?1",
            [&manifest.channel],
            |r| r.get(0),
        )?;
        recorded.revision = next;
        conn.execute(
            "INSERT INTO launch_manifest (channel, revision, digest, body, created_ms)
             VALUES (?1,?2,?3,?4,?5)",
            params![
                recorded.channel,
                next,
                recorded.digest,
                serde_json::to_string(&recorded)?,
                now_ms(),
            ],
        )?;
        Ok(recorded)
    }

    /// The manifest a session launched under, by the digest it recorded.
    /// `None` when that digest was never recorded here — a session from
    /// another node, or one that predates the manifest.
    pub fn manifest_by_digest(&self, digest: &str) -> Result<Option<LaunchManifest>> {
        let conn = self.conn.lock().unwrap();
        let body: Option<String> = conn
            .query_row(
                "SELECT body FROM launch_manifest WHERE digest=?1 ORDER BY revision LIMIT 1",
                [digest],
                |r| r.get(0),
            )
            .optional()?;
        match body {
            Some(body) => Ok(Some(serde_json::from_str(&body)?)),
            None => Ok(None),
        }
    }

    /// The latest revision recorded for a channel, for a pane that wants to
    /// say "running sessions are on r3, the next launch is r4".
    pub fn manifest_latest(&self, channel: &str) -> Result<Option<LaunchManifest>> {
        let conn = self.conn.lock().unwrap();
        let body: Option<String> = conn
            .query_row(
                "SELECT body FROM launch_manifest WHERE channel=?1
                 ORDER BY revision DESC LIMIT 1",
                [channel],
                |r| r.get(0),
            )
            .optional()?;
        match body {
            Some(body) => Ok(Some(serde_json::from_str(&body)?)),
            None => Ok(None),
        }
    }
}

/// The `description` out of a stored SKILL.md, for the effective-selection
/// listing. Re-derived rather than stored twice: the file is the truth, and a
/// second copy could disagree with it.
fn description_of(text: &str) -> String {
    let Some(body) = text
        .strip_prefix("---\n")
        .or_else(|| text.strip_prefix("---\r\n"))
    else {
        return String::new();
    };
    let end = body.find("\n---").unwrap_or(body.len());
    body[..end]
        .lines()
        .filter_map(|line| line.split_once(':'))
        .find(|(key, _)| key.trim() == "description")
        .map(|(_, value)| {
            value
                .trim()
                .trim_matches('"')
                .trim_matches('\'')
                .to_string()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest;

    fn entry(name: &str, body: &str) -> SkillEntry {
        let files = vec![ManifestFile {
            path: "SKILL.md".into(),
            text: format!("---\nname: {name}\ndescription: d\n---\n\n{body}\n"),
        }];
        SkillEntry {
            name: name.into(),
            description: "d".into(),
            source: "dir:/tmp/x".into(),
            digest: manifest::skill::digest_of(&files),
            warnings: vec!["code".into()],
            files,
        }
    }

    fn built(store: &Store, channel: &str) -> LaunchManifest {
        let (skills, instructions, agents) = store.manifest_contents(channel).unwrap();
        manifest::build(manifest::Inputs {
            channel,
            skills,
            instructions,
            agents,
            plugins: &[],
            baked: &[],
            lsp: Vec::new(),
            formatters: Vec::new(),
            providers: vec!["anthropic".into()],
            policy_revision: "1".into(),
        })
        .unwrap()
    }

    #[test]
    fn a_skill_round_trips_and_re_import_replaces_rather_than_duplicates() {
        let store = Store::open_in_memory().unwrap();
        store
            .manifest_put_skill("work", &entry("alpha", "one"))
            .unwrap();
        store
            .manifest_put_skill("work", &entry("alpha", "two"))
            .unwrap();
        let items = store.manifest_items("work").unwrap();
        assert_eq!(items.len(), 1);
        let (skills, _, _) = store.manifest_contents("work").unwrap();
        assert_eq!(skills.len(), 1);
        assert!(skills[0].files[0].text.contains("two"));
        assert_eq!(skills[0].description, "d");
    }

    #[test]
    fn a_revision_is_minted_only_when_the_digest_changes() {
        let store = Store::open_in_memory().unwrap();
        store
            .manifest_put_skill("work", &entry("alpha", "one"))
            .unwrap();
        let first = store.manifest_record(&built(&store, "work")).unwrap();
        assert_eq!(first.revision, 1);

        // Rebuilt from the same rows: same digest, same revision, no history.
        let again = store.manifest_record(&built(&store, "work")).unwrap();
        assert_eq!(again.revision, 1);
        assert_eq!(again.digest, first.digest);

        store
            .manifest_put_skill("work", &entry("alpha", "two"))
            .unwrap();
        let second = store.manifest_record(&built(&store, "work")).unwrap();
        assert_eq!(second.revision, 2);
        assert_ne!(second.digest, first.digest);

        // And the first is still resolvable, which is the point: a session
        // that recorded it launched with those bytes.
        let looked_up = store.manifest_by_digest(&first.digest).unwrap().unwrap();
        assert_eq!(looked_up.revision, 1);
        assert!(looked_up.skills[0].files[0].text.contains("one"));
        assert_eq!(store.manifest_latest("work").unwrap().unwrap().revision, 2);
    }

    #[test]
    fn channels_do_not_share_a_manifest() {
        let store = Store::open_in_memory().unwrap();
        store
            .manifest_put_skill("work", &entry("alpha", "one"))
            .unwrap();
        assert!(store.manifest_items("personal").unwrap().is_empty());
        let work = store.manifest_record(&built(&store, "work")).unwrap();
        let personal = store.manifest_record(&built(&store, "personal")).unwrap();
        assert_ne!(work.digest, personal.digest);
        assert_eq!(personal.revision, 1);
    }

    #[test]
    fn removing_an_item_is_reported_and_idempotent() {
        let store = Store::open_in_memory().unwrap();
        store
            .manifest_put_skill("work", &entry("alpha", "one"))
            .unwrap();
        assert!(store.manifest_remove("work", KIND_SKILL, "alpha").unwrap());
        assert!(!store.manifest_remove("work", KIND_SKILL, "alpha").unwrap());
    }

    #[test]
    fn instructions_and_agents_are_kept_apart() {
        let store = Store::open_in_memory().unwrap();
        store
            .manifest_put_text("work", KIND_INSTRUCTION, "house-style", "Small commits.")
            .unwrap();
        store
            .manifest_put_text("work", KIND_AGENT, "reviewer", "Reads diffs.")
            .unwrap();
        let (_, instructions, agents) = store.manifest_contents("work").unwrap();
        assert_eq!(instructions.len(), 1);
        assert_eq!(agents.len(), 1);
        assert_eq!(instructions[0].body, "Small commits.");
        assert!(store
            .manifest_put_text("work", "nonsense", "x", "y")
            .is_err());
    }
}
