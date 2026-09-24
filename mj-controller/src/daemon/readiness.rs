//! The bounded wait for a live session's harness to become usable.
//!
//! A session whose container and worker both reported ready still depends on
//! the harness process inside it opening its ACP session and advertising what
//! it offers. Until the daemon sees that, the session takes no prompt and no
//! configuration change, and nothing else in the daemon ever gives up on it:
//! the relay actor reconnects forever and the durable record keeps saying
//! `running` (#1090). This is the timer that ends that wait.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

/// How long a session that has just reported ready may go without the daemon
/// seeing a usable harness.
///
/// This is the same ceiling `mj` already gives a harness to open its session
/// while a create or resume waits on it, so the daemon cannot disagree with
/// its own launch path about when a startup has gone on too long. Everything
/// the wait is for has already happened by the time a record says `running`,
/// so this is a generous bound, deliberately: failing a session that would
/// have answered is worse than the stall it replaces.
pub(super) const HARNESS_READINESS_TIMEOUT: Duration =
    crate::controller::NATIVE_SESSION_STARTUP_TIMEOUT;

/// What the daemon knows about one live session at one sweep.
pub(super) struct ReadinessObservation {
    pub(super) session_id: String,
    /// Whether the daemon holds a relay snapshot whose worker reports a usable
    /// native session.
    ///
    /// Empty `config_options` alone is deliberately not the test: a harness is
    /// allowed to offer no settings, and failing those sessions would be worse
    /// than the stall this timer ends. A worker that has opened its ACP
    /// session has published its configuration with it.
    pub(super) harness_ready: bool,
    /// How long ago the durable record was last written, when that can be
    /// read. A record that has not been written since it started running
    /// carries the moment the session reported ready, which is what the wait
    /// is measured from.
    pub(super) record_age: Option<Duration>,
    /// The durable record's `updated_at`, so the write that fails the session
    /// can refuse to act on a record that has moved since.
    pub(super) updated_at: String,
    /// What the session manager last reported about this session, when it
    /// reported a failure at all. A silent stall carries none.
    pub(super) detail: Option<String>,
}

/// A live session whose harness never became usable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct UnreadySession {
    pub(super) session_id: String,
    pub(super) observed_updated_at: String,
    pub(super) waited: Duration,
    pub(super) detail: Option<String>,
}

impl UnreadySession {
    /// The sentence stored on the record and shown to the person.
    pub(super) fn cause(&self) -> String {
        let mut cause = format!(
            "the harness never advertised its configuration within {}s, so this session cannot be used; destroy it and create a replacement",
            self.waited.as_secs()
        );
        if let Some(detail) = &self.detail {
            cause.push_str("; the daemon last saw: ");
            cause.push_str(detail);
        }
        cause
    }
}

struct Waiting {
    /// When the session reported ready, as far as the daemon can tell.
    since: Instant,
    reported: bool,
}

/// Which live sessions are still waiting for their harness, and for how long.
///
/// Held by the daemon runtime and driven by its background sweep, never by a
/// request or a render.
#[derive(Default)]
pub(super) struct HarnessReadinessWatch {
    waiting: BTreeMap<String, Waiting>,
}

impl HarnessReadinessWatch {
    /// Fold one sweep into the wait and report the sessions that have run out
    /// of it. Each session is reported once per wait; a session that becomes
    /// ready, ends, or starts a lifecycle operation drops out of the wait
    /// entirely.
    pub(super) fn observe(
        &mut self,
        now: Instant,
        observations: Vec<ReadinessObservation>,
    ) -> Vec<UnreadySession> {
        self.waiting.retain(|session_id, _| {
            observations
                .iter()
                .any(|observation| &observation.session_id == session_id)
        });
        let mut unready = Vec::new();
        for observation in observations {
            if observation.harness_ready {
                self.waiting.remove(&observation.session_id);
                continue;
            }
            if !self.waiting.contains_key(&observation.session_id) {
                // Only a session that reported ready recently is armed. A
                // daemon that has just restarted onto sessions it did not
                // start must not fail them because their worker is behind an
                // outage it has been told nothing about.
                let Some(age) = observation
                    .record_age
                    .filter(|age| *age < HARNESS_READINESS_TIMEOUT)
                else {
                    continue;
                };
                self.waiting.insert(
                    observation.session_id.clone(),
                    Waiting {
                        since: now.checked_sub(age).unwrap_or(now),
                        reported: false,
                    },
                );
            }
            let waiting = self
                .waiting
                .get_mut(&observation.session_id)
                .expect("the session was armed above");
            let waited = now.saturating_duration_since(waiting.since);
            if waiting.reported || waited < HARNESS_READINESS_TIMEOUT {
                continue;
            }
            waiting.reported = true;
            unready.push(UnreadySession {
                session_id: observation.session_id,
                observed_updated_at: observation.updated_at,
                waited,
                detail: observation.detail,
            });
        }
        unready
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn observation(session_id: &str, harness_ready: bool) -> ReadinessObservation {
        ReadinessObservation {
            session_id: session_id.to_owned(),
            harness_ready,
            record_age: Some(Duration::ZERO),
            updated_at: "2026-09-18T15:28:35Z".to_owned(),
            detail: None,
        }
    }

    #[test]
    fn a_session_that_never_advertises_its_harness_fails_once_the_wait_runs_out() {
        let mut watch = HarnessReadinessWatch::default();
        let start = Instant::now();

        assert!(
            watch
                .observe(start, vec![observation("session-1", false)])
                .is_empty(),
            "a session that has just reported ready is still waiting"
        );
        assert!(
            watch
                .observe(
                    start + HARNESS_READINESS_TIMEOUT - Duration::from_secs(1),
                    vec![observation("session-1", false)],
                )
                .is_empty(),
            "the wait is not cut short"
        );

        let unready = watch.observe(
            start + HARNESS_READINESS_TIMEOUT,
            vec![ReadinessObservation {
                detail: Some("relay worker is unreachable".to_owned()),
                ..observation("session-1", false)
            }],
        );
        assert_eq!(
            unready,
            vec![UnreadySession {
                session_id: "session-1".to_owned(),
                observed_updated_at: "2026-09-18T15:28:35Z".to_owned(),
                waited: HARNESS_READINESS_TIMEOUT,
                detail: Some("relay worker is unreachable".to_owned()),
            }]
        );
        assert!(
            unready[0].cause().starts_with(&format!(
                "the harness never advertised its configuration within {}s",
                HARNESS_READINESS_TIMEOUT.as_secs()
            )),
            "{}",
            unready[0].cause()
        );
        assert!(
            unready[0].cause().contains("relay worker is unreachable"),
            "the reason carries what the daemon last saw"
        );

        assert!(
            watch
                .observe(
                    start + HARNESS_READINESS_TIMEOUT + Duration::from_secs(30),
                    vec![observation("session-1", false)],
                )
                .is_empty(),
            "one failure per wait, however long the record takes to change"
        );
    }

    #[test]
    fn a_session_whose_harness_arrives_in_time_is_never_failed() {
        let mut watch = HarnessReadinessWatch::default();
        let start = Instant::now();

        assert!(
            watch
                .observe(start, vec![observation("session-1", false)])
                .is_empty()
        );
        assert!(
            watch
                .observe(
                    start + Duration::from_secs(20),
                    vec![observation("session-1", true)],
                )
                .is_empty()
        );
        assert!(
            watch
                .observe(
                    start + HARNESS_READINESS_TIMEOUT + Duration::from_secs(60),
                    vec![observation("session-1", true)],
                )
                .is_empty(),
            "a harness that answered is never failed for the wait it ended"
        );
    }

    #[test]
    fn the_wait_is_measured_from_the_moment_the_session_reported_ready() {
        let mut watch = HarnessReadinessWatch::default();
        let start = Instant::now();
        let already_waited = HARNESS_READINESS_TIMEOUT - Duration::from_secs(60);

        assert!(
            watch
                .observe(
                    start,
                    vec![ReadinessObservation {
                        record_age: Some(already_waited),
                        ..observation("session-1", false)
                    }],
                )
                .is_empty()
        );

        let unready = watch.observe(
            start + Duration::from_secs(60),
            vec![ReadinessObservation {
                record_age: Some(already_waited + Duration::from_secs(60)),
                ..observation("session-1", false)
            }],
        );
        assert_eq!(unready.len(), 1, "the record's own clock starts the wait");
        assert_eq!(unready[0].waited, HARNESS_READINESS_TIMEOUT);
    }

    #[test]
    fn a_session_the_daemon_did_not_see_start_is_left_alone() {
        let mut watch = HarnessReadinessWatch::default();
        let start = Instant::now();
        let stale = ReadinessObservation {
            record_age: Some(HARNESS_READINESS_TIMEOUT + Duration::from_secs(1)),
            ..observation("session-1", false)
        };

        assert!(watch.observe(start, vec![stale]).is_empty());
        assert!(
            watch
                .observe(
                    start + HARNESS_READINESS_TIMEOUT * 2,
                    vec![ReadinessObservation {
                        record_age: Some(HARNESS_READINESS_TIMEOUT * 3),
                        ..observation("session-1", false)
                    }],
                )
                .is_empty(),
            "a session that was already running before the daemon saw it keeps reconnecting"
        );
    }

    #[test]
    fn a_session_that_leaves_the_sweep_starts_its_wait_again() {
        let mut watch = HarnessReadinessWatch::default();
        let start = Instant::now();

        assert!(
            watch
                .observe(start, vec![observation("session-1", false)])
                .is_empty()
        );
        // A lifecycle operation owns the session, so it is not observed.
        assert!(
            watch
                .observe(start + Duration::from_secs(30), Vec::new())
                .is_empty()
        );
        assert!(
            watch
                .observe(
                    start + HARNESS_READINESS_TIMEOUT,
                    vec![observation("session-1", false)],
                )
                .is_empty(),
            "the wait restarts once the session is the daemon's to watch again"
        );
    }
}
