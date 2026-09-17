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

pub fn load_profile_config_cache(
    profile: &str,
    model: &str,
    fingerprint: &str,
) -> Result<Option<String>> {
    load_profile_config_cache_from(&database_path(), profile, model, fingerprint)
}

pub(crate) fn load_profile_config_cache_from(
    path: &Path,
    profile: &str,
    model: &str,
    fingerprint: &str,
) -> Result<Option<String>> {
    Ok(open_reader(path)?.query_row(
        "SELECT body FROM profile_config_cache WHERE profile = ?1 AND model = ?2 AND fingerprint = ?3 AND observed_at > ?4",
        params![profile, model, fingerprint, Utc::now().timestamp() - 86400], |row| row.get(0),
    ).optional()?)
}

pub fn save_profile_config_cache(
    profile: String,
    model: String,
    fingerprint: String,
    body: String,
) -> Result<()> {
    submit_database_write("save profile configuration cache", move |connection| {
        save_profile_config_cache_with(connection, &profile, &model, &fingerprint, &body)
    })
}

/// Write one cache row into the store at `path`. The queued writer behind
/// [`save_profile_config_cache`] serves the live store; this names a store
/// directly so a caller can use an isolated one.
#[cfg(test)]
pub(crate) fn save_profile_config_cache_at(
    path: &Path,
    profile: &str,
    model: &str,
    fingerprint: &str,
    body: &str,
) -> Result<()> {
    save_profile_config_cache_with(&open(path)?, profile, model, fingerprint, body)
}

pub(super) fn save_profile_config_cache_with(
    connection: &Connection,
    profile: &str,
    model: &str,
    fingerprint: &str,
    body: &str,
) -> Result<()> {
    connection.execute("INSERT OR REPLACE INTO profile_config_cache(profile, model, fingerprint, observed_at, body) VALUES (?1, ?2, ?3, ?4, ?5)", params![profile, model, fingerprint, Utc::now().timestamp(), body])?;
    Ok(())
}
