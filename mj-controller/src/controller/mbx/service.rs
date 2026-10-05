//! Reconcile machine cache policy without tying it to session launches.
use super::*;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

/// Applications are atomic and reconstructible from configuration, so they do
/// not hold daemon handoff admission. Shutdown cancels and joins every attempt.
pub(crate) async fn run(
    state: Arc<crate::daemon::RuntimeState>,
    stop: tokio_util::sync::CancellationToken,
) -> Result<()> {
    let cancelled = Arc::new(AtomicBool::new(false));
    let mut jobs = tokio::task::JoinSet::new();
    let mut active = BTreeSet::new();
    let mut policies: BTreeMap<String, (String, Instant, u32)> = BTreeMap::new();
    let mut tick = tokio::time::interval(Duration::from_secs(5));
    let outcome = async {
    loop {
        tokio::select! {
            _ = stop.cancelled() => break,
            _ = tick.tick() => {
                if crate::upgrade::is_draining() { continue; }
                let projection = state.controller_projection();
                let config = projection.config;
                let active_hosts: BTreeSet<_> = projection.state.sessions.values()
                    .filter_map(|session| session.build_cache.as_ref().map(|cache| cache.host.clone())).collect();
                let mut machines = config.machines.clone();
                if active_hosts.contains("local") {
                    machines.entry("local".into()).or_insert(mj_core::config::Machine::Local { build_cache: None });
                }
                for machine in machines.values() {
                    let Some(host) = CacheHost::for_machine(machine) else { continue; };
                    let key = host.key();
                    let directories: Vec<_> = projection.state.sessions.values()
                        .filter_map(|session| session.build_cache.as_ref())
                        .filter(|cache| cache.host == key)
                        .map(|cache| cache.directory.clone())
                        .collect::<BTreeSet<_>>().into_iter().collect();
                    if machine.build_cache().and_then(|settings| settings.enabled) == Some(false)
                        && directories.is_empty() { continue; }
                    if machine.build_cache().is_none() && !active_hosts.contains(&key) { continue; }
                    if active.contains(&key) { continue; }
                    let desired = format!("{machine:?}|{directories:?}");
                    if let Some((previous, next, _)) = policies.get(&key)
                        && previous == &desired && Instant::now() < *next { continue; }
                    active.insert(key.clone());
                    let machine = machine.clone();
                    let cancelled = cancelled.clone();
                    jobs.spawn(async move {
                        let result = tokio::task::spawn_blocking(move || {
                            let executor = targets::CancellableProcessExecutor::new(cancelled)
                                .with_deadline(Duration::from_secs(30));
                            apply_machine_build_cache(&machine, &directories, &executor)
                        }).await.context("machine build cache task failed").and_then(|result| result);
                        (key, desired, result)
                    });
                }
            }
            completed = jobs.join_next(), if !jobs.is_empty() => {
                let (key, desired, result) = completed.context("missing build cache application")?.context("build cache supervisor failed")?;
                active.remove(&key);
                let failures = if result.is_err() { policies.get(&key).map_or(1, |(_, _, failures)| failures.saturating_add(1)) } else { 0 };
                let delay = if failures == 0 { 60 } else { 5 * (1u64 << failures.min(8)) };
                if let Err(error) = result {
                    tracing::warn!(host = %key, "machine build cache configuration failed: {error:#}");
                    state.push_notice("", format!("Build cache settings on {key} could not be applied: {error:#}. Retrying in {delay} seconds."));
                }
                policies.insert(key, (desired, Instant::now() + Duration::from_secs(delay), failures));
            }
        }
    }
    Ok(())
    }.await;
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
