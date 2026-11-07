//! The AWS profile the controller staged into a session.
//!
//! The session's AWS configuration lives in a home the session can write, so
//! the profile it names cannot be trusted as the selection: a session that
//! renames its profile section would have the sync export whichever host
//! profile it named. The controller keeps the selection itself, here, written
//! by the one step that stages the configuration and read by everything that
//! mints or exports for the session.

use super::*;

/// Record the profile just staged into the session. A re-stage is a new
/// selection, so this replaces any earlier one.
pub(crate) fn set_session_aws_profile(session_id: &str, profile: &str) -> Result<()> {
    let session_id = session_id.to_owned();
    let profile = profile.to_owned();
    submit_database_write("set_session_aws_profile", move |connection| {
        write_session_aws_profile(connection, &session_id, &profile, true).map(|_| ())
    })
}

/// Record the profile a session staged by an older daemon was staged with,
/// unless a selection exists already. Returns the selection now recorded.
pub(crate) fn backfill_session_aws_profile(session_id: &str, profile: &str) -> Result<String> {
    let session_id = session_id.to_owned();
    let profile = profile.to_owned();
    submit_database_write("backfill_session_aws_profile", move |connection| {
        write_session_aws_profile(connection, &session_id, &profile, false)
    })
}

pub(super) fn write_session_aws_profile(
    connection: &Connection,
    session_id: &str,
    profile: &str,
    replace: bool,
) -> Result<String> {
    ensure!(!profile.is_empty(), "an AWS profile name cannot be empty");
    let conflict = if replace {
        "DO UPDATE SET aws_profile = excluded.aws_profile,
                       recorded_at_ms = excluded.recorded_at_ms"
    } else {
        "DO NOTHING"
    };
    connection.execute(
        &format!(
            "INSERT INTO session_aws_selections(session_id, aws_profile, recorded_at_ms)
             VALUES (?1, ?2, ?3)
             ON CONFLICT(session_id) {conflict}"
        ),
        params![session_id, profile, Utc::now().timestamp_millis()],
    )?;
    connection
        .query_row(
            "SELECT aws_profile FROM session_aws_selections WHERE session_id = ?1",
            [session_id],
            |row| row.get(0),
        )
        .context("read the recorded AWS profile")
}

/// The profile recorded for the session, if any.
pub(crate) fn session_aws_profile(session_id: &str) -> Result<Option<String>> {
    Ok(open_reader(&database_path())?
        .query_row(
            "SELECT aws_profile FROM session_aws_selections WHERE session_id = ?1",
            [session_id],
            |row| row.get(0),
        )
        .optional()?)
}
