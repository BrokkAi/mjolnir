use super::*;

/// The exact result of a configuration command, once durably projected.
pub fn load_config_result(session_id: &str, command_id: &str) -> Result<Option<Option<String>>> {
    Ok(open_reader(&database_path())?
        .query_row(
            "SELECT error FROM api_config_results WHERE session_id = ?1 AND command_id = ?2",
            params![session_id, command_id],
            |row| row.get(0),
        )
        .optional()?)
}
