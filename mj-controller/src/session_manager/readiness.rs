//! The worker owns startup expiry. Only pre-deadline workers use the legacy clock.
use mj_core::relay::{HarnessPreparation, RelayOperationalState};
use std::time::Duration;
use tokio::time::Instant;

pub(super) const HARNESS_READINESS_TIMEOUT: Duration =
    mj_core::relay::NATIVE_SESSION_STARTUP_TIMEOUT;

#[derive(Debug, Clone)]
pub struct HarnessStartupFailure {
    pub(crate) cause: String,
    pub observed_updated_at: String,
}
impl HarnessStartupFailure {
    pub fn cause(&self) -> String {
        self.cause.clone()
    }
}
pub type HarnessFailureHandler = std::sync::Arc<
    dyn Fn(
            String,
            HarnessStartupFailure,
        )
            -> std::pin::Pin<Box<dyn std::future::Future<Output = anyhow::Result<bool>> + Send>>
        + Send
        + Sync,
>;

struct Waiting {
    since: Instant,
}
enum HarnessStartupWait {
    Legacy(Waiting),
    Started(Waiting),
    Applied,
}
#[derive(Default)]
pub(super) struct HarnessStartupWatch {
    waiting: Option<HarnessStartupWait>,
}
impl HarnessStartupWatch {
    /// Explicit failures are the worker's terminal decision. The daemon clock
    /// is used only when the negotiated worker version has no such decision.
    pub(super) fn observe_worker(
        &mut self,
        now: Instant,
        protocol: u32,
        operational: Option<&RelayOperationalState>,
    ) -> Option<String> {
        if matches!(self.waiting, Some(HarnessStartupWait::Applied)) {
            return None;
        }
        if let Some(HarnessPreparation::Failed { step, error, .. }) =
            operational.and_then(|state| state.harness_preparation.as_ref())
        {
            return Some(
                mj_session_types::HarnessPreparationFailure {
                    step: step.clone(),
                    error: error.clone(),
                }
                .to_string(),
            );
        }
        if protocol >= mj_core::relay::RELAY_WORKER_STARTUP_DEADLINE_PROTOCOL {
            self.waiting = None;
            return None;
        }
        self.observe(now, operational).map(|waited| format!(
            "the daemon did not observe a usable harness within {}s after hearing from the legacy worker; use Restart agent to retry in the existing environment",
            waited.as_secs(),
        ))
    }

    pub(super) fn lost_contact(&mut self) {
        self.waiting = None;
    }

    /// Called only after the guarded write was accepted inside the actor loop.
    pub(super) fn applied(&mut self) {
        self.waiting = Some(HarnessStartupWait::Applied);
    }

    /// Only the owning actor calls this after successful hello/sync, including
    /// unchanged syncs. None clears contact immediately, independently of the
    /// delayed display status. Never run a clock decision on a cached view.
    fn observe(
        &mut self,
        now: Instant,
        operational: Option<&RelayOperationalState>,
    ) -> Option<Duration> {
        let Some(operational) = operational else {
            self.waiting = None;
            return None;
        };
        if operational.native_session_is_ready()
            || matches!(
                operational.harness_preparation,
                Some(HarnessPreparation::Preparing { .. })
            )
        {
            self.waiting = None;
            return None;
        }
        let waiting = self
            .waiting
            .get_or_insert(HarnessStartupWait::Legacy(Waiting { since: now }));
        // Match the launch path: an observed harness start gets its own full
        // allowance, rather than spending the worker's earlier legacy wait.
        if matches!(
            operational.harness_preparation,
            Some(HarnessPreparation::Started)
        ) && matches!(waiting, HarnessStartupWait::Legacy(_))
        {
            *waiting = HarnessStartupWait::Started(Waiting { since: now });
        }
        let timer = match waiting {
            HarnessStartupWait::Legacy(timer) | HarnessStartupWait::Started(timer) => timer,
            HarnessStartupWait::Applied => return None,
        };
        let waited = now.saturating_duration_since(timer.since);
        if waited < HARNESS_READINESS_TIMEOUT {
            return None;
        }
        Some(waited)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn snapshot(ready: bool) -> RelayOperationalState {
        let mut state =
            mj_core::relay::RelaySnapshot::new("session".to_owned()).operational_state();
        state.native_session_id = ready.then(|| "native-session".to_owned());
        state
    }

    // Hard-won: 0353d626b review: stalled old workers cannot pass idle upgrade admission.
    #[test]
    fn only_legacy_workers_use_the_actor_startup_clock() {
        let mut legacy = HarnessStartupWatch::default();
        let mut current = HarnessStartupWatch::default();
        let protocol = mj_core::relay::RELAY_WORKER_STARTUP_DEADLINE_PROTOCOL;
        let start = Instant::now();
        let mut state = snapshot(false);
        assert!(
            legacy
                .observe_worker(start, protocol - 1, Some(&state))
                .is_none()
        );
        assert!(
            current
                .observe_worker(start, protocol, Some(&state))
                .is_none()
        );
        let expired = start + HARNESS_READINESS_TIMEOUT;
        assert!(
            legacy
                .observe_worker(expired, protocol - 1, Some(&state))
                .unwrap()
                .contains("Restart agent")
        );
        assert!(
            current
                .observe_worker(expired, protocol, Some(&state))
                .is_none()
        );
        state.harness_preparation = Some(HarnessPreparation::Failed {
            step: "session-open".into(),
            error: "worker deadline expired; Restart agent".into(),
            at_ms: 1,
        });
        assert!(
            current
                .observe_worker(expired, protocol, Some(&state))
                .unwrap()
                .contains("worker deadline expired")
        );
    }

    // Hard-won: b2b519c3f review: a rejected timeout write consumed the wait permanently.
    #[test]
    fn an_unready_harness_retries_rejected_writes_until_applied() {
        let mut watch = HarnessStartupWatch::default();
        let start = Instant::now();
        let state = snapshot(false);
        assert!(watch.observe(start, Some(&state)).is_none());
        assert!(
            watch
                .observe(
                    start + HARNESS_READINESS_TIMEOUT - Duration::from_secs(1),
                    Some(&state)
                )
                .is_none()
        );
        let timeout = watch
            .observe(start + HARNESS_READINESS_TIMEOUT, Some(&state))
            .unwrap();
        assert_eq!(timeout, HARNESS_READINESS_TIMEOUT);
        // A rejected write or store error leaves the wait eligible, but only
        // a later successful sync calls observe again.
        assert!(
            watch
                .observe(
                    start + HARNESS_READINESS_TIMEOUT + Duration::from_secs(1),
                    Some(&state)
                )
                .is_some()
        );
        watch.applied();
        assert!(
            watch
                .observe(start + HARNESS_READINESS_TIMEOUT * 2, Some(&state))
                .is_none()
        );
    }

    // Hard-won: 07d2 handoff: a recent unrelated record write armed the readiness timer and a fresh daemon failed a live session
    #[test]
    fn record_writes_cannot_arm_an_actor_that_has_not_heard_from_its_worker() {
        let mut watch = HarnessStartupWatch::default();
        let start = Instant::now();
        // Even a record just written by a failed Move is no input to this
        // owner: only a successful worker exchange can arm the deadline.
        assert!(watch.observe(start, None).is_none());
        assert!(
            watch
                .observe(start + HARNESS_READINESS_TIMEOUT * 2, None)
                .is_none()
        );
        let heard = start + HARNESS_READINESS_TIMEOUT * 3;
        let state = snapshot(false);
        assert!(watch.observe(heard, Some(&state)).is_none());
        assert!(
            watch
                .observe(
                    heard + HARNESS_READINESS_TIMEOUT - Duration::from_secs(1),
                    Some(&state)
                )
                .is_none()
        );
        assert!(
            watch
                .observe(heard + HARNESS_READINESS_TIMEOUT, Some(&state))
                .is_some()
        );
    }

    #[test]
    fn a_ready_harness_or_preparation_clears_the_wait() {
        let mut watch = HarnessStartupWatch::default();
        let start = Instant::now();
        let unready = snapshot(false);
        assert!(watch.observe(start, Some(&unready)).is_none());
        assert!(
            watch
                .observe(start + HARNESS_READINESS_TIMEOUT, Some(&snapshot(true)))
                .is_none()
        );
        let mut preparing = snapshot(false);
        preparing.harness_preparation = Some(HarnessPreparation::Preparing {
            step: "Downloading".to_owned(),
            since_ms: 0,
        });
        assert!(
            watch
                .observe(start + HARNESS_READINESS_TIMEOUT * 2, Some(&preparing))
                .is_none()
        );
        assert!(
            watch
                .observe(start + HARNESS_READINESS_TIMEOUT * 3, Some(&unready))
                .is_none()
        );
    }

    // Hard-won: 25b8a2fd3 review: the pre-harness wait consumed the harness startup allowance.
    #[test]
    fn a_harness_start_gets_a_fresh_deadline_once() {
        let mut watch = HarnessStartupWatch::default();
        let start = Instant::now();
        assert!(watch.observe(start, Some(&snapshot(false))).is_none());
        let mut started = snapshot(false);
        started.harness_preparation = Some(HarnessPreparation::Started);
        let harness_start = start + HARNESS_READINESS_TIMEOUT - Duration::from_secs(1);
        assert!(watch.observe(harness_start, Some(&started)).is_none());
        assert!(
            watch
                .observe(start + HARNESS_READINESS_TIMEOUT, Some(&started))
                .is_none()
        );
        assert!(
            watch
                .observe(
                    harness_start + HARNESS_READINESS_TIMEOUT - Duration::from_secs(1),
                    Some(&started)
                )
                .is_none()
        );
        assert_eq!(
            watch
                .observe(harness_start + HARNESS_READINESS_TIMEOUT, Some(&started))
                .unwrap(),
            HARNESS_READINESS_TIMEOUT
        );
    }

    // Hard-won: 25b8a2fd3 review: display status retained connected=true after relay contact failed.
    #[test]
    fn contact_loss_and_in_flight_sync_cannot_fail_a_live_harness() {
        let mut watch = HarnessStartupWatch::default();
        let start = Instant::now();
        assert!(watch.observe(start, Some(&snapshot(false))).is_none());
        // An in-flight exchange makes no clock decision. If it fails, the
        // actor clears the wait immediately, even while display says connected.
        assert!(
            watch
                .observe(start + HARNESS_READINESS_TIMEOUT * 2, None)
                .is_none()
        );
        let reconnected = start + HARNESS_READINESS_TIMEOUT * 3;
        assert!(watch.observe(reconnected, Some(&snapshot(false))).is_none());
        assert!(
            watch
                .observe(
                    reconnected + HARNESS_READINESS_TIMEOUT - Duration::from_secs(1),
                    Some(&snapshot(false))
                )
                .is_none()
        );
        assert!(
            watch
                .observe(
                    reconnected + HARNESS_READINESS_TIMEOUT,
                    Some(&snapshot(true))
                )
                .is_none()
        );
    }
}
