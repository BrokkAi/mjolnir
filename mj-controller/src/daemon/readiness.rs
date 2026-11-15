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
/// its own launch path about when a startup has gone on too long. The wait
/// begins only after this daemon hears from the worker and harness preparation
/// has ended. A record write says nothing about startup.
pub(super) const HARNESS_READINESS_TIMEOUT: Duration =
    crate::controller::NATIVE_SESSION_STARTUP_TIMEOUT;

/// What the daemon knows about one live session at one sweep.
pub(super) struct ReadinessObservation {
    pub(super) session_id: String,
    /// The worker state this daemon has actually received. No snapshot means
    /// startup is unknown, regardless of how recently the record was written.
    pub(super) operational:
        Option<mj_core::snapshot_map::SharedSnapshot<mj_core::relay::RelayOperationalState>>,
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
            "the daemon did not observe a usable harness within {}s after hearing from the worker; use Restart agent to retry in the existing environment",
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
    /// When this daemon first observed the worker waiting for its harness.
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
            let Some(operational) = observation.operational else {
                self.waiting.remove(&observation.session_id);
                continue;
            };
            // Like the launch path's NativeStartupDeadline, wait until harness
            // preparation ends. A restarted daemon starts with no knowledge of
            // this worker; neither a record write nor wall time can supply it.
            if operational.native_session_is_ready()
                || matches!(
                    operational.harness_preparation,
                    Some(mj_core::relay::HarnessPreparation::Preparing { .. })
                )
            {
                self.waiting.remove(&observation.session_id);
                continue;
            }
            let waiting = self
                .waiting
                .entry(observation.session_id.clone())
                .or_insert(Waiting {
                    since: now,
                    reported: false,
                });
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
        let mut operational =
            mj_core::relay::RelaySnapshot::new(session_id.to_owned()).operational_state();
        operational.native_session_id = harness_ready.then(|| "native-session".to_owned());
        ReadinessObservation {
            session_id: session_id.to_owned(),
            operational: Some(operational.into()),
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
                "the daemon did not observe a usable harness within {}s",
                HARNESS_READINESS_TIMEOUT.as_secs()
            )),
            "{}",
            unready[0].cause()
        );
        assert!(unready[0].cause().contains("use Restart agent"));
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

    // Hard-won: 07d2 handoff: a recent unrelated record write armed the readiness timer and a fresh daemon failed a live session
    #[test]
    fn a_recent_record_write_cannot_arm_a_new_daemon_without_a_worker_snapshot() {
        let mut watch = HarnessReadinessWatch::default();
        let start = Instant::now();
        // A failed Move just wrote this timestamp. No worker snapshot has
        // arrived at this daemon, so there is no evidence of harness startup.
        let recent_write = chrono::Utc::now().to_rfc3339();
        for elapsed in [Duration::ZERO, HARNESS_READINESS_TIMEOUT * 2] {
            assert!(
                watch
                    .observe(
                        start + elapsed,
                        vec![ReadinessObservation {
                            operational: None,
                            updated_at: recent_write.clone(),
                            ..observation("session-1", false)
                        }]
                    )
                    .is_empty()
            );
        }
        let heard_worker = start + HARNESS_READINESS_TIMEOUT * 2;
        assert!(
            watch
                .observe(heard_worker, vec![observation("session-1", false)])
                .is_empty()
        );
        assert!(
            watch
                .observe(
                    heard_worker + HARNESS_READINESS_TIMEOUT - Duration::from_secs(1),
                    vec![observation("session-1", false)],
                )
                .is_empty(),
            "the full wait starts only after hearing from the worker"
        );
        let unready = watch.observe(
            heard_worker + HARNESS_READINESS_TIMEOUT,
            vec![observation("session-1", false)],
        );
        assert_eq!(unready.len(), 1);
        assert_eq!(unready[0].waited, HARNESS_READINESS_TIMEOUT);
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
        // Worker preparation resets the startup wait, as in the launch path.
        let mut preparing = observation("session-1", false);
        preparing.operational.as_mut().unwrap().harness_preparation =
            Some(mj_core::relay::HarnessPreparation::Preparing {
                step: "Downloading".to_owned(),
                since_ms: 0,
            });
        assert!(
            watch
                .observe(start + HARNESS_READINESS_TIMEOUT, vec![preparing])
                .is_empty()
        );
        assert!(
            watch
                .observe(
                    start + HARNESS_READINESS_TIMEOUT + Duration::from_secs(10),
                    vec![observation("session-1", false)],
                )
                .is_empty()
        );
        // A lifecycle operation owns the session, so it is not observed.
        assert!(
            watch
                .observe(
                    start + HARNESS_READINESS_TIMEOUT + Duration::from_secs(30),
                    Vec::new()
                )
                .is_empty()
        );
        assert!(
            watch
                .observe(
                    start + HARNESS_READINESS_TIMEOUT * 3,
                    vec![observation("session-1", false)],
                )
                .is_empty(),
            "the wait restarts once the session is the daemon's to watch again"
        );
    }
}
