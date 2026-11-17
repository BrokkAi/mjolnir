use super::*;

#[cfg(test)]
thread_local! {
    static POLLABILITY_VISITS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
pub(crate) fn take_pollability_visits() -> usize {
    POLLABILITY_VISITS.with(|visits| visits.replace(0))
}

/// `is_active` means visible on the active dashboard, not necessarily backed
/// by a live target. A recoverable error stays visible so the user can resume
/// its checkpoint, but its failed target must not keep reconnecting or being
/// sampled. `Destroying` also stays visible, but its verified close has
/// permanently handed the target to cleanup, even after a cleanup task fails.
///
/// `Provisioning` is excluded for the same reason `credential_sync_targets`
/// excludes it, and for a sharper one: a session gets its `target` as soon as
/// the target itself exists, which is *before* its worker binary has been
/// copied into place. Polling that window means running `execve` on a file
/// `cp` still holds open for writing, which fails with `ETXTBSY` and leaves
/// the session recorded as unreachable. Provisioning connects to its own
/// worker when it is ready and then marks the session `Running`, which is when
/// there is something here to poll.
///
/// `Parked` is excluded because a parked sub-agent's worker was stopped on
/// purpose: polling it would find a dead worker, and the session manager's
/// recovery would start it again. This predicate is what keeps the session
/// manager, worker recovery and resource sampling away from parked children.
pub fn session_target_is_pollable(session: &mj_core::state::SessionRecord) -> bool {
    #[cfg(test)]
    POLLABILITY_VISITS.with(|visits| visits.set(visits.get() + 1));
    session.state.is_active()
        && !matches!(
            session.state,
            SessionState::Error
                | SessionState::StartupCleanup
                | SessionState::Provisioning
                | SessionState::Destroying
                | SessionState::Parked
        )
        && session.target.is_some()
}
