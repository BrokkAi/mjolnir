//! The store fence: one row that stops every daemon from starting on a store
//! that was moved to another machine, and on every copy of it, until
//! `mj daemon unfence`.
//!
//! The fence is read before the schema is verified or migrated
//! (`schema::open_writable`), so a newer build refuses a fenced store without
//! first advancing it. Readers stay allowed so an offline copy can be edited.

use super::values::{blob_to_path, path_to_blob};
use super::*;

/// Where and why a store was fenced. The host and data directory say where the
/// row was written, so a copy can tell it is not the original.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct StoreFenceRecord {
    pub destination: String,
    pub fenced_at: String,
    pub fenced_host: String,
    pub fenced_data_dir: PathBuf,
    pub note: Option<String>,
}

impl StoreFenceRecord {
    /// The refusal a daemon or client at `current_data_dir` on this host gets.
    pub fn refusal(&self, current_data_dir: &Path) -> StoreFenced {
        StoreFenced::new(
            self.destination.clone(),
            self.fenced_at.clone(),
            self.fenced_host.clone(),
            self.fenced_data_dir.clone(),
            self.note.clone(),
            &mj_core::storage::local_host_name(),
            current_data_dir,
        )
    }

    fn validate(&self) -> Result<()> {
        ensure!(
            !self.destination.trim().is_empty(),
            "fence destination must not be empty"
        );
        ensure!(
            !self.fenced_at.trim().is_empty(),
            "fence time must not be empty"
        );
        ensure!(
            !self.fenced_host.trim().is_empty(),
            "fence host must not be empty"
        );
        ensure!(
            !self.fenced_data_dir.as_os_str().is_empty(),
            "fence data directory must not be empty"
        );
        Ok(())
    }
}

/// The fence row, or `None` when the store is not fenced or predates the
/// `store_fence` table (a store without the table is unfenced).
pub fn read_store_fence(connection: &Connection) -> Result<Option<StoreFenceRecord>> {
    let table_exists: bool = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'store_fence')",
        [],
        |row| row.get(0),
    )?;
    if !table_exists {
        return Ok(None);
    }
    let record = connection
        .query_row(
            "SELECT destination, fenced_at, fenced_host, fenced_data_dir, note
             FROM store_fence WHERE singleton = 1",
            [],
            |row| {
                let data_dir: Vec<u8> = row.get(3)?;
                Ok(StoreFenceRecord {
                    destination: row.get(0)?,
                    fenced_at: row.get(1)?,
                    fenced_host: row.get(2)?,
                    fenced_data_dir: blob_to_path(&data_dir),
                    note: row.get(4)?,
                })
            },
        )
        .optional()
        .context("read store fence")?;
    Ok(record)
}

/// Write the fence row, replacing an earlier one.
pub fn set_store_fence(connection: &Connection, record: &StoreFenceRecord) -> Result<()> {
    record.validate()?;
    connection
        .execute(
            "INSERT INTO store_fence
                 (singleton, destination, fenced_at, fenced_host, fenced_data_dir, note)
             VALUES (1, ?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(singleton) DO UPDATE SET
                 destination = excluded.destination,
                 fenced_at = excluded.fenced_at,
                 fenced_host = excluded.fenced_host,
                 fenced_data_dir = excluded.fenced_data_dir,
                 note = excluded.note",
            params![
                record.destination,
                record.fenced_at,
                record.fenced_host,
                path_to_blob(&record.fenced_data_dir),
                record.note,
            ],
        )
        .context("write store fence")?;
    Ok(())
}

/// Remove the fence row. Returns whether one existed.
pub fn clear_store_fence(connection: &Connection) -> Result<bool> {
    let removed = connection
        .execute("DELETE FROM store_fence WHERE singleton = 1", [])
        .context("clear store fence")?;
    Ok(removed > 0)
}

/// The fence of the store at the configured path, read without creating,
/// migrating, or writing it. A missing database is unfenced.
pub fn read_configured_store_fence() -> Result<Option<StoreFenceRecord>> {
    schema::read_store_fence_at(&database_path())
}

/// Fence the configured store: the caller holds the controller lock, so no
/// daemon writes it. Opens the store writable without the fence check, which
/// migrates it to the revision that has the fence table. Returns the fence it
/// replaced, if any.
pub fn fence_configured_store(
    destination: &str,
    note: Option<&str>,
) -> Result<(StoreFenceRecord, Option<StoreFenceRecord>)> {
    let connection = schema::open_writable_ignoring_fence(&database_path())?;
    let previous = read_store_fence(&connection)?;
    let record = StoreFenceRecord {
        destination: destination.to_owned(),
        fenced_at: Utc::now().to_rfc3339(),
        fenced_host: mj_core::storage::local_host_name(),
        fenced_data_dir: mj_core::config::DataLayout::current().root().to_path_buf(),
        note: note.map(str::to_owned),
    };
    set_store_fence(&connection, &record)?;
    Ok((record, previous))
}

/// Remove the fence from the configured store. A store with no fence is left
/// untouched, not even migrated. Returns the fence that was removed.
pub fn unfence_configured_store() -> Result<Option<StoreFenceRecord>> {
    if read_configured_store_fence()?.is_none() {
        return Ok(None);
    }
    let connection = schema::open_writable_ignoring_fence(&database_path())?;
    let previous = read_store_fence(&connection)?;
    clear_store_fence(&connection)?;
    Ok(previous)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fence_can_be_set_replaced_and_cleared() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("mj.sqlite3");
        let connection = schema::open_writable_ignoring_fence(&path).unwrap();
        assert_eq!(read_store_fence(&connection).unwrap(), None);
        let record = StoreFenceRecord {
            destination: "ssh://b".to_owned(),
            fenced_at: "2026-11-25T00:00:00Z".to_owned(),
            fenced_host: "a".to_owned(),
            fenced_data_dir: PathBuf::from("/srv/mj"),
            note: Some("test".to_owned()),
        };
        set_store_fence(&connection, &record).unwrap();
        assert_eq!(read_store_fence(&connection).unwrap(), Some(record.clone()));
        assert!(schema::open_writer(&path).is_err());
        assert!(clear_store_fence(&connection).unwrap());
        assert_eq!(read_store_fence(&connection).unwrap(), None);
        drop(connection);
        schema::open_writer(&path).unwrap();
    }
}
