//! Automatically populated profile capabilities, cached for the life of the daemon.

use std::collections::BTreeMap;
use std::sync::{Arc, LazyLock, Mutex, OnceLock, Weak};
use std::time::Duration;

use crate::targets::{CancellableProcessExecutor, CommandExecutor, CommandSpec, TargetLocator};
use anyhow::{Context, Result, ensure};
use mj_core::config::{Config, HarnessProfile};
use mj_core::worker_launch::{ProfileConfig, ProfileProbeSpec};
use tokio_util::sync::CancellationToken;

/// Pre-session choices use the same enabled-profile set as delegation.
/// Discovery remains supervised and cached; independent profiles run concurrently.
pub async fn subagent_options(
    parent: String,
    model: Option<String>,
) -> Result<mj_core::subagent::SubagentOptions> {
    let config = tokio::task::spawn_blocking(Config::load)
        .await
        .context("load subagent profiles")??;
    subagent_options_with(&config, &parent, model, |id, model| {
        discover(id, model, false)
    })
    .await
}

/// Setup probes its draft without saving it or changing the live configuration.
pub async fn subagent_options_for(
    config: Config,
    parent: String,
    model: Option<String>,
) -> Result<mj_core::subagent::SubagentOptions> {
    subagent_options_with(&config, &parent, model, |id, model| {
        let draft = config.clone();
        serialized(id.clone(), move |cancelled| {
            discover_from_config(&draft, &id, model, false, cancelled)
        })
    })
    .await
}

pub(crate) async fn discover_for(
    config: Config,
    id: String,
    model: Option<String>,
    cancellation: CancellationToken,
) -> Result<ProfileConfig> {
    serialized_until(id.clone(), cancellation, move |cancelled| {
        discover_from_config(&config, &id, model, false, cancelled)
    })
    .await
}

/// Validate the session's straightforward choice, and its subtle choice when
/// it has one, against enabled profiles. Subtle is optional; straightforward
/// is not.
pub async fn validate_session_subagent_policy(
    config: &Config,
    profile_id: &str,
    policy: &mj_core::subagent::SubagentPolicy,
) -> Result<()> {
    let refuse = |message: String| {
        anyhow::Error::new(
            mj_core::refusal::Refusal::unusable(message)
                .with_code(mj_core::subagent::CHOICE_UNAVAILABLE_CODE),
        )
    };
    let kind = config
        .enabled_profile(profile_id)
        .context("parent profile unavailable")?
        .kind;
    if !kind.supports_delegation_tools() && !policy.is_native() {
        return Err(refuse(format!(
            "{} sessions use their harness's own delegation; Mjolnir sub-agents and the None choice need a Claude, Codex, or DSH profile",
            kind.display_name()
        )));
    }
    if policy.is_native() && !kind.has_native_subagents() {
        return Err(anyhow::Error::new(mj_core::refusal::Refusal::unusable(
            format!(
                "{} has no sub-agents of its own, so Native delegation does nothing for it; choose Mjolnir or None",
                kind.display_name()
            ),
        )));
    }
    if !policy.uses_mjolnir() {
        return Ok(());
    }
    use mj_core::subagent::SubagentTaskType;
    if config
        .subagents
        .effective_choice(policy, SubagentTaskType::Straightforward)
        .is_none()
    {
        return Err(refuse(
            "Mjolnir requires [subagents.straightforward] or --subagent-straightforward <MODEL>"
                .into(),
        ));
    }
    for task_type in [SubagentTaskType::Straightforward, SubagentTaskType::Subtle] {
        let Some(choice) = config.subagents.effective_choice(policy, task_type) else {
            continue;
        };
        let options = subagent_options_for(
            config.clone(),
            profile_id.to_owned(),
            Some(choice.model.clone()),
        )
        .await?;
        options
            .validate(choice)
            .map_err(|message| refuse(format!("[subagents.{}]: {message}", task_type.id())))?;
    }
    Ok(())
}

pub(crate) async fn subagent_options_with<F, Fut>(
    config: &Config,
    parent: &str,
    model: Option<String>,
    probe: F,
) -> Result<mj_core::subagent::SubagentOptions>
where
    F: Fn(String, Option<String>) -> Fut + Copy,
    Fut: std::future::Future<Output = Result<ProfileConfig>>,
{
    config
        .enabled_profile(parent)
        .context("parent profile is unavailable")?;
    let candidates = config
        .profiles
        .iter()
        .filter(|(_, profile)| profile.enabled)
        .map(|(id, _)| id.clone())
        .collect::<Vec<_>>();
    let mut options = mj_core::subagent::SubagentOptions::default();
    let results = futures::future::join_all(candidates.into_iter().map(|id| {
        let model = model.clone();
        async move {
            let result = async {
                let mut choices = probe(id.clone(), None).await?;
                if let Some(model) = model
                    .as_ref()
                    .filter(|model| choices.models.iter().any(|choice| &choice.value == *model))
                {
                    choices = probe(id.clone(), Some(model.clone())).await?;
                }
                Ok::<_, anyhow::Error>(choices)
            }
            .await;
            (id, result)
        }
    }))
    .await;
    for (id, result) in results {
        match result {
            Ok(choices) => {
                if model
                    .as_ref()
                    .is_some_and(|model| choices.models.iter().any(|choice| &choice.value == model))
                {
                    for choice in choices.efforts {
                        if !options
                            .efforts
                            .iter()
                            .any(|existing| existing.value == choice.value)
                        {
                            options.efforts.push(choice);
                        }
                    }
                }
                for choice in choices.models {
                    if !options
                        .models
                        .iter()
                        .any(|existing| existing.value == choice.value)
                    {
                        options.models.push(choice);
                    }
                }
            }
            Err(error) => options.unavailable.push(format!("{id}: {error:#}")),
        }
    }
    Ok(options)
}

#[derive(Default)]
struct ProbeLock {
    gate: tokio::sync::Mutex<()>,
    cancellation: CancellationToken,
}

pub fn cancel_all() {
    if let Some(probes) = PROBES.get() {
        for probe in probes
            .lock()
            .expect("profile probe locks poisoned")
            .values()
            .filter_map(Weak::upgrade)
        {
            probe.cancellation.cancel();
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
    serialized_until(profile_id, CancellationToken::new(), job).await
}

async fn serialized_until(
    profile_id: String,
    cancellation: CancellationToken,
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
        let _guard = tokio::select! {
            biased;
            _ = cancellation.cancelled() => anyhow::bail!("profile definition retired"),
            _ = lock.cancellation.cancelled() => anyhow::bail!("profile discovery cancelled"),
            guard = lock.gate.lock() => guard,
        };
        let cancelled = Arc::new(std::sync::atomic::AtomicBool::new(false));
        // Runtime shutdown can drop this supervisor before it polls a token.
        // Its blocking process must still receive cancellation before the
        // profile lock is released.
        struct CancelOnDrop(Arc<std::sync::atomic::AtomicBool>);
        impl Drop for CancelOnDrop {
            fn drop(&mut self) {
                self.0.store(true, std::sync::atomic::Ordering::Release);
            }
        }
        let _cancel_on_drop = CancelOnDrop(cancelled.clone());
        let job_cancelled = cancelled.clone();
        let mut task = tokio::task::spawn_blocking(move || job(job_cancelled));
        let result = tokio::select! {
            biased;
            _ = cancellation.cancelled() => {
                cancelled.store(true, std::sync::atomic::Ordering::Release);
                task.await
            }
            _ = lock.cancellation.cancelled() => {
                cancelled.store(true, std::sync::atomic::Ordering::Release);
                task.await
            }
            result = &mut task => result,
        }
        .context("profile discovery task panicked")?;
        match &result {
            Err(error) if discovery_is_unsupported(error) => {
                tracing::debug!(error = %format!("{error:#}"), "profile discovery unsupported");
            }
            Err(error) => {
                tracing::warn!(error = %format!("{error:#}"), "profile discovery failed");
            }
            Ok(_) => {}
        }
        result
    })
    .await
    .context("profile discovery supervisor panicked")?
}

/// What a session's worker must show before its observed choices are cached.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ObservationCheck {
    /// Its build matches the local probe binary, so it reports what a probe
    /// would.
    ProbeBuild,
    /// None: this machine runs no worker, so no probe can ever describe a
    /// profile, and a session's worker is the only source there is.
    NoProbe,
}

/// The one decision of whose observations may fill the profile cache. Where
/// a probe can run, only a managed bare worker that matches it may;
/// container and ambient installations cannot establish that match.
pub(crate) fn observation_check(
    target: &mj_core::state::TargetLocator,
) -> Option<ObservationCheck> {
    use mj_core::state::TargetLocator;
    if !mj_core::targets::HOST_RUNS_WORKERS {
        return Some(ObservationCheck::NoProbe);
    }
    matches!(
        target,
        TargetLocator::LocalBare { .. }
            | TargetLocator::SshBare { .. }
            | TargetLocator::AwsEc2 { .. }
    )
    .then_some(ObservationCheck::ProbeBuild)
}

/// Cache the choices a session's worker reports, when `check` allows it.
/// Returns whether anything was cached.
pub(crate) async fn observe(
    profile_id: String,
    worker_build: String,
    state: mj_core::relay::RelayOperationalState,
    check: ObservationCheck,
) -> Result<bool> {
    let stored = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let job_stored = stored.clone();
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
        let choices = ProfileConfig {
            model: facts.current_model().map(str::to_owned),
            models: mj_core::acp::session_config_choices(&state.config_options, "model"),
            efforts: mj_core::acp::session_config_choices(&state.config_options, "effort"),
            observed_at: chrono::Utc::now().timestamp(),
        };
        // Worker observations do not carry authentication provenance. Claude's
        // setup-token and login catalogues differ, so only probes may cache
        // it. Muse catalogues are never cached at all: its models and efforts
        // change server-side without a managed-pin bump, so a cached copy
        // hides options people expect.
        if matches!(
            profile.kind,
            mj_core::config::HarnessKind::Claude | mj_core::config::HarnessKind::Muse
        ) {
            return Ok(choices);
        }
        let fingerprint = fingerprint(profile);
        match check {
            ObservationCheck::ProbeBuild => {
                let executor = CancellableProcessExecutor::new(cancelled)
                    .with_deadline(Duration::from_secs(30));
                super::worker_binary::worker_binary_for(
                    &TargetLocator::LocalBare {
                        worker_root: String::new(),
                    },
                    &executor,
                )?;
                if mj_core::worker_build::WORKER_BUILD_ID != worker_build {
                    return Ok(choices);
                }
            }
            ObservationCheck::NoProbe => {
                // Nothing else will ever fill the profile's default entry, so
                // the first session observed stands in for it.
                if load(&profile_id, &None, &fingerprint).is_none() {
                    store(&profile_id, &fingerprint, &None, &choices);
                }
            }
        }
        store(&profile_id, &fingerprint, &choices.model, &choices);
        job_stored.store(true, std::sync::atomic::Ordering::Release);
        Ok(choices)
    })
    .await?;
    Ok(stored.load(std::sync::atomic::Ordering::Acquire))
}

/// Cache identity follows configured definitions only. Harness-home
/// files and refreshed credentials do not invalidate advertised choices.
fn fingerprint(profile: &HarnessProfile) -> String {
    profile.capabilities_key("")
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
    discover_from_config(&config, profile_id, model, refresh, cancelled)
}

fn discover_from_config(
    config: &Config,
    profile_id: &str,
    model: Option<String>,
    refresh: bool,
    cancelled: Arc<std::sync::atomic::AtomicBool>,
) -> Result<ProfileConfig> {
    ensure!(
        !cancelled.load(std::sync::atomic::Ordering::Acquire),
        "profile discovery cancelled"
    );
    let profile = config
        .enabled_profile(profile_id)
        .with_context(|| format!("unknown or disabled profile {profile_id:?}"))?;
    let mut environment = profile.environment.resolved().clone();
    let _ = super::worker_binary::apply_claude_setup_token(
        &mut environment,
        profile.kind,
        &mj_core::credentials::claude_oauth_token_path(profile_id),
    );
    // Muse models and efforts change server-side without a managed-pin bump,
    // so a cached catalogue hides options people expect. There is no point
    // caching it: always probe the installation that actually runs, and never
    // write the result back.
    if profile.kind == mj_core::config::HarnessKind::Muse {
        return probe_profile(profile_id, profile, environment, model, cancelled);
    }
    let fingerprint = fingerprint(profile);
    resolve_cached(
        refresh,
        is_current,
        || Ok(load(profile_id, &model, &fingerprint)),
        || {
            probe_profile(
                profile_id,
                profile,
                environment.clone(),
                model.clone(),
                cancelled,
            )
        },
        |choices| {
            if model.is_none() || choices.model == model {
                store(profile_id, &fingerprint, &model, choices);
            }
            store(profile_id, &fingerprint, &choices.model, choices);
            Ok(())
        },
    )
}

/// How long discovered choices are served before discovery probes again.
/// Providers add and retire models without any change Mjolnir can see.
pub(crate) const CHOICES_LIFETIME: Duration = Duration::from_secs(60 * 60);

/// Whether `choices` were observed within [`CHOICES_LIFETIME`].
pub(crate) fn is_current(choices: &ProfileConfig) -> bool {
    is_current_at(choices, chrono::Utc::now().timestamp())
}

fn is_current_at(choices: &ProfileConfig, now: i64) -> bool {
    now.saturating_sub(choices.observed_at) < CHOICES_LIFETIME.as_secs() as i64
}

/// Serve current cached choices, or probe. An out-of-date entry is still
/// served when the probe fails, so an offline or signed-out provider keeps
/// its last known choices instead of losing them.
fn resolve_cached(
    refresh: bool,
    current: impl FnOnce(&ProfileConfig) -> bool,
    load: impl FnOnce() -> Result<Option<ProfileConfig>>,
    probe: impl FnOnce() -> Result<ProfileConfig>,
    save: impl FnOnce(&ProfileConfig) -> Result<()>,
) -> Result<ProfileConfig> {
    let cached = if refresh { None } else { load()? };
    if let Some(choices) = &cached
        && current(choices)
    {
        return Ok(choices.clone());
    }
    match probe() {
        Ok(choices) => {
            save(&choices)?;
            Ok(choices)
        }
        Err(error) => match cached {
            Some(choices) => {
                if !discovery_is_unsupported(&error) {
                    tracing::warn!(error = %format!("{error:#}"), "profile discovery failed; serving out-of-date cached choices");
                }
                Ok(choices)
            }
            None => Err(error),
        },
    }
}

/// Discovery that no retry can make succeed: this machine runs no local
/// worker to probe a profile with. The profile's capabilities stay unknown;
/// its sessions still offer the harness's own choices.
#[derive(Debug)]
pub(crate) struct DiscoveryUnsupported;

impl std::fmt::Display for DiscoveryUnsupported {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(
            "this machine runs no local Mjolnir worker, so it cannot discover the profile's models and efforts before a session starts",
        )
    }
}

impl std::error::Error for DiscoveryUnsupported {}

/// Whether `error` is a discovery that no retry can make succeed.
pub(crate) fn discovery_is_unsupported(error: &anyhow::Error) -> bool {
    error
        .chain()
        .any(|cause| cause.is::<DiscoveryUnsupported>())
}

fn probe_profile(
    profile_id: &str,
    profile: &HarnessProfile,
    environment: BTreeMap<String, String>,
    model: Option<String>,
    cancelled: Arc<std::sync::atomic::AtomicBool>,
) -> Result<ProfileConfig> {
    // Checked first, so a profile that is also not signed in is still
    // reported as the failure no retry can fix.
    if !mj_core::targets::HOST_RUNS_WORKERS {
        return Err(DiscoveryUnsupported.into());
    }
    profile.ensure_ready(profile_id)?;
    let root = tempfile::tempdir().context("create private profile discovery directory")?;
    // Muse and OpenCode read `$XDG_CONFIG_HOME/<id>`, so their staged home
    // must carry that leaf name or the harness never sees the staged login.
    let home = profile
        .kind
        .home_from_environment(root.path().join("profile"));
    super::worker_binary::stage_profile_with_catalog(
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
    let mut environment = environment;
    let excluded_environment = profile.exclude_harness_environment(&mut environment);
    let spec = ProfileProbeSpec {
        harness: profile.kind,
        profile_home: home,
        environment,
        excluded_environment,
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

/// Discovered choices by profile, model (empty for the profile's default)
/// and definition fingerprint. They live only as long as the daemon, so a
/// restart always discovers again.
type ChoiceKey = (String, String, String);
static CHOICES: LazyLock<Mutex<BTreeMap<ChoiceKey, ProfileConfig>>> =
    LazyLock::new(Default::default);

fn choice_key(profile: &str, model: &Option<String>, fingerprint: &str) -> ChoiceKey {
    (
        profile.to_owned(),
        model.clone().unwrap_or_default(),
        fingerprint.to_owned(),
    )
}

/// Each profile's most recently observed model list.
pub(crate) fn advertised_models() -> BTreeMap<String, Vec<mj_core::acp::SessionConfigChoice>> {
    let choices = CHOICES
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut newest = BTreeMap::<&str, &ProfileConfig>::new();
    for ((profile, _, _), observed) in choices.iter() {
        let entry = newest.entry(profile).or_insert(observed);
        if observed.observed_at > entry.observed_at {
            *entry = observed;
        }
    }
    newest
        .into_iter()
        .map(|(profile, observed)| (profile.to_owned(), observed.models.clone()))
        .collect()
}

fn load(profile: &str, model: &Option<String>, fingerprint: &str) -> Option<ProfileConfig> {
    CHOICES
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(&choice_key(profile, model, fingerprint))
        .cloned()
}

fn store(profile: &str, fingerprint: &str, model: &Option<String>, choices: &ProfileConfig) {
    CHOICES
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(choice_key(profile, model, fingerprint), choices.clone());
}

#[cfg(test)]
mod tests {
    use super::*;
    use mj_core::refusal::Refusal;
    use std::cell::{Cell, RefCell};

    // Hard-won: #1284: discovery staged Muse credentials outside its XDG home.
    #[test]
    #[ignore = "requires MJ_MUSE_TEST_HOME with a Muse login and MJ_WORKER_BINARY pointing to the current mj-worker build; discovers models without a provider turn"]
    fn live_muse_profile_discovery_reads_staged_login_and_catalog() {
        let account = std::path::PathBuf::from(
            std::env::var_os("MJ_MUSE_TEST_HOME").expect("set authenticated Muse home"),
        );
        let isolated = tempfile::tempdir().unwrap();
        let home = isolated.path().join("muse");
        std::fs::create_dir(&home).unwrap();
        std::fs::copy(account.join("auth.json"), home.join("auth.json")).unwrap();
        std::fs::write(
            home.join("settings.json"),
            r#"{"schema_version":1,"provider":"meta","model":"muse-spark-1.3"}"#,
        )
        .unwrap();
        // The source home is isolated too: refreshing a staged login must
        // never change the person's own credentials or session data.
        let credentials = std::fs::read(home.join("auth.json")).unwrap();
        let profile = HarnessProfile {
            enabled: true,
            kind: mj_core::config::HarnessKind::Muse,
            home: home.clone(),
            environment: Default::default(),
            context_window_bytes: None,
            guardian_review_model: None,
            codex_profile: None,
            legacy_subagents: None,
        };
        let choices = probe_profile(
            "muse-discovery-1284-test",
            &profile,
            Default::default(),
            None,
            Arc::new(std::sync::atomic::AtomicBool::new(false)),
        )
        .unwrap();
        assert_eq!(choices.model.as_deref(), Some("muse-spark-1.3"));
        assert!(
            choices
                .models
                .iter()
                .any(|choice| choice.value == "muse-spark-1.3")
        );
        assert!(!choices.efforts.is_empty());
        assert_eq!(std::fs::read(home.join("auth.json")).unwrap(), credentials);
    }

    /// Discovery offers the providers of an OpenCode 2 login stored in its
    /// database, and of an OpenCode 1 login in `auth.json` that no OpenCode 2
    /// has taken over, both through the pinned OpenCode's own import.
    #[test]
    #[ignore = "requires MJ_OPENCODE_TEST_BINARY naming an OpenCode 2 executable and MJ_WORKER_BINARY pointing to the current mj-worker build; discovers models without a provider turn"]
    fn live_opencode_profile_discovery_offers_logins_from_its_database() {
        let opencode = std::path::PathBuf::from(
            std::env::var_os("MJ_OPENCODE_TEST_BINARY").expect("set an OpenCode 2 executable"),
        );
        let isolated = tempfile::tempdir().unwrap();
        let stored = isolated.path().join("stored/opencode");
        std::fs::create_dir_all(&stored).unwrap();
        // OpenCode itself writes the login, so the read meets its own schema.
        let export = isolated.path().join("export.json");
        std::fs::write(
            &export,
            r#"[{"id":"cred_mjolnir_live_test","integrationID":"deepseek","label":"DeepSeek","active":true,"value":{"type":"key","key":"sk-mjolnir-live-test"}}]"#,
        )
        .unwrap();
        let status = std::process::Command::new(&opencode)
            .args(["auth", "import", "--standalone"])
            .arg(&export)
            .env("XDG_CONFIG_HOME", isolated.path().join("stored"))
            .env("XDG_DATA_HOME", stored.join(".data"))
            .env("OPENCODE_DISABLE_AUTOUPDATE", "1")
            .status()
            .unwrap();
        assert!(status.success(), "seed the OpenCode login: {status}");
        let legacy = isolated.path().join("legacy/opencode");
        std::fs::create_dir_all(legacy.join(".data/opencode")).unwrap();
        std::fs::write(
            legacy.join(".data/opencode/auth.json"),
            r#"{"deepseek":{"type":"api","key":"sk-mjolnir-live-legacy"}}"#,
        )
        .unwrap();
        for home in [stored, legacy] {
            let profile = HarnessProfile {
                enabled: true,
                kind: mj_core::config::HarnessKind::OpenCode,
                home: home.clone(),
                environment: Default::default(),
                context_window_bytes: None,
                guardian_review_model: None,
                codex_profile: None,
                legacy_subagents: None,
            };
            let choices = probe_profile(
                "opencode-database-login-test",
                &profile,
                Default::default(),
                None,
                Arc::new(std::sync::atomic::AtomicBool::new(false)),
            )
            .unwrap();
            assert!(
                choices
                    .models
                    .iter()
                    .any(|choice| choice.value.starts_with("deepseek/")),
                "{}: discovered models: {:?}",
                home.display(),
                choices.models
            );
        }
    }

    #[tokio::test]
    async fn unsaved_profile_discovery_reads_the_owned_cache_without_saving_settings() {
        const CHILD: &str = "MJ_TEST_UNSAVED_PROFILE_DISCOVERY";
        if std::env::var_os(CHILD).is_none() {
            let root = tempfile::tempdir().unwrap();
            crate::controller::test_support::IsolatedTest::new(
                crate::controller::test_support::test_name(
                    module_path!(),
                    "unsaved_profile_discovery_reads_the_owned_cache_without_saving_settings",
                ),
            )
            .env(CHILD, "1")
            .env("MJ_INSTANCE", "unsaved-profile-discovery-test")
            .isolated_store(root.path())
            .run();
            return;
        }
        let _writer = crate::database::install_isolated_test_writer();
        Config::default().save().unwrap();
        let home = tempfile::tempdir().unwrap();
        let profile = HarnessProfile {
            enabled: true,
            kind: mj_core::config::HarnessKind::Codex,
            home: home.path().into(),
            environment: Default::default(),
            context_window_bytes: None,
            guardian_review_model: None,
            codex_profile: None,
            legacy_subagents: None,
        };
        let key = fingerprint(&profile);
        let choice = |value: &str| mj_core::acp::SessionConfigChoice {
            value: value.into(),
            name: value.into(),
            description: None,
        };
        let catalog = ProfileConfig {
            model: Some("chosen".into()),
            models: vec![choice("chosen")],
            efforts: vec![choice("high")],
            observed_at: chrono::Utc::now().timestamp(),
        };
        store("unsaved", &key, &None, &catalog);
        store("unsaved", &key, &catalog.model, &catalog);
        let mut draft = Config::default();
        draft.profiles.insert("unsaved".into(), profile);
        assert!(!draft.subagents.is_configured());
        draft.subagents.straightforward = Some(mj_core::config::SubagentChoice {
            model: "chosen".into(),
            effort: Some("high".into()),
        });
        for (model, effort, available) in [
            ("chosen", Some("high"), true),
            ("chosen", None, true),
            ("missing", None, false),
            ("chosen", Some("missing"), false),
            ("chosen", Some("adaptive"), false),
        ] {
            let policy = mj_core::subagent::SubagentPolicy::Mjolnir {
                straightforward: None,
                subtle: Some(Box::new(mj_core::config::SubagentChoice {
                    model: model.into(),
                    effort: effort.map(str::to_owned),
                })),
            };
            let result = validate_session_subagent_policy(&draft, "unsaved", &policy).await;
            if available {
                result.unwrap();
            } else {
                let error = result.unwrap_err();
                assert_eq!(
                    Refusal::of(&error).unwrap().code(),
                    Some(mj_core::subagent::CHOICE_UNAVAILABLE_CODE)
                );
            }
        }
        validate_session_subagent_policy(
            &draft,
            "unsaved",
            &mj_core::subagent::SubagentPolicy::Mjolnir {
                straightforward: None,
                subtle: Some(Box::new(mj_core::config::SubagentChoice {
                    model: "chosen".into(),
                    effort: None,
                })),
            },
        )
        .await
        .unwrap();
        // Subtle is optional: the straightforward choice alone is enough.
        validate_session_subagent_policy(
            &draft,
            "unsaved",
            &mj_core::subagent::SubagentPolicy::Mjolnir {
                straightforward: None,
                subtle: None,
            },
        )
        .await
        .unwrap();
        draft.subagents.straightforward = None;
        let missing = validate_session_subagent_policy(
            &draft,
            "unsaved",
            &mj_core::subagent::SubagentPolicy::Mjolnir {
                straightforward: None,
                subtle: Some(Box::new(mj_core::config::SubagentChoice {
                    model: "chosen".into(),
                    effort: None,
                })),
            },
        )
        .await
        .unwrap_err();
        assert_eq!(
            Refusal::of(&missing).unwrap().code(),
            Some(mj_core::subagent::CHOICE_UNAVAILABLE_CODE)
        );
        assert!(
            missing.to_string().contains("[subagents.straightforward]"),
            "subtle alone is never enough: {missing:#}"
        );
        let options = subagent_options_for(draft, "unsaved".into(), Some("chosen".into()))
            .await
            .unwrap();
        assert_eq!(options.models, catalog.models);
        assert_eq!(options.efforts, catalog.efforts);
        assert!(options.unavailable.is_empty());
        assert!(!Config::load().unwrap().profiles.contains_key("unsaved"));
    }

    #[test]
    fn harness_home_files_do_not_invalidate_configured_capabilities() {
        let home = tempfile::tempdir().unwrap();
        let profile = HarnessProfile {
            enabled: true,
            kind: mj_core::config::HarnessKind::Codex,
            home: home.path().into(),
            environment: Default::default(),
            context_window_bytes: None,
            legacy_subagents: Default::default(),
            guardian_review_model: None,
            codex_profile: None,
        };
        let key = || fingerprint(&profile);

        let built_in = key();
        std::fs::write(
            home.path().join("config.toml"),
            "model = \"glm-5.3\"\n\
             model_provider = \"zai\"\n\
             \n\
             [model_providers.zai]\n\
             base_url = \"https://api.z.ai/api/v1\"\n\
             env_key = \"ZAI_API_KEY\"\n\
             wire_api = \"responses\"\n",
        )
        .unwrap();
        let provider = key();
        assert_eq!(
            built_in, provider,
            "only configured profile definitions invalidate capabilities"
        );
        std::fs::write(
            home.path().join("models.json"),
            r#"{"models":[{"slug":"glm-5.3-flash"}]}"#,
        )
        .unwrap();
        assert_eq!(
            provider,
            key(),
            "a home catalog edit does not invalidate capabilities"
        );
    }

    #[test]
    fn discovery_fingerprint_ignores_creation_defaults_and_unrelated_profile_preferences() {
        let home = tempfile::tempdir().unwrap();
        let mut profile = HarnessProfile {
            enabled: true,
            kind: mj_core::config::HarnessKind::Codex,
            home: home.path().into(),
            environment: Default::default(),
            context_window_bytes: None,
            legacy_subagents: Default::default(),
            guardian_review_model: None,
            codex_profile: None,
        };
        let original = fingerprint(&profile);
        for _ in 0..2 {
            profile.context_window_bytes = Some(42);
            profile.guardian_review_model = Some("session".into());
            assert_eq!(fingerprint(&profile), original);
        }
        profile.environment = [("PROVIDER".into(), "different".into())]
            .into_iter()
            .collect();
        assert_ne!(fingerprint(&profile), original);
    }

    #[test]
    fn setup_token_rotation_preserves_configured_capabilities() {
        use mj_core::credentials::{CLAUDE_OAUTH_TOKEN_ENV, write_claude_oauth_token};
        let root = tempfile::tempdir().unwrap();
        let token = root.path().join("token");
        let profile = HarnessProfile {
            enabled: true,
            kind: mj_core::config::HarnessKind::Claude,
            home: root.path().into(),
            environment: Default::default(),
            context_window_bytes: None,
            legacy_subagents: Default::default(),
            guardian_review_model: None,
            codex_profile: None,
        };
        let resolve = |environment: BTreeMap<String, String>| {
            let mut environment = environment;
            let _ = super::super::worker_binary::apply_claude_setup_token(
                &mut environment,
                profile.kind,
                &token,
            );
            environment
        };
        let login = fingerprint(&profile);
        write_claude_oauth_token(&token, b"setup-first").unwrap();
        let first = resolve(BTreeMap::new());
        assert_eq!(first[CLAUDE_OAUTH_TOKEN_ENV], "setup-first");
        let first_key = fingerprint(&profile);
        assert_eq!(login, first_key);
        write_claude_oauth_token(&token, b"setup-second").unwrap();
        assert_eq!(
            first[CLAUDE_OAUTH_TOKEN_ENV], "setup-first",
            "an in-flight probe retains its authentication snapshot"
        );
        assert_eq!(first_key, fingerprint(&profile));
        let explicit = BTreeMap::from([(CLAUDE_OAUTH_TOKEN_ENV.into(), "explicit".into())]);
        assert_eq!(resolve(explicit.clone()), explicit);
        assert!(!first_key.contains("setup-first"));
    }

    #[tokio::test]
    async fn retiring_a_definition_cancels_probes_and_preserves_its_replacement() {
        let cancellation = CancellationToken::new();
        let (started, running) = tokio::sync::oneshot::channel();
        let first = tokio::spawn(serialized_until(
            "retirement-test".into(),
            cancellation.clone(),
            move |cancelled| {
                started.send(()).unwrap();
                while !cancelled.load(std::sync::atomic::Ordering::Acquire) {
                    std::thread::sleep(Duration::from_millis(1));
                }
                anyhow::bail!("running probe cancelled")
            },
        ));
        running.await.unwrap();
        let queued_ran = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let marker = queued_ran.clone();
        let queued = serialized_until("retirement-test".into(), cancellation.clone(), move |_| {
            marker.store(true, std::sync::atomic::Ordering::Release);
            Ok(ProfileConfig {
                model: None,
                models: vec![],
                efforts: vec![],
                observed_at: 0,
            })
        });
        tokio::pin!(queued);
        tokio::select! {
            biased;
            _ = &mut queued => panic!("queued probe passed the running owner"),
            _ = tokio::task::yield_now() => {}
        }
        cancellation.cancel();
        let queued_result = tokio::time::timeout(Duration::from_secs(1), &mut queued)
            .await
            .unwrap();
        assert!(queued_result.is_err());
        assert!(!queued_ran.load(std::sync::atomic::Ordering::Acquire));
        assert!(
            tokio::time::timeout(Duration::from_secs(1), first)
                .await
                .unwrap()
                .unwrap()
                .is_err()
        );
        let replacement =
            serialized_until("retirement-test".into(), CancellationToken::new(), |_| {
                Ok(ProfileConfig {
                    model: None,
                    models: vec![],
                    efforts: vec![],
                    observed_at: 0,
                })
            });
        tokio::time::timeout(Duration::from_secs(1), replacement)
            .await
            .unwrap()
            .unwrap();
    }

    #[test]
    fn runtime_shutdown_cancels_a_probes_blocking_job_even_if_its_supervisor_is_dropped() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let (started, running) = tokio::sync::oneshot::channel();
        let saw_cancellation = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stopped = saw_cancellation.clone();
        runtime.block_on(async move {
            tokio::spawn(serialized_until(
                "runtime-drop-test".into(),
                CancellationToken::new(),
                move |cancelled| {
                    started.send(()).unwrap();
                    let deadline = std::time::Instant::now() + Duration::from_secs(3);
                    while !cancelled.load(std::sync::atomic::Ordering::Acquire)
                        && std::time::Instant::now() < deadline
                    {
                        std::thread::sleep(Duration::from_millis(1));
                    }
                    stopped.store(
                        cancelled.load(std::sync::atomic::Ordering::Acquire),
                        std::sync::atomic::Ordering::Release,
                    );
                    Ok(ProfileConfig {
                        model: None,
                        models: vec![],
                        efforts: vec![],
                        observed_at: 0,
                    })
                },
            ));
            running.await.unwrap();
        });
        drop(runtime);
        assert!(saw_cancellation.load(std::sync::atomic::Ordering::Acquire));
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
                        |_| true,
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
                |_| true,
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
                |_| true,
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

    #[test]
    fn choices_expire_after_an_hour() {
        let lifetime = CHOICES_LIFETIME.as_secs() as i64;
        let at = |observed_at| ProfileConfig {
            model: None,
            models: vec![],
            efforts: vec![],
            observed_at,
        };
        assert!(is_current_at(&at(1_000), 1_000 + lifetime - 1));
        assert!(!is_current_at(&at(1_000), 1_000 + lifetime));
    }

    #[test]
    fn out_of_date_choices_are_probed_again_and_kept_when_the_probe_fails() {
        let stale = ProfileConfig {
            model: Some("old".into()),
            models: vec![],
            efforts: vec![],
            observed_at: 1,
        };
        let fresh = ProfileConfig {
            model: Some("new".into()),
            observed_at: 2,
            ..stale.clone()
        };
        let saved = RefCell::new(None);
        let resolved = resolve_cached(
            false,
            |choices| choices.observed_at > 1,
            || Ok(Some(stale.clone())),
            || Ok(fresh.clone()),
            |choices| {
                *saved.borrow_mut() = Some(choices.clone());
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(resolved, fresh);
        assert_eq!(saved.borrow().as_ref(), Some(&fresh));
        let resolved = resolve_cached(
            false,
            |_| false,
            || Ok(Some(stale.clone())),
            || anyhow::bail!("provider offline"),
            |_| panic!("a failed probe saves nothing"),
        )
        .unwrap();
        assert_eq!(resolved, stale);
    }

    /// Muse models and efforts change server-side without a managed-pin bump,
    /// so caching a worker's observed catalogue hides options people expect.
    /// Muse observations must leave the cache untouched while other
    /// harnesses keep caching theirs.
    #[tokio::test]
    async fn muse_worker_observations_are_never_cached() {
        const CHILD: &str = "MJ_TEST_MUSE_OBSERVATIONS_UNCACHED";
        if std::env::var_os(CHILD).is_none() {
            let root = tempfile::tempdir().unwrap();
            crate::controller::test_support::IsolatedTest::new(
                crate::controller::test_support::test_name(
                    module_path!(),
                    "muse_worker_observations_are_never_cached",
                ),
            )
            .env(CHILD, "1")
            .env("MJ_INSTANCE", "muse-observations-uncached-test")
            .isolated_store(root.path())
            .run();
            return;
        }
        use agent_client_protocol::schema::v1::{
            SessionConfigOption, SessionConfigOptionCategory, SessionConfigSelectOption,
            SessionConfigSelectOptions,
        };
        use mj_core::relay::{
            RELAY_EVENT_GENESIS_DIGEST, RelayExecutionState, RelayOperationalState,
        };

        let _writer = crate::database::install_isolated_test_writer();
        let root = tempfile::tempdir().unwrap();
        // A Muse home must end in /muse, its XDG configuration directory.
        let home = root.path().join("muse");
        std::fs::create_dir(&home).unwrap();
        std::fs::write(
            home.join("settings.json"),
            br#"{"model":"muse-spark-1.3-contributor"}"#,
        )
        .unwrap();
        let profile = |kind: mj_core::config::HarnessKind| HarnessProfile {
            enabled: true,
            kind,
            home: if kind == mj_core::config::HarnessKind::Muse {
                home.clone()
            } else {
                root.path().into()
            },
            environment: Default::default(),
            context_window_bytes: None,
            guardian_review_model: None,
            codex_profile: None,
            legacy_subagents: None,
        };
        let mut config = Config::default();
        config
            .profiles
            .insert("muse".into(), profile(mj_core::config::HarnessKind::Muse));
        config
            .profiles
            .insert("codex".into(), profile(mj_core::config::HarnessKind::Codex));
        config.save().unwrap();

        fn observed() -> RelayOperationalState {
            RelayOperationalState {
                assessment: None,
                assessment_context: None,
                turn_completion: None,
                continuation: Default::default(),
                relay_protocol_version: Some(mj_core::relay::RELAY_PROTOCOL_VERSION),
                native_agents: Vec::new(),
                steering: None,
                cancelling_prompt_id: None,
                clear_context: false,
                clear_context_started_at_ms: None,
                native_agent_count: 0,
                expected_continuation: None,
                inferred_idle_since_ms: None,
                task_settled_at_ms: None,
                background_needed: None,
                goal: Default::default(),
                capacity_retry: None,
                retry_assessment_pending: false,
                store_id: None,
                idle_since_ms: None,
                session_id: "observed".to_owned(),
                execution: RelayExecutionState::Idle,
                latest_ordinal: 0,
                latest_digest: RELAY_EVENT_GENESIS_DIGEST.to_owned(),
                acknowledged_through: 0,
                acknowledged_digest: RELAY_EVENT_GENESIS_DIGEST.to_owned(),
                recovery_floor_ordinal: 0,
                recovery_floor_digest: RELAY_EVENT_GENESIS_DIGEST.to_owned(),
                native_session_id: Some("observed".to_owned()),
                native_continuity_lost: false,
                replaced_unused_native_session_id: None,
                checkpoint_only: false,
                acp_ready: None,
                harness_preparation: None,
                agent_capabilities: None,
                agent_info: None,
                runtime: None,
                steering_supported: None,
                turn_controls: None,
                config_options: vec![
                    SessionConfigOption::select(
                        "effort".to_owned(),
                        "Effort".to_owned(),
                        "low".to_owned(),
                        SessionConfigSelectOptions::Ungrouped(vec![
                            SessionConfigSelectOption::new("low".to_owned(), "Low".to_owned()),
                            SessionConfigSelectOption::new("high".to_owned(), "High".to_owned()),
                        ]),
                    )
                    .category(SessionConfigOptionCategory::ThoughtLevel),
                ],
                modes: None,
                available_commands: Vec::new(),
                config: BTreeMap::new(),
                active_prompt: None,
                queued_prompts: Vec::new(),
                active_user_shells: Vec::new(),
                active_agent_terminals: Vec::new(),
                checkpoint_barrier: None,
                checkpoint_ready: None,
                last_acp_activity_at_ms: None,
                activity_turn_started_at_ms: None,
                current_step_started_at_ms: None,
                foreground_tool_started_at_ms: None,
                tools_in_flight: Vec::new(),
                activity: None,
                harness_turn: None,
                last_harness_turn_started_ordinal: None,
                background_commands: Vec::new(),
                background_work_known: None,
            }
        }

        // A stale row from before the bypass must survive the observation
        // byte-identical: nothing may refresh it, and no model-keyed row may
        // appear beside it.
        let stale = ProfileConfig {
            model: Some("stale-model".into()),
            models: Vec::new(),
            efforts: Vec::new(),
            observed_at: 1,
        };
        let muse_fingerprint = fingerprint(config.enabled_profile("muse").unwrap());
        store("muse", &muse_fingerprint, &None, &stale);
        let cached_before = load("muse", &None, &muse_fingerprint);

        assert!(
            !observe(
                "muse".into(),
                String::new(),
                observed(),
                ObservationCheck::NoProbe,
            )
            .await
            .unwrap(),
            "a Muse observation caches nothing"
        );
        assert_eq!(load("muse", &None, &muse_fingerprint), cached_before,);
        assert!(
            load(
                "muse",
                &Some("muse-spark-1.3-contributor".into()),
                &muse_fingerprint,
            )
            .is_none(),
            "a Muse observation stores no model-keyed row either"
        );

        // Other harnesses still cache worker observations.
        assert!(
            observe(
                "codex".into(),
                String::new(),
                observed(),
                ObservationCheck::NoProbe,
            )
            .await
            .unwrap()
        );
        let codex_fingerprint = fingerprint(config.enabled_profile("codex").unwrap());
        assert!(load("codex", &None, &codex_fingerprint).is_some());
    }
    #[tokio::test]
    async fn create_and_move_mjolnir_policy_refuse_unconfigured_and_unsupported_parents() {
        for kind in [
            mj_core::config::HarnessKind::Codex,
            mj_core::config::HarnessKind::Dsh,
            mj_core::config::HarnessKind::Grok,
        ] {
            let mut config = Config::default();
            config.profiles.insert(
                "parent".into(),
                HarnessProfile {
                    enabled: true,
                    kind,
                    home: "/fixture".into(),
                    environment: Default::default(),
                    context_window_bytes: None,
                    guardian_review_model: None,
                    codex_profile: None,
                    legacy_subagents: None,
                },
            );
            let error = validate_session_subagent_policy(
                &config,
                "parent",
                &mj_core::subagent::SubagentPolicy::Mjolnir {
                    straightforward: None,
                    subtle: None,
                },
            )
            .await
            .unwrap_err();
            let refusal = Refusal::of(&error).unwrap();
            assert_eq!(
                refusal.code(),
                Some(mj_core::subagent::CHOICE_UNAVAILABLE_CODE)
            );
            if kind.supports_delegation_tools() {
                assert!(refusal.message().contains("[subagents.straightforward]"));
                assert!(refusal.message().contains("--subagent-straightforward"));
            } else {
                assert!(refusal.message().contains("Claude, Codex, or DSH"));
            }
            // Native keeps a harness's own sub-agents; DSH has none, so an
            // explicit Native is refused with the two choices it offers.
            let native = validate_session_subagent_policy(
                &config,
                "parent",
                &mj_core::subagent::SubagentPolicy::Native,
            )
            .await;
            if kind.has_native_subagents() {
                native.unwrap();
            } else {
                let error = native.unwrap_err();
                let refusal = Refusal::of(&error).unwrap();
                assert_eq!(refusal.code(), None, "the Settings remedy does not apply");
                assert!(
                    refusal.message().contains("choose Mjolnir or None"),
                    "{}",
                    refusal.message()
                );
                // The default for an omitted policy is accepted.
                validate_session_subagent_policy(
                    &config,
                    "parent",
                    &mj_core::subagent::SubagentPolicy::default_for(kind),
                )
                .await
                .unwrap();
            }
        }
    }
}
