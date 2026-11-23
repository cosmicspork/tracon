//! A consistent copy of the database, for a reader that must not hold the
//! store's lock while it walks every table (the data export).

use std::path::Path;

use super::{Result, Store};

impl Store {
    /// Write a copy of the whole database to `path`, which must not exist.
    /// One statement under the lock, so the copy is one point in time; the
    /// caller then reads the copy on its own connection.
    pub fn snapshot_into(&self, path: &Path) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute("VACUUM INTO ?1", [path.to_string_lossy()])?;
        Ok(())
    }
}
