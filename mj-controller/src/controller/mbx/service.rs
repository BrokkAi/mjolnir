//! Reconcile machine cache policy without tying it to session launches.
use super::*;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{
    Arc, Mutex, PoisonError,
    atomic::{AtomicBool, Ordering},
};

#[derive(Default)]
pub(crate) struct InstallWake {
    hosts: Mutex<BTreeSet<String>>,
    notify: tokio::sync::Notify,
}

impl InstallWake {
    pub(crate) fn wake_host(&self, host_key: String) {
        self.hosts
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(host_key);
        self.notify.notify_one();
    }

    fn take_hosts(&self) -> BTreeSet<String> {
        std::mem::take(&mut *self.hosts.lock().unwrap_or_else(PoisonError::into_inner))
    }

    fn notified(&self) -> tokio::sync::futures::Notified<'_> {
        self.notify.notified()
    }
}

#[derive(Default)]
struct ReconcileState {
    active: BTreeSet<String>,
    last_desired: BTreeMap<String, String>,
    stale_active: BTreeSet<String>,
}

impl ReconcileState {
    fn invalidate_hosts(&mut self, hosts: impl IntoIterator<Item = String>) {
        for key in hosts {
            self.last_desired.remove(&key);
            if self.active.contains(&key) {
                self.stale_active.insert(key);
            }
        }
    }

    fn should_start(&self, key: &str, desired: &str) -> bool {
        !self.active.contains(key)
            && self.last_desired.get(key).map(String::as_str) != Some(desired)
    }

    fn mark_started(&mut self, key: String) {
        self.active.insert(key);
    }

    fn mark_finished(&mut self, key: String, desired: String) {
        self.active.remove(&key);
        if !self.stale_active.remove(&key) {
            self.last_desired.insert(key, desired);
        }
    }
}

/// Applications are atomic and reconstructible from configuration, so they do
/// not hold daemon handoff admission. Shutdown cancels and joins every attempt.
pub(crate) async fn run(
    state: Arc<crate::daemon::RuntimeState>,
    stop: tokio_util::sync::CancellationToken,
) -> Result<()> {
    let cancelled = Arc::new(AtomicBool::new(false));
    let mut jobs = tokio::task::JoinSet::new();
    let mut reconcile = ReconcileState::default();
    let mut revisions = state.revisions();
    let outcome = async {
        if !stop.is_cancelled() {
            reconcile_pass(&state, &cancelled, &mut jobs, &mut reconcile);
        }
        loop {
            tokio::select! {
                biased;
                _ = stop.cancelled() => break,
                completed = jobs.join_next(), if !jobs.is_empty() => {
                    let (key, desired, result) = completed
                        .context("missing build cache application")?
                        .context("build cache supervisor failed")?;
                    if let Err(error) = result {
                        tracing::warn!(host = %key, "machine build cache configuration failed: {error:#}");
                        state.push_notice(
                            "",
                            format!(
                                "Build cache settings on {key} could not be applied: {error:#}. They will be applied with the next change to this machine's build cache settings, the next session launch on it, or an mbx install."
                            ),
                        );
                    }
                    reconcile.mark_finished(key, desired);
                    reconcile_pass(&state, &cancelled, &mut jobs, &mut reconcile);
                }
                changed = revisions.changed() => {
                    if changed.is_err() {
                        break;
                    }
                    reconcile_pass(&state, &cancelled, &mut jobs, &mut reconcile);
                }
                _ = state.mbx_install_wake().notified() => {
                    reconcile_pass(&state, &cancelled, &mut jobs, &mut reconcile);
                }
            }
        }
        Ok(())
    }
    .await;
    cancelled.store(true, Ordering::Release);
    while let Some(result) = jobs.join_next().await {
        match result {
            Ok((host, _, Err(error))) => {
                tracing::debug!(%host, "cancelled cache application: {error:#}")
            }
            Err(error) => tracing::warn!(%error, "cache application failed during shutdown"),
            _ => {}
        }
    }
    outcome
}

fn reconcile_pass(
    state: &crate::daemon::RuntimeState,
    cancelled: &Arc<AtomicBool>,
    jobs: &mut tokio::task::JoinSet<(String, String, Result<()>)>,
    reconcile: &mut ReconcileState,
) {
    if crate::upgrade::is_draining() {
        return;
    }
    reconcile.invalidate_hosts(state.mbx_install_wake().take_hosts());
    let projection = state.controller_projection();
    let config = projection.config;
    let active_hosts: BTreeSet<_> = projection
        .state
        .sessions
        .values()
        .filter_map(|session| session.build_cache.as_ref().map(|cache| cache.host.clone()))
        .collect();
    let mut machines = config.machines.clone();
    if active_hosts.contains("local") {
        machines
            .entry("local".into())
            .or_insert(mj_core::config::Machine::Local { build_cache: None });
    }
    for machine in machines.values() {
        let Some(host) = CacheHost::for_machine(machine) else {
            continue;
        };
        let key = host.key();
        let directories: Vec<_> = projection
            .state
            .sessions
            .values()
            .filter_map(|session| session.build_cache.as_ref())
            .filter(|cache| cache.host == key)
            .map(|cache| cache.directory.clone())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        if machine.build_cache().and_then(|settings| settings.enabled) == Some(false)
            && directories.is_empty()
        {
            continue;
        }
        if machine.build_cache().is_none() && !active_hosts.contains(&key) {
            continue;
        }
        let desired = format!("{machine:?}|{directories:?}");
        if !reconcile.should_start(&key, &desired) {
            continue;
        }
        reconcile.mark_started(key.clone());
        let machine = machine.clone();
        let cancelled = cancelled.clone();
        jobs.spawn(async move {
            let result = tokio::task::spawn_blocking(move || {
                let executor = targets::CancellableProcessExecutor::new(cancelled)
                    .with_deadline(Duration::from_secs(30));
                apply_machine_build_cache(&machine, &directories, &executor)
            })
            .await
            .context("machine build cache task failed")
            .and_then(|result| result);
            (key, desired, result)
        });
    }
}

#[cfg(test)]
mod tests {
    use super::ReconcileState;

    #[test]
    fn change_during_inflight_is_applied_after_completion_and_failures_are_not_retried() {
        let mut state = ReconcileState::default();

        // Initial policy starts.
        assert!(state.should_start("local", "v1"));
        state.mark_started("local".into());

        // A change arrives while "v1" is in flight: skipped while active.
        assert!(!state.should_start("local", "v2"));

        // "v1" finishes; the follow-up pass sees "v2" differs from "v1" and starts it.
        state.mark_finished("local".into(), "v1".into());
        assert!(state.should_start("local", "v2"));
        state.mark_started("local".into());

        // "v2" fails (recorded as last desired); unrelated revisions do not retry it.
        state.mark_finished("local".into(), "v2".into());
        assert!(!state.should_start("local", "v2"));

        // An mbx install while an application is in flight invalidates it even if desired matches.
        assert!(state.should_start("local", "v3"));
        state.mark_started("local".into());
        state.invalidate_hosts(["local".to_owned()]);
        state.mark_finished("local".into(), "v3".into());
        assert!(state.should_start("local", "v3"));
    }
}
