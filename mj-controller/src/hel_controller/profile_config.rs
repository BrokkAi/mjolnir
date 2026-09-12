//! Automatically populated, persistent profile capabilities.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use hel::hel_config::{HarnessProfile, HelConfig};
use hel::hel_targets::{CancellableProcessExecutor, CommandExecutor, CommandSpec, TargetLocator};
use hel::hel_worker_launch::{ProfileConfig, ProfileProbeSpec};
use sha2::{Digest, Sha256};

#[derive(Default)]
struct ProbeLock {
    gate: tokio::sync::Mutex<()>,
    cancelled: Arc<std::sync::atomic::AtomicBool>,
}

pub fn cancel_all() {
    if let Some(probes) = PROBES.get() {
        for probe in probes
            .lock()
            .expect("profile probe locks poisoned")
            .values()
            .filter_map(Weak::upgrade)
        {
            probe
                .cancelled
                .store(true, std::sync::atomic::Ordering::Release);
        }
    }
}
static PROBES: OnceLock<Mutex<BTreeMap<String, Weak<ProbeLock>>>> = OnceLock::new();

/// A request owns its background job independently of the HTTP connection.
/// Concurrent callers serialize by profile and reuse the completed cache.
pub async fn discover(
    profile_id: String,
    model: Option<String>,
    refresh: bool,
) -> Result<ProfileConfig> {
    serialized(profile_id.clone(), move |cancelled| {
        discover_blocking(&profile_id, model, refresh, cancelled)
    })
    .await
}

async fn serialized(
    profile_id: String,
    job: impl FnOnce(Arc<std::sync::atomic::AtomicBool>) -> Result<ProfileConfig> + Send + 'static,
) -> Result<ProfileConfig> {
    let lock = {
        let mut locks = PROBES
            .get_or_init(Default::default)
            .lock()
            .expect("profile probe locks poisoned");
        locks.retain(|_, lock| lock.strong_count() > 0);
        let lock = locks
            .get(&profile_id)
            .and_then(Weak::upgrade)
            .unwrap_or_else(|| Arc::new(ProbeLock::default()));
        locks.insert(profile_id, Arc::downgrade(&lock));
        lock
    };
    tokio::spawn(async move {
        let _guard = lock.gate.lock().await;
        let cancelled = lock.cancelled.clone();
        let result = tokio::task::spawn_blocking(move || job(cancelled))
            .await
            .context("profile discovery task panicked")?;
        if let Err(error) = &result {
            tracing::warn!(error = %format!("{error:#}"), "profile discovery failed");
        }
        result
    })
    .await
    .context("profile discovery supervisor panicked")?
}

/// Update a model-specific entry only when a managed worker matches the local
/// probe binary. Container/ambient installations cannot establish that match.
pub async fn observe(
    profile_id: String,
    worker_build: String,
    state: hel::hel_worker::RelayOperationalState,
) -> Result<()> {
    serialized(profile_id.clone(), move |cancelled| {
        let config = HelConfig::load()?;
        let profile = config
            .enabled_profile(&profile_id)
            .context("observed profile was removed")?;
        let executor =
            CancellableProcessExecutor::new(cancelled).with_deadline(Duration::from_secs(30));
        let worker = super::worker_binary::worker_binary_for(
            &TargetLocator::LocalBare {
                worker_root: String::new(),
            },
            &executor,
        )?;
        let facts = hel::hel_acp::AcpSessionFacts::from_operational(
            profile.kind,
            &state.config,
            &state.config_options,
            state.modes.as_ref(),
        );
        let choices = ProfileConfig {
            model: facts.current_model().map(str::to_owned),
            models: hel::hel_acp::session_config_choices(&state.config_options, "model"),
            efforts: hel::hel_acp::session_config_choices(&state.config_options, "effort"),
            observed_at: chrono::Utc::now().timestamp(),
        };
        if hel::hel_worker_launch::worker_executable_digest(&worker)? == worker_build {
            store(
                &profile_id,
                &fingerprint(profile)?,
                &choices.model,
                &choices,
            )?;
        }
        Ok(choices)
    })
    .await
    .map(|_| ())
}

fn fingerprint(profile: &HarnessProfile) -> Result<String> {
    let mut hash = Sha256::new();
    hash.update(serde_json::to_vec(profile)?);
    hash.update(
        hel::hel_harness_runtime::pin(profile.kind)
            .install_id
            .as_bytes(),
    );
    Ok(format!("{:x}", hash.finalize()))
}

fn discover_blocking(
    profile_id: &str,
    model: Option<String>,
    refresh: bool,
    cancelled: Arc<std::sync::atomic::AtomicBool>,
) -> Result<ProfileConfig> {
    ensure!(
        !cancelled.load(std::sync::atomic::Ordering::Acquire),
        "profile discovery cancelled"
    );
    let config = HelConfig::load()?;
    let profile = config
        .enabled_profile(profile_id)
        .with_context(|| format!("unknown or disabled profile {profile_id:?}"))?;
    let fingerprint = fingerprint(profile)?;
    resolve_cached(
        refresh,
        || {
            hel::hel_database::load_profile_config_cache(
                profile_id,
                model.as_deref().unwrap_or_default(),
                &fingerprint,
            )?
            .map(|body| serde_json::from_str(&body).context("read cached profile configuration"))
            .transpose()
        },
        || probe_profile(profile, model.clone(), cancelled),
        |choices| {
            if model.is_none() || choices.model == model {
                store(profile_id, &fingerprint, &model, choices)?;
            }
            store(profile_id, &fingerprint, &choices.model, choices)
        },
    )
}

fn resolve_cached(
    refresh: bool,
    load: impl FnOnce() -> Result<Option<ProfileConfig>>,
    probe: impl FnOnce() -> Result<ProfileConfig>,
    save: impl FnOnce(&ProfileConfig) -> Result<()>,
) -> Result<ProfileConfig> {
    if !refresh && let Some(choices) = load()? {
        return Ok(choices);
    }
    let choices = probe()?;
    save(&choices)?;
    Ok(choices)
}

fn probe_profile(
    profile: &HarnessProfile,
    model: Option<String>,
    cancelled: Arc<std::sync::atomic::AtomicBool>,
) -> Result<ProfileConfig> {
    let root = tempfile::tempdir().context("create private profile discovery directory")?;
    let home = root.path().join("profile");
    super::worker_binary::stage_profile(profile, &home)?;
    let cwd = root.path().join("workspace");
    std::fs::create_dir(&cwd)?;
    let executor =
        CancellableProcessExecutor::new(cancelled).with_deadline(Duration::from_secs(300));
    let worker = super::worker_binary::worker_binary_for(
        &TargetLocator::LocalBare {
            worker_root: root.path().to_string_lossy().into_owned(),
        },
        &executor,
    )?;
    let spec = ProfileProbeSpec {
        harness: profile.kind,
        profile_home: home,
        environment: profile.environment.clone(),
        cwd,
        model,
    };
    let path = root.path().join("probe.json");
    hel::hel_config::atomic_write(&path, &serde_json::to_vec(&spec)?)?;
    let command = CommandSpec::new(
        worker.to_string_lossy(),
        [
            "worker".to_owned(),
            "discover-config".into(),
            "--spec".into(),
            path.to_string_lossy().into_owned(),
        ],
    )
    .purpose("discover profile configuration");
    let output = executor.execute(&command)?;
    ensure!(
        output.status == 0,
        "profile discovery failed: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    let choices: ProfileConfig =
        serde_json::from_slice(&output.stdout).context("decode discovered configuration")?;
    Ok(choices)
}

fn store(
    profile: &str,
    fingerprint: &str,
    model: &Option<String>,
    choices: &ProfileConfig,
) -> Result<()> {
    hel::hel_database::save_profile_config_cache(
        profile.to_owned(),
        model.clone().unwrap_or_default(),
        fingerprint.to_owned(),
        serde_json::to_string(choices)?,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};

    #[tokio::test]
    async fn concurrent_cold_lookups_share_the_first_probe() {
        let cache = Arc::new(Mutex::new(None));
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut tasks = vec![];
        for _ in 0..8 {
            let cache = cache.clone();
            let calls = calls.clone();
            tasks.push(tokio::spawn(serialized(
                "concurrent-cache-test".into(),
                move |_| {
                    resolve_cached(
                        false,
                        || Ok(cache.lock().unwrap().clone()),
                        || {
                            calls.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            std::thread::sleep(Duration::from_millis(20));
                            Ok(ProfileConfig {
                                model: None,
                                models: vec![],
                                efforts: vec![],
                                observed_at: 1,
                            })
                        },
                        |value| {
                            *cache.lock().unwrap() = Some(value.clone());
                            Ok(())
                        },
                    )
                },
            )));
        }
        for task in tasks {
            task.await.unwrap().unwrap();
        }
        assert_eq!(calls.load(std::sync::atomic::Ordering::Relaxed), 1);
    }

    #[test]
    fn empty_cache_discovers_automatically_and_failed_probes_remain_retryable() {
        let cache = RefCell::new(None);
        let calls = Cell::new(0);
        let choices = ProfileConfig {
            model: Some("full/model".into()),
            models: vec![],
            efforts: vec![],
            observed_at: 1,
        };
        let lookup = |fail: bool| {
            resolve_cached(
                false,
                || Ok(cache.borrow().clone()),
                || {
                    calls.set(calls.get() + 1);
                    ensure!(!fail, "probe failed");
                    Ok(choices.clone())
                },
                |value| {
                    *cache.borrow_mut() = Some(value.clone());
                    Ok(())
                },
            )
        };
        assert!(lookup(true).is_err());
        assert!(cache.borrow().is_none());
        assert_eq!(lookup(false).unwrap(), choices);
        assert_eq!(
            lookup(true).unwrap(),
            choices,
            "a warm cache must not run the failing probe"
        );
        assert_eq!(calls.get(), 2);
        assert_eq!(
            resolve_cached(
                true,
                || Ok(cache.borrow().clone()),
                || Ok(ProfileConfig {
                    observed_at: 2,
                    ..choices.clone()
                }),
                |_| Ok(())
            )
            .unwrap()
            .observed_at,
            2
        );
    }
}
