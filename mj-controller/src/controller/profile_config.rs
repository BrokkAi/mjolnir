//! Automatically populated, persistent profile capabilities.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::Duration;

use crate::targets::{CancellableProcessExecutor, CommandExecutor, CommandSpec, TargetLocator};
use anyhow::{Context, Result, ensure};
use mj_core::config::{Config, HarnessProfile};
use mj_core::worker_launch::{ProfileConfig, ProfileProbeSpec};
use tokio_util::sync::CancellationToken;

/// Pre-session choices use the same eligibility predicate as delegation.
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

/// Validates a newly selected top-level policy, not a recorded resume policy.
/// Multi-model is retired for new selections. Only Claude and
/// Codex take Mjolnir's delegation tools, and a single-model policy must name
/// a model and effort that discovery offers now.
pub async fn validate_session_subagent_policy(
    config: &Config,
    profile_id: &str,
    policy: &mj_core::subagent::SubagentPolicy,
) -> Result<()> {
    let kind = config
        .enabled_profile(profile_id)
        .context("parent profile unavailable")?
        .kind;
    refuse_unsupported_policy(kind, policy)?;
    if let mj_core::subagent::SubagentPolicy::SingleModel { model, .. } = policy {
        let options =
            subagent_options_for(config.clone(), profile_id.to_owned(), Some(model.clone()))
                .await?;
        refuse_unavailable_choice(&options, policy)?;
    }
    Ok(())
}

/// These two failures name something in the request the caller can change, so
/// they are refusals: the HTTP API answers 422 with the sentence, where an
/// unmarked error would be a 500 that hides it in the daemon log.
fn refuse_unsupported_policy(
    kind: mj_core::config::HarnessKind,
    policy: &mj_core::subagent::SubagentPolicy,
) -> Result<()> {
    if *policy == mj_core::subagent::SubagentPolicy::AllModels {
        return Err(anyhow::Error::new(mj_core::refusal::Refusal::unusable(
            "Mjolnir multi-model subagents are no longer available for new selections; use native, single_model, or none",
        )));
    }
    if kind.supports_delegation_tools() || *policy == mj_core::subagent::SubagentPolicy::Native {
        return Ok(());
    }
    Err(anyhow::Error::new(mj_core::refusal::Refusal::unusable(
        "subagent policies are supported only by Claude and Codex",
    )))
}

fn refuse_unavailable_choice(
    options: &mj_core::subagent::SubagentOptions,
    policy: &mj_core::subagent::SubagentPolicy,
) -> Result<()> {
    options.validate(policy).map_err(|message| {
        anyhow::Error::new(
            mj_core::refusal::Refusal::unusable(message)
                .with_code(mj_core::subagent::CHOICE_UNAVAILABLE_CODE),
        )
    })
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
        .filter(|(id, profile)| profile.enabled && config.subagents.profile_is_eligible(parent, id))
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
            store(&profile_id, &fingerprint(profile), &choices.model, &choices)?;
        }
        Ok(choices)
    })
    .await
    .map(|_| ())
}

/// Persistent identity follows configured definitions only. Harness-home
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
    super::worker_binary::apply_claude_setup_token(
        &mut environment,
        profile.kind,
        &mj_core::credentials::claude_oauth_token_path(profile_id),
    );
    let fingerprint = fingerprint(profile);
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
    let home = root.path().join("profile");
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
    use mj_core::refusal::Refusal;
    use mj_core::subagent::{SubagentOptions, SubagentPolicy};
    use std::cell::{Cell, RefCell};

    fn refusal_message(error: &anyhow::Error) -> Option<String> {
        Refusal::of(error).map(|refusal| refusal.message().to_owned())
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
            subagents: SubagentPolicy::SingleModel {
                model: "chosen".into(),
                effort: Some("high".into()),
            },
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
            observed_at: 1,
        };
        store("unsaved", &key, &None, &catalog).unwrap();
        store("unsaved", &key, &catalog.model, &catalog).unwrap();
        let mut draft = Config::default();
        draft.profiles.insert("unsaved".into(), profile);
        let options = subagent_options_for(draft, "unsaved".into(), Some("chosen".into()))
            .await
            .unwrap();
        assert_eq!(options.models, catalog.models);
        assert_eq!(options.efforts, catalog.efforts);
        assert!(options.unavailable.is_empty());
        assert!(!Config::load().unwrap().profiles.contains_key("unsaved"));
    }

    #[test]
    fn a_policy_on_a_harness_without_delegation_is_refused_with_its_message() {
        let error =
            refuse_unsupported_policy(mj_core::config::HarnessKind::Grok, &SubagentPolicy::None)
                .unwrap_err();
        assert_eq!(
            refusal_message(&error).as_deref(),
            Some("subagent policies are supported only by Claude and Codex")
        );
        assert!(
            refuse_unsupported_policy(mj_core::config::HarnessKind::Grok, &SubagentPolicy::Native)
                .is_ok()
        );
    }

    #[tokio::test]
    async fn new_policies_accept_native_and_none_but_refuse_multi_model_without_discovery() {
        let mut config = Config::default();
        config.profiles.insert(
            "parent".into(),
            HarnessProfile {
                enabled: true,
                kind: mj_core::config::HarnessKind::Codex,
                home: "missing-test-profile-home".into(),
                environment: Default::default(),
                context_window_bytes: None,
                guardian_review_model: None,
                subagents: SubagentPolicy::Native,
            },
        );
        for policy in [SubagentPolicy::Native, SubagentPolicy::None] {
            validate_session_subagent_policy(&config, "parent", &policy)
                .await
                .unwrap();
        }
        let error = validate_session_subagent_policy(&config, "parent", &SubagentPolicy::AllModels)
            .await
            .unwrap_err();
        let refusal = Refusal::of(&error).expect("a request refusal, not a discovery failure");
        assert_eq!(refusal.kind(), mj_core::refusal::RefusalKind::Unusable);
        assert!(refusal.message().contains("no longer available"));
        assert!(refusal.message().contains("native, single_model, or none"));
    }

    #[test]
    fn an_unavailable_model_or_effort_is_refused_with_its_message() {
        let unavailable = SubagentPolicy::SingleModel {
            model: "fake-child-model".into(),
            effort: None,
        };
        let error =
            refuse_unavailable_choice(&SubagentOptions::default(), &unavailable).unwrap_err();
        let message = refusal_message(&error).expect("a refusal, not an internal error");
        assert!(
            message.contains("\"fake-child-model\" is unavailable"),
            "{message}"
        );
        // Clients choose the remedy from the code, not from the sentence.
        assert_eq!(
            Refusal::of(&error).and_then(|refusal| refusal.code()),
            Some(mj_core::subagent::CHOICE_UNAVAILABLE_CODE)
        );

        // A model that is offered but a missing effort is refused the same way.
        let choice = |value: &str| mj_core::acp::SessionConfigChoice {
            value: value.into(),
            name: value.into(),
            description: None,
        };
        let options = SubagentOptions {
            models: vec![choice("haiku")],
            efforts: vec![choice("high")],
            unavailable: Vec::new(),
        };
        let policy = SubagentPolicy::SingleModel {
            model: "haiku".into(),
            effort: Some("low".into()),
        };
        let error = refuse_unavailable_choice(&options, &policy).unwrap_err();
        assert!(refusal_message(&error).is_some(), "{error:#}");
    }

    #[tokio::test]
    async fn subagent_options_use_eligible_profiles_and_model_specific_efforts() {
        let mut config = Config::default();
        for id in ["parent", "eligible", "disabled", "excluded", "broken"] {
            config.profiles.insert(
                id.into(),
                HarnessProfile {
                    enabled: id != "disabled",
                    kind: mj_core::config::HarnessKind::Codex,
                    home: std::path::PathBuf::from("/unused"),
                    environment: Default::default(),
                    context_window_bytes: None,
                    subagents: Default::default(),
                    guardian_review_model: None,
                },
            );
        }
        config.subagents.eligible_profiles = BTreeMap::from([
            ("eligible".into(), true),
            ("disabled".into(), true),
            ("broken".into(), true),
        ]);
        let choices = |values: &[&str]| {
            values
                .iter()
                .map(|value| mj_core::acp::SessionConfigChoice {
                    value: (*value).into(),
                    name: (*value).into(),
                    description: None,
                })
                .collect()
        };
        let probe = |id: String, model: Option<String>| async move {
            assert!(!matches!(id.as_str(), "disabled" | "excluded"));
            anyhow::ensure!(id != "broken", "profile cannot sign in");
            Ok(ProfileConfig {
                model: model.clone(),
                models: choices(if id == "parent" {
                    &["parent-model"]
                } else {
                    &["child-model"]
                }),
                efforts: choices(if model.as_deref() == Some("child-model") {
                    &["high"]
                } else {
                    &["low"]
                }),
                observed_at: 1,
            })
        };
        let options = subagent_options_with(&config, "parent", Some("child-model".into()), probe)
            .await
            .unwrap();
        assert_eq!(
            options
                .models
                .iter()
                .map(|choice| choice.value.as_str())
                .collect::<Vec<_>>(),
            ["child-model", "parent-model"]
        );
        assert_eq!(
            options
                .efforts
                .iter()
                .map(|choice| choice.value.as_str())
                .collect::<Vec<_>>(),
            ["high"]
        );
        assert_eq!(options.unavailable, ["broken: profile cannot sign in"]);
        assert!(
            options
                .validate(&mj_core::subagent::SubagentPolicy::SingleModel {
                    model: "child-model".into(),
                    effort: Some("high".into())
                })
                .is_ok()
        );
        assert!(
            options
                .validate(&mj_core::subagent::SubagentPolicy::SingleModel {
                    model: "child-model".into(),
                    effort: Some("low".into())
                })
                .is_err()
        );
    }

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
            environment: Default::default(),
            context_window_bytes: None,
            subagents: Default::default(),
            guardian_review_model: None,
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
    fn harness_home_files_do_not_invalidate_configured_capabilities() {
        let home = tempfile::tempdir().unwrap();
        let profile = HarnessProfile {
            enabled: true,
            kind: mj_core::config::HarnessKind::Codex,
            home: home.path().into(),
            environment: Default::default(),
            context_window_bytes: None,
            subagents: Default::default(),
            guardian_review_model: None,
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
            subagents: Default::default(),
            guardian_review_model: None,
        };
        let original = fingerprint(&profile);
        for effort in ["low", "high"] {
            profile.subagents = SubagentPolicy::SingleModel {
                model: "chosen".into(),
                effort: Some(effort.into()),
            };
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
            subagents: Default::default(),
            guardian_review_model: None,
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
