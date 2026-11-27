//! Stable legacy project-memory identities retained during host migration.

use super::*;

pub fn read_pinned_memory_key(session_id: &str) -> Result<Option<String>> {
    let connection = open_reader(&database_path())?;
    read_pinned_memory_key_with(&connection, session_id)
}

pub fn read_pinned_memory_key_with(
    connection: &Connection,
    session_id: &str,
) -> Result<Option<String>> {
    Ok(connection
        .query_row(
            "SELECT memory_key FROM session_memory_keys WHERE session_id = ?1",
            [session_id],
            |row| row.get(0),
        )
        .optional()?)
}

pub fn pin_session_memory_key_with(
    connection: &Connection,
    session_id: &str,
    memory_key: &str,
) -> Result<()> {
    connection.execute(
        "INSERT INTO session_memory_keys(session_id, memory_key) VALUES (?1, ?2)
         ON CONFLICT(session_id) DO UPDATE SET memory_key = excluded.memory_key",
        params![session_id, memory_key],
    )?;
    Ok(())
}

pub fn pin_session_memory_key(session_id: &str, memory_key: &str) -> Result<()> {
    let session_id = session_id.to_owned();
    let memory_key = memory_key.to_owned();
    submit_database_write("pin_session_memory_key", move |connection| {
        pin_session_memory_key_with(connection, &session_id, &memory_key)
    })
}
