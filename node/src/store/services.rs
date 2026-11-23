//! The services a session started, read back from its events.

use rusqlite::params;
use serde_json::Value;

use super::{Result, Store};

impl Store {
    /// Each `service` event's payload for a session, oldest first.
    pub fn session_service_events(&self, session_id: &str) -> Result<Vec<(Value, i64)>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT payload, at_ms FROM event WHERE session_id=?1 AND kind='service' ORDER BY seq",
        )?;
        let rows = stmt
            .query_map(params![session_id], |row| {
                let payload: String = row.get(0)?;
                Ok((
                    serde_json::from_str(&payload).unwrap_or(Value::Null),
                    row.get::<_, i64>(1)?,
                ))
            })?
            .collect::<std::result::Result<_, _>>()?;
        Ok(rows)
    }
}

/// One repository's persistent volume for a catalogue service
/// (`crate::service_volume`).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ServiceVolumeRow {
    pub volume: String,
    pub repo_path: String,
    pub service: String,
    pub created_ms: i64,
    pub used_ms: i64,
}

fn service_volume_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ServiceVolumeRow> {
    Ok(ServiceVolumeRow {
        volume: row.get(0)?,
        repo_path: row.get(1)?,
        service: row.get(2)?,
        created_ms: row.get(3)?,
        used_ms: row.get(4)?,
    })
}

const SERVICE_VOLUME_COLUMNS: &str = "volume, repo_path, service, created_ms, used_ms";

impl Store {
    /// Record that a session mounts `volume` for `repo_path`'s `service`
    /// now: made at `now_ms` the first time, used at `now_ms` every time.
    pub fn service_volume_used(
        &self,
        volume: &str,
        repo_path: &str,
        service: &str,
        now_ms: i64,
    ) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO service_volume (volume, repo_path, service, created_ms, used_ms)
             VALUES (?1, ?2, ?3, ?4, ?4)
             ON CONFLICT(volume) DO UPDATE SET used_ms = excluded.used_ms",
            params![volume, repo_path, service, now_ms],
        )?;
        Ok(())
    }

    /// Every service volume this node made, by repository then service.
    pub fn service_volumes(&self) -> Result<Vec<ServiceVolumeRow>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(&format!(
            "SELECT {SERVICE_VOLUME_COLUMNS} FROM service_volume ORDER BY repo_path, service"
        ))?;
        let rows = stmt
            .query_map([], service_volume_row)?
            .collect::<std::result::Result<_, _>>()?;
        Ok(rows)
    }

    pub fn service_volume(&self, volume: &str) -> Result<Option<ServiceVolumeRow>> {
        use rusqlite::OptionalExtension;
        let conn = self.conn.lock().unwrap();
        Ok(conn
            .query_row(
                &format!("SELECT {SERVICE_VOLUME_COLUMNS} FROM service_volume WHERE volume=?1"),
                params![volume],
                service_volume_row,
            )
            .optional()?)
    }

    /// Forget a service volume once it is removed.
    pub fn forget_service_volume(&self, volume: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "DELETE FROM service_volume WHERE volume=?1",
            params![volume],
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_service_volume_is_made_once_and_used_again() {
        let store = Store::open_in_memory().unwrap();
        store
            .service_volume_used("tracon-svcdata-a", "/r", "postgres", 10)
            .unwrap();
        store
            .service_volume_used("tracon-svcdata-a", "/r", "postgres", 20)
            .unwrap();
        store
            .service_volume_used("tracon-svcdata-b", "/r", "redis", 30)
            .unwrap();
        let rows = store.service_volumes().unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(
            (
                rows[0].service.as_str(),
                rows[0].created_ms,
                rows[0].used_ms
            ),
            ("postgres", 10, 20)
        );
        store.forget_service_volume("tracon-svcdata-a").unwrap();
        assert!(store.service_volume("tracon-svcdata-a").unwrap().is_none());
        assert_eq!(
            store
                .service_volume("tracon-svcdata-b")
                .unwrap()
                .unwrap()
                .repo_path,
            "/r"
        );
    }
}
