//! Reviewer prewarm: stage the shareable content of every profile a reviewer
//! may use on each remote host that runs a session, ahead of the review that
//! needs it, so a later `request_review` waits for its private bundle only.
//!
//! The daemon derives what to warm from durable session records, so a new
//! daemon starts from the same facts as the one it replaced and holds no
//! admission while it works: handoff cancels it, and the next daemon plans
//! again from the records. The receiver owns deduplication of overlapping
//! content across sessions, aliases and accounts; this only decides when a
//! host is offered the content again.
//!
//! A host is warm for one profile fingerprint and one launch-upload
//! generation. Sessions on the host do not matter: the store is the host's.
//! A real launch that had to send shared objects advances the generation (its
//! host lacked content a prewarm believed present), which makes the host due
//! once more. Only a prewarm that finished without failure counts, and a
//! failed one waits out a growing delay before the same facts plan it again.
use super::*;
use mj_core::state::SessionRecord;
use mj_core::targets::SshTarget;
use std::collections::{BTreeMap, BTreeSet};
use tokio::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;

/// A host as the receiver sees it: account and connection options, as given.
type HostKey = (String, Vec<String>);

fn host_key(ssh: &SshTarget) -> HostKey {
    (ssh.destination.clone(), ssh.ssh_args.clone())
}

fn host_label(ssh: &SshTarget) -> String {
    format!("{} {}", ssh.destination, ssh.ssh_args.join(" "))
}

/// Delays after the first, second, ... consecutive failure of one host; the
/// last repeats. This only keeps unrelated revisions from re-running a
/// prewarm that keeps failing.
const RETRY_DELAYS: [Duration; 6] = [
    Duration::from_secs(30),
    Duration::from_secs(60),
    Duration::from_secs(120),
    Duration::from_secs(240),
    Duration::from_secs(480),
    Duration::from_secs(900),
];

/// What a finished prewarm answered for: the profile set it offered and the
/// host's launch-upload generation when it began.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Coverage {
    fingerprint: String,
    generation: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Backoff {
    fingerprint: String,
    generation: u64,
    attempts: usize,
    retry_at: Instant,
}

/// Which hosts are warm, failing, or being warmed. Every decision about a host
/// is made here.
#[derive(Default)]
struct Ledger {
    covered: BTreeMap<HostKey, Coverage>,
    backoff: BTreeMap<HostKey, Backoff>,
    running: BTreeSet<HostKey>,
}

impl Ledger {
    fn is_due(&self, key: &HostKey, fingerprint: &str, generation: u64, now: Instant) -> bool {
        if self.running.contains(key) {
            return false;
        }
        let facts = |f: &str, g: u64| f == fingerprint && g == generation;
        if self
            .covered
            .get(key)
            .is_some_and(|c| facts(&c.fingerprint, c.generation))
        {
            return false;
        }
        // A changed fingerprint or generation is new information that ends
        // the wait.
        !self
            .backoff
            .get(key)
            .is_some_and(|b| facts(&b.fingerprint, b.generation) && now < b.retry_at)
    }

    /// Record how a prewarm ended. A cancelled one says nothing about the
    /// host.
    fn finish(&mut self, plan: &HostPlan, failed: bool, cancelled: bool, now: Instant) {
        self.running.remove(&plan.key);
        if cancelled {
            return;
        }
        if failed {
            let attempts = match self.backoff.get(&plan.key) {
                Some(b) if b.fingerprint == plan.fingerprint && b.generation == plan.generation => {
                    b.attempts + 1
                }
                _ => 1,
            };
            let delay = RETRY_DELAYS[(attempts - 1).min(RETRY_DELAYS.len() - 1)];
            self.covered.remove(&plan.key);
            self.backoff.insert(
                plan.key.clone(),
                Backoff {
                    fingerprint: plan.fingerprint.clone(),
                    generation: plan.generation,
                    attempts,
                    retry_at: now + delay,
                },
            );
            return;
        }
        self.backoff.remove(&plan.key);
        self.covered.insert(
            plan.key.clone(),
            Coverage {
                fingerprint: plan.fingerprint.clone(),
                generation: plan.generation,
            },
        );
    }

    /// The soonest moment a waiting host may be tried again. A wait that has
    /// already ended is the planner's to act on, not a reason to wake.
    fn next_retry(&self, now: Instant) -> Option<Instant> {
        self.backoff
            .values()
            .map(|b| b.retry_at)
            .filter(|at| *at > now)
            .min()
    }
}

/// One host's turn: the session whose record the transfer is staged from, and
/// the facts that made the host due.
#[derive(Debug, Clone, PartialEq, Eq)]
struct HostPlan {
    key: HostKey,
    label: String,
    ssh: SshTarget,
    /// A session on the host whose record the transfer is staged from.
    representative: String,
    fingerprint: String,
    /// The host's launch-upload generation, read before the prewarm begins so
    /// that an upload during it is never taken as covered.
    generation: u64,
}

/// The hosts that run a running, non-reviewer session and are not warm for
/// this fingerprint and generation, or waiting after a failure.
fn plan_hosts(
    sessions: &mj_core::snapshot_map::SnapshotMap<String, SessionRecord>,
    reviewers: &BTreeSet<String>,
    ledger: &Ledger,
    fingerprint: &str,
    now: Instant,
    generation: impl Fn(&SshTarget) -> u64,
    locate: impl Fn(&SessionRecord) -> Option<SshTarget>,
) -> Vec<HostPlan> {
    let mut hosts: BTreeMap<HostKey, HostPlan> = BTreeMap::new();
    for (id, session) in sessions.iter() {
        if session.state != mj_core::state::SessionState::Running || reviewers.contains(id) {
            continue;
        }
        let Some(ssh) = locate(session) else { continue };
        let key = host_key(&ssh);
        if hosts.contains_key(&key) {
            continue;
        }
        let generation = generation(&ssh);
        if !ledger.is_due(&key, fingerprint, generation, now) {
            continue;
        }
        hosts.insert(
            key.clone(),
            HostPlan {
                key,
                label: host_label(&ssh),
                ssh,
                representative: id.clone(),
                fingerprint: fingerprint.to_owned(),
                generation,
            },
        );
    }
    hosts.into_values().collect()
}

struct HostResult {
    plan: HostPlan,
    failures: Vec<(String, String)>,
    /// The daemon stopped the prewarm; it neither warmed nor failed the host.
    cancelled: bool,
}

/// One host's prewarm: every profile in turn, off the runtime. The process
/// executor stops on a flag, which the daemon's token sets; the flag's watcher
/// ends with the transfer.
async fn prewarm_host(
    plan: HostPlan,
    session: SessionRecord,
    config: mj_core::config::Config,
    profiles: Vec<String>,
    token: CancellationToken,
) -> HostResult {
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let finished = CancellationToken::new();
    let watcher = {
        let (stop, token, finished) = (stop.clone(), token.clone(), finished.clone());
        mj_core::runtime::spawn(async move {
            tokio::select! {
                _ = token.cancelled() => stop.store(true, std::sync::atomic::Ordering::SeqCst),
                _ = finished.cancelled() => {}
            }
        })
    };
    let host = plan.label.clone();
    let failures = mj_core::runtime::spawn_blocking(move || {
        let executor = mj_controller::targets::CancellableProcessExecutor::new(stop);
        let mut failures = Vec::new();
        for profile in profiles {
            // Staging a profile copies its trees and may fetch a catalog, work
            // the process executor cannot interrupt, so a cancelled daemon
            // stops before the next profile rather than after all of them.
            if executor.cancellation_requested() {
                break;
            }
            // A profile that cannot launch a reviewer has nothing to warm;
            // readiness reads its files, so it is decided here, off the runtime.
            let ready = config
                .enabled_profile(&profile)
                .is_some_and(|candidate| candidate.ensure_ready(&profile).is_ok());
            if !ready {
                continue;
            }
            match mj_controller::controller::prewarm_profile_content(
                &config, &session, &profile, &executor,
            ) {
                Ok(outcome) => {
                    tracing::info!(host, profile, ?outcome, "reviewer profile prewarm finished")
                }
                Err(error) if executor.cancellation_requested() => {
                    tracing::debug!(
                        host,
                        profile,
                        error = format!("{error:#}"),
                        "reviewer profile prewarm cancelled"
                    );
                    break;
                }
                Err(error) => failures.push((profile, format!("{error:#}"))),
            }
        }
        failures
    })
    .await
    .unwrap_or_else(|error| vec![(String::new(), format!("prewarm task failed: {error}"))]);
    finished.cancel();
    let _ = watcher.await;
    let cancelled = token.is_cancelled();
    HostResult {
        plan,
        failures,
        cancelled,
    }
}

pub(super) fn spawn(
    state: Arc<RuntimeState>,
    cancellation: CancellationToken,
) -> tokio::task::JoinHandle<()> {
    mj_core::runtime::spawn(async move {
        let mut revisions = state.revisions();
        let mut ledger = Ledger::default();
        let mut last_failures = BTreeMap::<(HostKey, String), String>::new();
        let mut tasks = tokio::task::JoinSet::<HostResult>::new();
        loop {
            let config = state.with_config(|config| config.clone());
            let records = state.session_records();
            let reviewers: BTreeSet<String> = {
                let owner = state.owner();
                owner
                    .controller()
                    .state
                    .subagents
                    .iter()
                    .filter(|(_, child)| child.kind == mj_core::subagent::SubagentKind::Reviewer)
                    .map(|(id, _)| id.clone())
                    .collect()
            };
            // Every profile a reviewer may be given: the enabled ones. This
            // depends on the configuration alone and does no I/O here; a
            // profile that is not ready is skipped where the work runs, off
            // the runtime. A configuration change is what makes the set new.
            let profiles: Vec<String> = config
                .enabled_profiles()
                .map(|(id, profile)| format!("{id}={}", profile.home.display()))
                .collect();
            let fingerprint = profiles.join(",");
            let profiles: Vec<String> = config
                .enabled_profiles()
                .map(|(id, _)| id.to_owned())
                .collect();
            let now = Instant::now();
            let plans = plan_hosts(
                &records,
                &reviewers,
                &ledger,
                &fingerprint,
                now,
                mj_controller::controller::profile_upload_generation,
                |session| {
                    let locator = session.target.as_ref()?;
                    let backend =
                        mj_controller::controller::backend_locator(locator, session, &config)
                            .ok()?;
                    mj_controller::controller::shared_content_ssh(&backend).cloned()
                },
            );
            for plan in plans {
                let Some(session) = records.get(&plan.representative).cloned() else {
                    continue;
                };
                ledger.running.insert(plan.key.clone());
                let config = config.clone();
                let profiles = profiles.clone();
                let token = cancellation.child_token();
                tasks.spawn(prewarm_host(plan, session, config, profiles, token));
            }
            let retry_at = ledger.next_retry(now);
            tokio::select! {
                biased;
                _ = cancellation.cancelled() => break,
                Some(joined) = tasks.join_next(), if !tasks.is_empty() => {
                    match joined {
                        Ok(result) => {
                            let HostResult { plan, failures, cancelled } = result;
                            let now = Instant::now();
                            let failed = !failures.is_empty();
                            ledger.finish(&plan, failed, cancelled, now);
                            if cancelled {
                                continue;
                            }
                            // A profile that no longer fails may fail again later
                            // with a notice of its own; a clean host has none.
                            last_failures.retain(|(key, profile), _| {
                                *key != plan.key || failures.iter().any(|(failed, _)| failed == profile)
                            });
                            for (profile, error) in failures {
                                tracing::warn!(host = plan.label, profile, %error, "could not prewarm a reviewer profile");
                                let key = (plan.key.clone(), profile.clone());
                                if last_failures.get(&key) != Some(&error) {
                                    state.push_notice(
                                        &plan.representative,
                                        format!("Could not prepare reviewer profile {profile} on {}: {error}", plan.label),
                                    );
                                    last_failures.insert(key, error);
                                }
                            }
                        }
                        Err(error) => {
                            tracing::error!(%error, "reviewer prewarm task stopped");
                        }
                    }
                }
                // A host that failed is tried again when its wait ends.
                _ = async {
                    match retry_at {
                        Some(at) => tokio::time::sleep_until(at).await,
                        None => std::future::pending().await,
                    }
                } => {}
                changed = revisions.changed() => {
                    if changed.is_err() {
                        break;
                    }
                }
            }
        }
        tasks.abort_all();
        while tasks.join_next().await.is_some() {}
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn running_session(id: &str) -> SessionRecord {
        let mut session = crate::daemon::tests::runtime_test_session(
            id,
            "workspace",
            mj_core::state::SessionState::Running,
        );
        session.id = id.into();
        session
    }

    fn ssh(host: &str) -> SshTarget {
        SshTarget {
            destination: host.into(),
            ssh_args: vec!["-p".into(), "22".into()],
        }
    }

    fn sessions_on(count: usize) -> mj_core::snapshot_map::SnapshotMap<String, SessionRecord> {
        let mut sessions = mj_core::snapshot_map::SnapshotMap::new();
        for index in 0..count {
            let id = format!("s{index}");
            sessions.insert(id.clone(), running_session(&id));
        }
        sessions
    }

    fn plan(
        sessions: &mj_core::snapshot_map::SnapshotMap<String, SessionRecord>,
        ledger: &Ledger,
        fingerprint: &str,
        generation: u64,
        now: Instant,
    ) -> Vec<HostPlan> {
        plan_hosts(
            sessions,
            &BTreeSet::new(),
            ledger,
            fingerprint,
            now,
            |_| generation,
            |session| Some(ssh(if session.id == "elsewhere" { "b" } else { "a" })),
        )
    }

    // Ten sessions on one host prewarm it once, and sessions that join a warm
    // host, or a host a reviewer runs on, do not plan it again. Only new
    // facts do: a launch that uploaded objects (a later generation), a new
    // profile, or a session on another host. A restarted daemon has no ledger
    // and plans from the records.
    #[test]
    fn a_warm_host_is_planned_again_only_by_new_facts() {
        let now = Instant::now();
        let mut sessions = sessions_on(10);
        sessions.insert("elsewhere".into(), running_session("elsewhere"));
        let mut ledger = Ledger::default();
        let plans = plan(&sessions, &ledger, "p", 0, now);
        assert_eq!(plans.len(), 2);
        let host_a = plans.iter().find(|p| p.ssh.destination == "a").unwrap();
        // While it runs a host is not planned again.
        ledger.running.insert(host_a.key.clone());
        assert_eq!(plan(&sessions, &ledger, "p", 0, now).len(), 1);
        for plan in &plans {
            ledger.running.insert(plan.key.clone());
            ledger.finish(plan, false, false, now);
        }
        assert!(plan(&sessions, &ledger, "p", 0, now).is_empty());
        sessions.insert("late".into(), running_session("late"));
        assert!(plan(&sessions, &ledger, "p", 0, now).is_empty());
        // A real launch that sent objects advanced the host's generation.
        let after_launch = plan(&sessions, &ledger, "p", 1, now);
        assert_eq!(after_launch.len(), 2);
        // A newly enabled profile is new as well.
        assert_eq!(plan(&sessions, &ledger, "p,q", 0, now).len(), 2);
        // An upload that happened while the prewarm ran is not covered by it:
        // the generation was read before the prewarm began.
        let started = plan(&sessions, &Ledger::default(), "p", 0, now);
        let mut racing = Ledger::default();
        for plan in &started {
            racing.running.insert(plan.key.clone());
            racing.finish(plan, false, false, now);
        }
        assert_eq!(plan(&sessions, &racing, "p", 1, now).len(), 2);
        // A restarted daemon plans every host again from the records.
        assert_eq!(plan(&sessions, &Ledger::default(), "p", 0, now).len(), 2);
        // A session that moved to a host nothing has warmed makes it due.
        let moved = |session: &SessionRecord| Some(ssh(if session.id == "s0" { "c" } else { "a" }));
        let after_move = plan_hosts(&sessions, &BTreeSet::new(), &ledger, "p", now, |_| 0, moved);
        assert_eq!(
            after_move
                .iter()
                .map(|plan| plan.ssh.destination.as_str())
                .collect::<Vec<_>>(),
            ["c"]
        );
        // A reviewer never makes a host due.
        let reviewers = BTreeSet::from(["s0".to_owned()]);
        let only_reviewer = sessions_on(1);
        assert!(
            plan_hosts(
                &only_reviewer,
                &reviewers,
                &Ledger::default(),
                "p",
                now,
                |_| 0,
                |_| Some(ssh("a"))
            )
            .is_empty()
        );
    }

    // A failed prewarm does not count as covering the host. Its host waits
    // 30 s, then longer after each failure up to 15 min, no matter how many
    // revisions arrive, until the wait ends or the fingerprint or generation
    // changes; a success ends it.
    #[test]
    fn a_failed_prewarm_backs_off_until_the_wait_or_the_facts_change() {
        let start = Instant::now();
        let sessions = sessions_on(1);
        let mut ledger = Ledger::default();
        let mut now = start;
        let mut waits = Vec::new();
        for _ in 0..8 {
            let plans = plan(&sessions, &ledger, "p", 0, now);
            assert_eq!(plans.len(), 1, "due at the end of each wait");
            ledger.running.insert(plans[0].key.clone());
            // Revisions while it runs plan nothing.
            assert!(plan(&sessions, &ledger, "p", 0, now).is_empty());
            ledger.finish(&plans[0], true, false, now);
            assert!(!ledger.covered.contains_key(&plans[0].key));
            let retry = ledger.next_retry(now).unwrap();
            waits.push(retry - now);
            // Not due a moment before the wait ends, whatever the revisions.
            assert!(plan(&sessions, &ledger, "p", 0, retry - Duration::from_millis(1)).is_empty());
            assert_eq!(ledger.next_retry(retry), None);
            now = retry;
        }
        let seconds: Vec<u64> = waits.iter().map(Duration::as_secs).collect();
        assert_eq!(seconds, [30, 60, 120, 240, 480, 900, 900, 900]);
        // New facts end the wait early and start counting from the first.
        let during = now;
        let plans = plan(&sessions, &ledger, "p", 0, during);
        ledger.running.insert(plans[0].key.clone());
        ledger.finish(&plans[0], true, false, during);
        assert!(plan(&sessions, &ledger, "p", 0, during).is_empty());
        assert_eq!(plan(&sessions, &ledger, "p", 1, during).len(), 1);
        assert_eq!(plan(&sessions, &ledger, "p,q", 0, during).len(), 1);
        let new_facts = plan(&sessions, &ledger, "p,q", 0, during).remove(0);
        ledger.finish(&new_facts, true, false, during);
        assert_eq!(
            ledger.next_retry(during).unwrap() - during,
            Duration::from_secs(30)
        );
        // A success warms the host and clears the wait.
        ledger.finish(&new_facts, false, false, during);
        assert!(ledger.backoff.is_empty());
        assert!(plan(&sessions, &ledger, "p,q", 0, during).is_empty());
    }

    // A prewarm the daemon cancelled neither warmed the host nor failed it.
    #[test]
    fn a_cancelled_prewarm_is_neither_covered_nor_backed_off() {
        let now = Instant::now();
        let sessions = sessions_on(1);
        let mut ledger = Ledger::default();
        let plans = plan(&sessions, &ledger, "p", 0, now);
        ledger.running.insert(plans[0].key.clone());
        ledger.finish(&plans[0], true, true, now);
        assert!(ledger.covered.is_empty() && ledger.backoff.is_empty());
        assert!(ledger.running.is_empty());
        assert_eq!(plan(&sessions, &ledger, "p", 0, now).len(), 1);
    }
}
