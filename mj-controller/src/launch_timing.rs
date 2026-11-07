//! Wall-clock launch phases, including work outside timed target commands.

use std::borrow::Cow;
use std::time::Instant;

pub(crate) struct LaunchPhaseTimer<'a> {
    session_id: Cow<'a, str>,
    phase: &'static str,
    started: Instant,
}

impl<'a> LaunchPhaseTimer<'a> {
    pub(crate) fn new(session_id: &'a str, phase: &'static str) -> Self {
        tracing::info!(session_id, phase, "session launch phase started");
        Self {
            session_id: Cow::Borrowed(session_id),
            phase,
            started: Instant::now(),
        }
    }

    pub(crate) fn set_session_id(&mut self, session_id: &str) {
        self.session_id = Cow::Owned(session_id.to_owned());
    }
}

impl Drop for LaunchPhaseTimer<'_> {
    fn drop(&mut self) {
        tracing::info!(
            session_id = self.session_id.as_ref(),
            phase = self.phase,
            elapsed_ms = self.started.elapsed().as_millis(),
            "session launch phase ended"
        );
    }
}
