//! The words for a session whose lifecycle operation died with the daemon.

use mj_core::state::{SessionRecord, SessionState};

/// Why a record left in an in-flight lifecycle state has nobody to finish it,
/// in words the user reads in `mj sessions` and the TUI.
///
/// Every in-flight state needs an owner that will complete it. A durable move
/// intent owns its session, `pollers::is_interrupted_close` owns a close or teardown
/// that still holds its target, and
/// `database::recover_interrupted_checkpointing_sessions` returns an
/// interrupted `Checkpointing` record to `Running` before the controller
/// loads. What is left is a record whose operation died with the process, and
/// it has to say so instead of waiting forever.
///
/// `None` means the state needs no reconciliation; callers exclude the owned
/// sessions before asking.
pub fn interrupted_lifecycle_cause(session: &SessionRecord) -> Option<String> {
    match session.state {
        // Provisioning has no durable operation behind it. Whatever the dead
        // provision created is not named by this record, so the resource is
        // recovered through `mj recover scan`, which can see it again once the
        // record is no longer in flight.
        SessionState::Provisioning => Some(
            "the daemon stopped while this session was provisioning; anything it created \
             is offered by `mj recover scan`"
                .to_owned(),
        ),
        // An interrupted close or teardown that still holds its target is
        // resumed rather than failed, so only the target-less residue reaches
        // here: there is nothing left to tear down, and no relay through which
        // to finish the close the record claims.
        SessionState::Closing => Some(
            "the daemon stopped while this session was closing, and it has no target left \
             to close"
                .to_owned(),
        ),
        SessionState::Destroying => Some(
            "the daemon stopped while this session was being torn down, and it has no \
             target left to remove"
                .to_owned(),
        ),
        // A parked sub-agent is settled: its worker was stopped on purpose and
        // it waits for its parent's next `send_message`.
        SessionState::StartupCleanup
        | SessionState::Checkpointing
        | SessionState::Running
        | SessionState::Disconnected
        | SessionState::Parked
        | SessionState::Stopped
        | SessionState::Lost
        | SessionState::Error
        | SessionState::DestroyedWithDataLoss => None,
    }
}
