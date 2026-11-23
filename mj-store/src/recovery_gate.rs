//! Per-session coordination between foreground operations and background
//! recovery. The gate itself lives in `mj-core`; this module adds the check a
//! queued placement makes against the store once it is admitted.
pub use mj_core::recovery_gate::*;

/// Validate a queued placement after admission. A lifecycle may have finished
/// between the observation and this attempt, so the old target cannot be used.
pub fn current_background_session(
    observed: &mj_core::state::SessionRecord,
) -> anyhow::Result<Option<mj_core::state::SessionRecord>> {
    if crate::database::container_upgrade_owns_target(&observed.id)? {
        return Ok(None);
    }
    let current = crate::database::read_durable_session_record(&observed.id)?;
    Ok(current.filter(|current| {
        current.state == mj_core::state::SessionState::Running
            && current.target == observed.target
            && current.native_session_id == observed.native_session_id
            && current.harness_kind == observed.harness_kind
            && current.last_profile == observed.last_profile
    }))
}
