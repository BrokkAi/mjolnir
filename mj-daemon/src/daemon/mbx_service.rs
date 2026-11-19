//! Reconcile machine cache policy without tying it to session launches.
use anyhow::{Context, Result};
use mj_controller::controller::CacheHost;
use mj_controller::controller::mbx::*;
use mj_controller::targets;
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::{
    Arc, Mutex, PoisonError,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;

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
    /// Hosts with an application in flight, and the session set when it
    /// started.
    active: BTreeMap<String, u64>,
    last_desired: BTreeMap<String, String>,
    stale_active: BTreeSet<String>,
    /// Hosts whose last application failed, with the session set then. Any
    /// session launched or removed retries it, even when `desired` is
    /// unchanged, including a launch that itself had to run without the cache.
    failed: BTreeMap<String, u64>,
}

impl ReconcileState {
    fn invalidate_hosts(&mut self, hosts: impl IntoIterator<Item = String>) {
        for key in hosts {
            self.last_desired.remove(&key);
            if self.active.contains_key(&key) {
                self.stale_active.insert(key);
            }
        }
    }

    /// A host that no longer qualifies (settings disabled, machine removed)
    /// loses its record, so qualifying again applies the policy afresh.
    fn forget_hosts_except<'a>(&mut self, eligible: impl IntoIterator<Item = &'a str>) {
        let eligible: BTreeSet<&str> = eligible.into_iter().collect();
        self.last_desired
            .retain(|key, _| eligible.contains(key.as_str()));
        self.failed.retain(|key, _| eligible.contains(key.as_str()));
    }

    fn should_start(&self, key: &str, desired: &str, sessions: u64) -> bool {
        !self.active.contains_key(key)
            && (self.last_desired.get(key).map(String::as_str) != Some(desired)
                || self
                    .failed
                    .get(key)
                    .is_some_and(|failed| *failed != sessions))
    }

    fn mark_started(&mut self, key: String, sessions: u64) {
        self.active.insert(key, sessions);
    }

    fn mark_finished(&mut self, key: String, desired: String, succeeded: bool) {
        let sessions = self.active.remove(&key).unwrap_or_default();
        if self.stale_active.remove(&key) {
            return;
        }
        self.last_desired.insert(key.clone(), desired);
        if succeeded {
            self.failed.remove(&key);
        } else {
            self.failed.insert(key, sessions);
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
                    let succeeded = result.is_ok();
                    if let Err(error) = result {
                        tracing::warn!(host = %key, "machine build cache configuration failed: {error:#}");
                        state.push_notice(
                            "",
                            format!(
                                "Build cache settings on {key} could not be applied: {error:#}. They will be applied with the next change to this machine's build cache settings, the next session launch, or an mbx install."
                            ),
                        );
                    }
                    reconcile.mark_finished(key, desired, succeeded);
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
    if mj_controller::upgrade::is_draining() {
        return;
    }
    reconcile.invalidate_hosts(state.mbx_install_wake().take_hosts());
    // Revisions are published for routine session activity too, so read the
    // inputs in one pass under the controller read, cloning only the few
    // machines that qualify rather than a projection of every record.
    let inputs = state.with_controller(eligible_hosts);
    reconcile.forget_hosts_except(inputs.iter().map(|input| input.key.as_str()));
    for HostInput {
        key,
        machine,
        directories,
        desired,
        sessions,
    } in inputs
    {
        if !reconcile.should_start(&key, &desired, sessions) {
            continue;
        }
        reconcile.mark_started(key.clone(), sessions);
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

/// One host whose policy the service applies, with the inputs that decide it.
struct HostInput {
    key: String,
    machine: mj_core::config::Machine,
    directories: Vec<PathBuf>,
    /// Machine settings and mounted directories; a change applies the policy.
    desired: String,
    /// Hash of every session ID; a change retries a failed application.
    sessions: u64,
}

fn eligible_hosts(controller: &mj_controller::controller::Controller) -> Vec<HostInput> {
    use std::hash::{Hash, Hasher};
    let mut mounted: BTreeMap<&str, BTreeSet<&PathBuf>> = BTreeMap::new();
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    for (id, session) in controller.state.sessions.iter() {
        id.hash(&mut hasher);
        if let Some(cache) = &session.build_cache {
            mounted
                .entry(cache.host.as_str())
                .or_default()
                .insert(&cache.directory);
        }
    }
    let sessions = hasher.finish();
    // One machine per cache host, chosen as session configuration chooses its
    // settings (the first entry for the host that has build cache settings),
    // so two entries for one host cannot take turns rewriting its policy.
    let mut machines: BTreeMap<String, &mj_core::config::Machine> = BTreeMap::new();
    for machine in controller.config.machines.values() {
        let Some(host) = CacheHost::for_machine(machine) else {
            continue;
        };
        let chosen = machines.entry(host.key()).or_insert(machine);
        if chosen.build_cache().is_none() && machine.build_cache().is_some() {
            *chosen = machine;
        }
    }
    let local = mj_core::config::Machine::Local { build_cache: None };
    if mounted.contains_key("local") {
        machines.entry("local".into()).or_insert(&local);
    }
    machines
        .into_iter()
        .filter_map(|(key, machine)| {
            let directories: Vec<PathBuf> = mounted
                .get(key.as_str())
                .map(|directories| {
                    directories
                        .iter()
                        .map(|directory| (*directory).clone())
                        .collect()
                })
                .unwrap_or_default();
            let settings = machine.build_cache();
            if settings.and_then(|settings| settings.enabled) == Some(false)
                && directories.is_empty()
            {
                return None;
            }
            if settings.is_none() && !mounted.contains_key(key.as_str()) {
                return None;
            }
            Some(HostInput {
                desired: format!("{machine:?}|{directories:?}"),
                key,
                machine: machine.clone(),
                directories,
                sessions,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::ReconcileState;

    #[test]
    fn change_during_inflight_is_applied_after_completion_and_failures_wait_for_an_event() {
        let mut state = ReconcileState::default();

        // A change that arrives while "v1" is in flight starts after it.
        assert!(state.should_start("local", "v1", 1));
        state.mark_started("local".into(), 1);
        assert!(!state.should_start("local", "v2", 1));
        state.mark_finished("local".into(), "v1".into(), true);
        assert!(state.should_start("local", "v2", 1));
        state.mark_started("local".into(), 1);

        // "v2" fails: unrelated revisions do not retry it, but a session
        // launched on the host does, although `desired` is unchanged.
        state.mark_finished("local".into(), "v2".into(), false);
        assert!(!state.should_start("local", "v2", 1));
        assert!(state.should_start("local", "v2", 2));
        state.mark_started("local".into(), 2);
        state.mark_finished("local".into(), "v2".into(), true);
        assert!(!state.should_start("local", "v2", 3));

        // A failed host that stops qualifying (settings disabled, machine
        // removed) applies afresh when it qualifies again with the same policy.
        state.mark_started("local".into(), 1);
        state.mark_finished("local".into(), "v4".into(), false);
        assert!(!state.should_start("local", "v4", 1));
        state.forget_hosts_except(std::iter::empty());
        assert!(state.should_start("local", "v4", 1));

        // An mbx install during an application invalidates it.
        assert!(state.should_start("local", "v3", 1));
        state.mark_started("local".into(), 1);
        state.invalidate_hosts(["local".to_owned()]);
        state.mark_finished("local".into(), "v3".into(), true);
        assert!(state.should_start("local", "v3", 1));
    }
}
