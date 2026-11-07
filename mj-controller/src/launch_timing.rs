//! Wall-clock launch phases, including work outside timed target commands.

use std::time::Instant;

pub(crate) struct LaunchPhaseTimer<'a> {
    session_id: &'a str,
    phase: &'static str,
    started: Instant,
}

impl<'a> LaunchPhaseTimer<'a> {
    pub(crate) fn new(session_id: &'a str, phase: &'static str) -> Self {
        tracing::info!(session_id, phase, "session launch phase started");
        Self {
            session_id,
            phase,
            started: Instant::now(),
        }
    }
}

impl Drop for LaunchPhaseTimer<'_> {
    fn drop(&mut self) {
        tracing::info!(
            session_id = self.session_id,
            phase = self.phase,
            elapsed_ms = self.started.elapsed().as_millis(),
            "session launch phase ended"
        );
    }
}
