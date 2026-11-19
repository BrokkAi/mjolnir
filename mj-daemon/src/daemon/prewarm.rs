//! Reviewer prewarm: stage the shareable content of every profile a reviewer
//! may use on each remote host that runs a session, ahead of the review that
//! needs it, so a later `request_review` waits for its private bundle only.
//!
//! The daemon derives what to warm from durable session records, so a new
//! daemon starts from the same facts as the one it replaced and holds no
//! admission while it works: handoff cancels it, and the next daemon plans
//! again from the records. The receiver owns deduplication of overlapping
//! content across sessions, aliases and accounts; this only decides that a
//! host with a session it has not planned for yet is offered the content once.
use super::*;
use mj_core::state::SessionRecord;
use mj_core::targets::SshTarget;
use std::collections::{BTreeMap, BTreeSet};
use tokio_util::sync::CancellationToken;

/// One host's turn: the sessions that made it due and the account to warm.
#[derive(Debug, Clone, PartialEq, Eq)]
struct HostPlan {
    key: String,
    ssh: SshTarget,
    /// A session on the host whose record the transfer is staged from.
    representative: String,
    sessions: Vec<String>,
}

fn host_key(ssh: &SshTarget) -> String {
    format!("{} {}", ssh.destination, ssh.ssh_args.join(" "))
}

/// What one prewarm answered for: a session on a host, with the reviewer
/// profiles on offer. A session that moves, or a profile that is enabled,
/// makes a new key, which is the state change that plans a host again.
fn coverage_key(session: &str, host: &str, profiles: &str) -> String {
    format!("{session}|{host}|{profiles}")
}

/// The hosts that run a running, non-reviewer session no earlier plan has
/// covered. A session is covered once for its host and profile set, whether
/// its prewarm worked or failed: a failure is retried when that changes
/// (a new session, a move, a new profile), not on every revision.
fn plan_hosts(
    sessions: &mj_core::snapshot_map::SnapshotMap<String, SessionRecord>,
    reviewers: &BTreeSet<String>,
    covered: &BTreeSet<String>,
    profiles: &str,
    running: &BTreeSet<String>,
    locate: impl Fn(&SessionRecord) -> Option<SshTarget>,
) -> Vec<HostPlan> {
    let mut hosts: BTreeMap<String, HostPlan> = BTreeMap::new();
    for (id, session) in sessions.iter() {
        if session.state != mj_core::state::SessionState::Running || reviewers.contains(id) {
            continue;
        }
        let Some(ssh) = locate(session) else { continue };
        let key = host_key(&ssh);
        if running.contains(&key) || covered.contains(&coverage_key(id, &key, profiles)) {
            continue;
        }
        hosts
            .entry(key.clone())
            .or_insert_with(|| HostPlan {
                key,
                ssh,
                representative: id.clone(),
                sessions: Vec::new(),
            })
            .sessions
            .push(id.clone());
    }
    hosts.into_values().collect()
}

struct HostResult {
    plan: HostPlan,
    failures: Vec<(String, String)>,
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
    let host = plan.key.clone();
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
    HostResult { plan, failures }
}

pub(super) fn spawn(
    state: Arc<RuntimeState>,
    cancellation: CancellationToken,
) -> tokio::task::JoinHandle<()> {
    mj_core::runtime::spawn(async move {
        let mut revisions = state.revisions();
        let mut covered = BTreeSet::<String>::new();
        let mut running = BTreeSet::<String>::new();
        let mut last_failures = BTreeMap::<String, String>::new();
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
            let plans = plan_hosts(
                &records,
                &reviewers,
                &covered,
                &fingerprint,
                &running,
                |session| {
                    let locator = session.target.as_ref()?;
                    let backend =
                        mj_controller::controller::backend_locator(locator, session, &config)
                            .ok()?;
                    mj_controller::controller::shared_content_ssh(&backend).cloned()
                },
            );
            for plan in plans {
                covered.extend(
                    plan.sessions
                        .iter()
                        .map(|id| coverage_key(id, &plan.key, &fingerprint)),
                );
                let Some(session) = records.get(&plan.representative).cloned() else {
                    continue;
                };
                running.insert(plan.key.clone());
                let config = config.clone();
                let profiles = profiles.clone();
                let token = cancellation.child_token();
                tasks.spawn(prewarm_host(plan, session, config, profiles, token));
            }
            tokio::select! {
                biased;
                _ = cancellation.cancelled() => break,
                Some(joined) = tasks.join_next(), if !tasks.is_empty() => {
                    match joined {
                        Ok(result) => {
                            running.remove(&result.plan.key);
                            for (profile, error) in result.failures {
                                let key = format!("{}|{profile}", result.plan.key);
                                tracing::warn!(host = result.plan.key, profile, %error, "could not prewarm a reviewer profile");
                                if last_failures.get(&key) != Some(&error) {
                                    state.push_notice(
                                        &result.plan.representative,
                                        format!("Could not prepare reviewer profile {profile} on {}: {error}", result.plan.key),
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

    // Ten sessions starting on one host prewarm it once; a session that starts
    // later, or a daemon that restarts and plans from the same records, is
    // the only thing that makes a host due again.
    #[test]
    fn sessions_on_one_host_prewarm_it_once_and_a_restart_plans_from_the_records() {
        let mut sessions = mj_core::snapshot_map::SnapshotMap::new();
        for index in 0..10 {
            let id = format!("s{index}");
            sessions.insert(id.clone(), running_session(&id));
        }
        sessions.insert("elsewhere".into(), running_session("elsewhere"));
        let locate =
            |session: &SessionRecord| Some(ssh(if session.id == "elsewhere" { "b" } else { "a" }));
        let none = BTreeSet::new();
        let plans = plan_hosts(&sessions, &none, &none, "p", &none, locate);
        assert_eq!(plans.len(), 2);
        let host_a = plans
            .iter()
            .find(|plan| plan.ssh.destination == "a")
            .unwrap();
        assert_eq!(host_a.sessions.len(), 10);
        // Covered sessions plan nothing, and a host with a prewarm running is
        // not planned a second time while it runs.
        let covered: BTreeSet<String> = plans
            .iter()
            .flat_map(|p| p.sessions.iter().map(|id| coverage_key(id, &p.key, "p")))
            .collect();
        assert!(plan_hosts(&sessions, &none, &covered, "p", &none, locate).is_empty());
        sessions.insert("late".into(), running_session("late"));
        let running = BTreeSet::from([host_key(&ssh("a"))]);
        assert!(plan_hosts(&sessions, &none, &covered, "p", &running, locate).is_empty());
        let due = plan_hosts(&sessions, &none, &covered, "p", &none, locate);
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].sessions, vec!["late".to_owned()]);
        // A restarted daemon has covered nothing and plans every host again;
        // the receiver finds the content warm and sends no object bodies.
        assert_eq!(
            plan_hosts(&sessions, &none, &none, "p", &none, locate).len(),
            2
        );
        // A newly enabled profile, or a session that moved to another host,
        // is a new fact and makes its hosts due again.
        assert_eq!(
            plan_hosts(&sessions, &none, &covered, "p,q", &none, locate).len(),
            2
        );
        let moved = |session: &SessionRecord| Some(ssh(if session.id == "s0" { "c" } else { "a" }));
        let after_move = plan_hosts(&sessions, &none, &covered, "p", &none, moved);
        assert!(after_move.iter().any(|plan| plan.ssh.destination == "c"));
        // A reviewer never makes a host due.
        let reviewers = BTreeSet::from(["late".to_owned()]);
        assert!(plan_hosts(&sessions, &reviewers, &covered, "p", &none, locate).is_empty());
    }
}
