//! Automatically populated, persistent profile capabilities.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::Duration;

use crate::targets::{CancellableProcessExecutor, CommandExecutor, CommandSpec, TargetLocator};
use anyhow::{Context, Result, ensure};
use mj_core::config::{Config, HarnessProfile};
use mj_core::worker_launch::{ProfileConfig, ProfileProbeSpec};
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
    state: mj_core::relay::RelayOperationalState,
) -> Result<()> {
    serialized(profile_id.clone(), move |cancelled| {
        let config = Config::load()?;
        let profile = config
            .enabled_profile(&profile_id)
            .context("observed profile was removed")?;
        let facts = mj_core::acp::AcpSessionFacts::from_operational(
            profile.kind,
            &state.config,
            &state.config_options,
            state.modes.as_ref(),
        );
        let mut choices = ProfileConfig {
            model: facts.current_model().map(str::to_owned),
            models: mj_core::acp::session_config_choices(&state.config_options, "model"),
            efforts: mj_core::acp::session_config_choices(&state.config_options, "effort"),
            observed_at: chrono::Utc::now().timestamp(),
        };
        enrich_profile_config(profile, &mut choices)?;
        // Worker observations do not carry authentication provenance. Claude's
        // setup-token and login catalogues differ, so only probes may cache it.
        if profile.kind == mj_core::config::HarnessKind::Claude {
            return Ok(choices);
        }
        let executor =
            CancellableProcessExecutor::new(cancelled).with_deadline(Duration::from_secs(30));
        let worker = super::worker_binary::worker_binary_for(
            &TargetLocator::LocalBare {
                worker_root: String::new(),
            },
            &executor,
        )?;
        if mj_core::worker_launch::worker_executable_digest(&worker)? == worker_build {
            store(
                &profile_id,
                &fingerprint(profile, &profile.environment)?,
                &choices.model,
                &choices,
            )?;
        }
        Ok(choices)
    })
    .await
    .map(|_| ())
}

fn fingerprint(profile: &HarnessProfile, environment: &BTreeMap<String, String>) -> Result<String> {
    let mut hash = Sha256::new();
    hash.update(b"profile-config-v3\0");
    hash.update(serde_json::to_vec(profile)?);
    hash.update(serde_json::to_vec(environment)?);
    hash.update(
        mj_core::harness_runtime::pin(profile.kind)
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
    let config = Config::load()?;
    let profile = config
        .enabled_profile(profile_id)
        .with_context(|| format!("unknown or disabled profile {profile_id:?}"))?;
    let mut environment = profile.environment.clone();
    super::worker_binary::apply_claude_setup_token(
        &mut environment,
        profile.kind,
        &mj_core::credentials::claude_oauth_token_path(profile_id),
    );
    let fingerprint = fingerprint(profile, &environment)?;
    resolve_cached(
        refresh,
        || {
            crate::database::load_profile_config_cache(
                profile_id,
                model.as_deref().unwrap_or_default(),
                &fingerprint,
            )?
            .map(|body| serde_json::from_str(&body).context("read cached profile configuration"))
            .transpose()
        },
        || {
            let mut choices = probe_profile(
                profile_id,
                profile,
                environment.clone(),
                model.clone(),
                cancelled,
            )?;
            enrich_profile_config(profile, &mut choices)?;
            Ok(choices)
        },
        |choices| {
            if model.is_none() || choices.model == model {
                store(profile_id, &fingerprint, &model, choices)?;
            }
            store(profile_id, &fingerprint, &choices.model, choices)
        },
    )
}

/// Muse's ACP bridge reports effort choices but no model selector. Its native
/// settings file is authoritative for the one model this profile will run, so
/// publish that value rather than making callers create a session to learn it.
fn enrich_profile_config(profile: &HarnessProfile, choices: &mut ProfileConfig) -> Result<()> {
    if profile.kind != mj_core::config::HarnessKind::Muse || !choices.models.is_empty() {
        return Ok(());
    }
    let path = profile.home.join("settings.json");
    let metadata = std::fs::metadata(&path)
        .with_context(|| format!("read Muse settings metadata {}", path.display()))?;
    ensure!(metadata.len() <= 1024 * 1024, "Muse settings are too large");
    let settings: serde_json::Value = serde_json::from_slice(
        &std::fs::read(&path).with_context(|| format!("read Muse settings {}", path.display()))?,
    )
    .context("decode Muse settings")?;
    let model = settings
        .get("model")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|model| !model.is_empty())
        .context("Muse settings do not select a model")?;
    choices.model = Some(model.to_owned());
    choices.models.push(mj_core::acp::SessionConfigChoice {
        value: model.to_owned(),
        name: model.to_owned(),
        description: Some("Configured by Muse Code settings".into()),
    });
    Ok(())
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
    profile_id: &str,
    profile: &HarnessProfile,
    environment: BTreeMap<String, String>,
    model: Option<String>,
    cancelled: Arc<std::sync::atomic::AtomicBool>,
) -> Result<ProfileConfig> {
    let root = tempfile::tempdir().context("create private profile discovery directory")?;
    let home = if profile.kind == mj_core::config::HarnessKind::Zcode {
        root.path().join("profile/.zcode")
    } else {
        root.path().join("profile")
    };
    super::worker_binary::stage_profile(profile, &home)?;
    super::worker_binary::stage_codex_catalog(
        profile_id,
        profile,
        &home,
        &super::worker_binary::fetch_catalog_over_https,
        &super::worker_binary::SharedCatalogCache,
    )?;
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
        environment,
        cwd,
        model,
    };
    let path = root.path().join("probe.json");
    mj_core::config::atomic_write(&path, &serde_json::to_vec(&spec)?)?;
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
    if !output.stderr.is_empty() {
        tracing::info!(harness = ?profile.kind, diagnostics = %String::from_utf8_lossy(&output.stderr).trim(), "profile discovery worker diagnostics");
    }
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
    crate::database::save_profile_config_cache(
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

    #[test]
    fn muse_discovery_publishes_the_model_selected_by_native_settings() {
        let home = tempfile::tempdir().unwrap();
        std::fs::write(
            home.path().join("settings.json"),
            br#"{"model":"muse-spark-1.3-contributor"}"#,
        )
        .unwrap();
        let profile = HarnessProfile {
            enabled: true,
            kind: mj_core::config::HarnessKind::Muse,
            home: home.path().into(),
            environment: BTreeMap::new(),
            context_window_bytes: None,
        };
        let mut choices = ProfileConfig {
            model: Some(String::new()),
            models: Vec::new(),
            efforts: Vec::new(),
            observed_at: 1,
        };

        enrich_profile_config(&profile, &mut choices).unwrap();

        assert_eq!(choices.model.as_deref(), Some("muse-spark-1.3-contributor"));
        assert_eq!(choices.models.len(), 1);
        assert_eq!(choices.models[0].value, "muse-spark-1.3-contributor");
    }

    #[test]
    fn setup_token_changes_invalidate_the_discovery_cache() {
        use mj_core::credentials::{CLAUDE_OAUTH_TOKEN_ENV, write_claude_oauth_token};
        let root = tempfile::tempdir().unwrap();
        let token = root.path().join("token");
        let profile = HarnessProfile {
            enabled: true,
            kind: mj_core::config::HarnessKind::Claude,
            home: root.path().into(),
            environment: BTreeMap::new(),
            context_window_bytes: None,
        };
        let resolve = |environment: BTreeMap<String, String>| {
            let mut environment = environment;
            super::super::worker_binary::apply_claude_setup_token(
                &mut environment,
                profile.kind,
                &token,
            );
            environment
        };
        let login = fingerprint(&profile, &resolve(BTreeMap::new())).unwrap();
        write_claude_oauth_token(&token, b"setup-first").unwrap();
        let first = resolve(BTreeMap::new());
        assert_eq!(first[CLAUDE_OAUTH_TOKEN_ENV], "setup-first");
        let first_key = fingerprint(&profile, &first).unwrap();
        assert_ne!(login, first_key);
        write_claude_oauth_token(&token, b"setup-second").unwrap();
        assert_eq!(
            first[CLAUDE_OAUTH_TOKEN_ENV], "setup-first",
            "an in-flight probe retains its authentication snapshot"
        );
        assert_ne!(
            first_key,
            fingerprint(&profile, &resolve(BTreeMap::new())).unwrap()
        );
        let explicit = BTreeMap::from([(CLAUDE_OAUTH_TOKEN_ENV.into(), "explicit".into())]);
        assert_eq!(resolve(explicit.clone()), explicit);
        assert!(!first_key.contains("setup-first"));
    }

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
