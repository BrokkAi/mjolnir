//! Compatibility checks and notices for same-target in-place Move.

use anyhow::{Result, bail};
use mj_core::relay::RELAY_STAGED_IN_PLACE_RELOAD_PROTOCOL;

/// User-facing refusal when a manual same-target Move targets a worker that
/// predates Relay Protocol 91 (`RELAY_STAGED_IN_PLACE_RELOAD_PROTOCOL`).
pub fn pre_release_worker_manual_refusal(session_id: &str) -> String {
    format!(
        "this session's worker predates in-place restarts. Restart the agent first; \
         that restarts its worker and stops every process it is running. Then move. \
         (Run `mj restart --session {session_id}` or choose Restart agent in the UI.)"
    )
}

/// Notice for automatic quota Move when the worker predates Relay Protocol 91.
pub fn pre_release_worker_quota_notice(session_id: &str) -> String {
    format!(
        "automatic quota Move skipped for {session_id}: this session's worker predates \
         in-place restarts (relay protocol {RELAY_STAGED_IN_PLACE_RELOAD_PROTOCOL} required). \
         Waiting for quota reset, or run `mj restart --session {session_id}` first."
    )
}

/// Format a deferred environment change notice for a same-target Move.
pub fn deferred_environment_change_notice(change: &str) -> String {
    format!("{change} takes effect when this session's environment is next recreated")
}

/// Validate that a worker supports Slice 5A in-place moves.
pub fn ensure_worker_supports_in_place_move(
    session_id: &str,
    worker_protocol: u32,
    is_automatic_quota: bool,
) -> Result<()> {
    if worker_protocol >= RELAY_STAGED_IN_PLACE_RELOAD_PROTOCOL {
        return Ok(());
    }
    if is_automatic_quota {
        bail!("{}", pre_release_worker_quota_notice(session_id));
    }
    bail!("{}", pre_release_worker_manual_refusal(session_id));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pre_release_worker_refuses_manual_move_naming_restart_command() {
        assert!(ensure_worker_supports_in_place_move("sess-42", 91, false).is_ok());
        let err = ensure_worker_supports_in_place_move("sess-42", 90, false).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("mj restart --session sess-42"), "{msg}");
        assert!(msg.contains("Restart agent"), "{msg}");
    }

    #[test]
    fn pre_release_worker_skips_automatic_quota_move_with_notice() {
        let err = ensure_worker_supports_in_place_move("sess-42", 90, true).unwrap_err();
        assert!(err.to_string().contains("Waiting for quota reset"));
    }
}
