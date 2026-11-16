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
    /// Hosts with an application in flight, and the sessions using each
    /// host's cache when it started.
    active: BTreeMap<String, String>,
    last_desired: BTreeMap<String, String>,
    stale_active: BTreeSet<String>,
    /// Hosts whose last application failed, with the sessions using the cache
    /// then. A session launched on (or removed from) such a host retries it,
    /// even when the set of cache directories, and so `desired`, is unchanged.
    failed: BTreeMap<String, String>,
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

    fn should_start(&self, key: &str, desired: &str, sessions: &str) -> bool {
        !self.active.contains_key(key)
            && (self.last_desired.get(key).map(String::as_str) != Some(desired)
                || self
                    .failed
                    .get(key)
                    .is_some_and(|failed| failed != sessions))
    }

    fn mark_started(&mut self, key: String, sessions: String) {
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
                                "Build cache settings on {key} could not be applied: {error:#}. They will be applied with the next change to this machine's build cache settings, the next session launch on it, or an mbx install."
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
    if crate::upgrade::is_draining() {
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
        if !reconcile.should_start(&key, &desired, &sessions) {
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
    /// Sessions using the host's cache; a change retries a failed application.
    sessions: String,
}

fn eligible_hosts(controller: &crate::controller::Controller) -> Vec<HostInput> {
    let mut mounted: BTreeMap<&str, (BTreeSet<&PathBuf>, BTreeSet<&str>)> = BTreeMap::new();
    for (id, session) in controller.state.sessions.iter() {
        if let Some(cache) = &session.build_cache {
            let (directories, sessions) = mounted.entry(cache.host.as_str()).or_default();
            directories.insert(&cache.directory);
            sessions.insert(id.as_str());
        }
    }
    let local = mj_core::config::Machine::Local { build_cache: None };
    let implicit_local = mounted.contains_key("local")
        && !controller.config.machines.values().any(|machine| {
            CacheHost::for_machine(machine).is_some_and(|host| host.key() == "local")
        });
    controller
        .config
        .machines
        .values()
        .chain(implicit_local.then_some(&local))
        .filter_map(|machine| {
            let key = CacheHost::for_machine(machine)?.key();
            let (directories, sessions) = mounted
                .get(key.as_str())
                .map(|(directories, sessions)| {
                    (
                        directories
                            .iter()
                            .map(|directory| (*directory).clone())
                            .collect::<Vec<_>>(),
                        format!("{sessions:?}"),
                    )
                })
                .unwrap_or_else(|| (Vec::new(), "{}".to_owned()));
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
        assert!(state.should_start("local", "v1", "[a]"));
        state.mark_started("local".into(), "[a]".into());
        assert!(!state.should_start("local", "v2", "[a]"));
        state.mark_finished("local".into(), "v1".into(), true);
        assert!(state.should_start("local", "v2", "[a]"));
        state.mark_started("local".into(), "[a]".into());

        // "v2" fails: unrelated revisions do not retry it, but a session
        // launched on the host does, although `desired` is unchanged.
        state.mark_finished("local".into(), "v2".into(), false);
        assert!(!state.should_start("local", "v2", "[a]"));
        assert!(state.should_start("local", "v2", "[a, b]"));
        state.mark_started("local".into(), "[a, b]".into());
        state.mark_finished("local".into(), "v2".into(), true);
        assert!(!state.should_start("local", "v2", "[a, b, c]"));

        // A failed host that stops qualifying (settings disabled, machine
        // removed) applies afresh when it qualifies again with the same policy.
        state.mark_started("local".into(), "[a]".into());
        state.mark_finished("local".into(), "v4".into(), false);
        assert!(!state.should_start("local", "v4", "[a]"));
        state.forget_hosts_except(std::iter::empty());
        assert!(state.should_start("local", "v4", "[a]"));

        // An mbx install during an application invalidates it.
        assert!(state.should_start("local", "v3", "[a]"));
        state.mark_started("local".into(), "[a]".into());
        state.invalidate_hosts(["local".to_owned()]);
        state.mark_finished("local".into(), "v3".into(), true);
        assert!(state.should_start("local", "v3", "[a]"));
    }
}
