//! One daemon-owned cache, warmed independently of settings and tool calls.
//! Entries belong to configured profile identities, never a shared generation.
//! Every reader joins the owner's attempt; only its supervisor retries failures.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use futures::future::{FutureExt, Shared, TryFutureExt, join_all};
use mj_client::session::BoxFuture;
use mj_core::config::{Config, HarnessKind};
#[cfg(test)]
use mj_core::config::{HarnessProfile, SubagentConfig};
use mj_core::profile_capabilities::{
    CapabilityState, ProfileCapabilities, ProfileCapabilitiesSnapshot,
};
use mj_core::worker_launch::ProfileConfig;
use tokio_util::sync::CancellationToken;

#[cfg(test)]
pub(crate) type Probe = dyn Fn(String) -> BoxFuture<'static, Result<ProfileConfig>> + Send + Sync;
#[cfg(test)]
type ModelProbe = dyn Fn(String, String) -> BoxFuture<'static, Result<ProfileConfig>> + Send + Sync;
type Attempt = Shared<BoxFuture<'static, Result<ProfileConfig, Arc<str>>>>;
type CacheKey = (String, Option<String>);

#[derive(Clone)]
enum Entry {
    Pending(Attempt),
    Ready(ProfileConfig),
    Failed { attempt: Attempt, error: Arc<str> },
}
impl Entry {
    fn state(&self) -> CapabilityState<ProfileConfig> {
        match self {
            Self::Pending(_) => CapabilityState::Pending,
            Self::Ready(choices) => CapabilityState::Ready(choices.clone()),
            Self::Failed { error, .. } => CapabilityState::Failed(error.to_string()),
        }
    }
    fn owns(&self, attempt: &Attempt) -> bool {
        match self {
            Self::Pending(current)
            | Self::Failed {
                attempt: current, ..
            } => current.ptr_eq(attempt),
            Self::Ready(_) => false,
        }
    }
}
#[derive(Clone)]
struct Definition {
    id: String,
    config: Arc<Config>,
    cancellation: CancellationToken,
}
#[derive(Default)]
struct Inner {
    live: Option<Config>,
    definitions: BTreeMap<String, Definition>,
    entries: BTreeMap<CacheKey, Entry>,
}

pub(crate) struct ProfileCatalog {
    cancellation: CancellationToken,
    inner: Mutex<Inner>,
    publisher: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
    revisions: tokio::sync::watch::Sender<u64>,
    #[cfg(test)]
    probe: Option<(Arc<Probe>, Arc<ModelProbe>)>,
}
impl ProfileCatalog {
    pub(crate) fn new(cancellation: CancellationToken) -> Arc<Self> {
        Arc::new(Self {
            cancellation,
            inner: Mutex::new(Inner::default()),
            publisher: Mutex::new(None),
            revisions: tokio::sync::watch::channel(0).0,
            #[cfg(test)]
            probe: None,
        })
    }

    pub(crate) fn set_publisher(&self, publisher: Arc<dyn Fn() + Send + Sync>) {
        *self
            .publisher
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(publisher);
    }

    fn publish(&self) {
        self.revisions
            .send_modify(|revision| *revision = revision.wrapping_add(1));
        let publisher = self
            .publisher
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        if let Some(publisher) = publisher {
            publisher();
        }
    }

    pub(crate) fn snapshot(&self) -> ProfileCapabilitiesSnapshot {
        let inner = self.lock();
        ProfileCapabilitiesSnapshot {
            profiles: inner
                .definitions
                .keys()
                .map(|identity| {
                    let choices = inner
                        .entries
                        .get(&(identity.clone(), None))
                        .map_or(CapabilityState::Pending, Entry::state);
                    let efforts = match &choices {
                        CapabilityState::Ready(choices) => choices
                            .models
                            .iter()
                            .map(|model| {
                                let state = match inner
                                    .entries
                                    .get(&(identity.clone(), Some(model.value.clone())))
                                {
                                    Some(Entry::Ready(choices)) => {
                                        CapabilityState::Ready(choices.efforts.clone())
                                    }
                                    Some(Entry::Failed { error, .. }) => {
                                        CapabilityState::Failed(error.to_string())
                                    }
                                    _ => CapabilityState::Pending,
                                };
                                (model.value.clone(), state)
                            })
                            .collect(),
                        _ => BTreeMap::new(),
                    };
                    (identity.clone(), ProfileCapabilities { choices, efforts })
                })
                .collect(),
        }
    }

    /// Adoption updates eligibility without invalidating capability entries.
    pub(crate) fn sync(self: &Arc<Self>, config: &Config) {
        let changed = {
            let mut inner = self.lock();
            let changed = inner.live.as_ref() != Some(config);
            if let Some(old) = &inner.live {
                for (id, profile) in old.enabled_profiles() {
                    let identity = profile.capabilities_key(id);
                    if config
                        .enabled_profile(id)
                        .is_none_or(|new| new.capabilities_key(id) != identity)
                        && let Some(definition) = inner.definitions.get(&identity)
                    {
                        definition.cancellation.cancel();
                    }
                }
            }
            inner.live = Some(config.clone());
            changed
        };
        self.ensure(config);
        if changed {
            self.publish();
        }
    }

    /// Claim new definitions and default probes under one lock. A Settings
    /// draft uses this same path without becoming the live configuration.
    pub(crate) fn ensure(self: &Arc<Self>, config: &Config) {
        let config = Arc::new(config.clone());
        let mut started = Vec::new();
        {
            let mut inner = self.lock();
            for (id, profile) in config.enabled_profiles() {
                let identity = profile.capabilities_key(id);
                if let Some(definition) = inner.definitions.get_mut(&identity) {
                    if !definition.cancellation.is_cancelled() {
                        // Keep successful entries and the in-flight attempt.
                        // A failed owner's next retry uses refreshed credentials.
                        definition.config = config.clone();
                        continue;
                    }
                    inner.entries.retain(|(entry_identity, _), entry| {
                        entry_identity != &identity || matches!(entry, Entry::Ready(_))
                    });
                }
                let definition = Definition {
                    id: id.into(),
                    config: config.clone(),
                    cancellation: self.cancellation.child_token(),
                };
                let key = (identity.clone(), None);
                let attempt = if inner.entries.contains_key(&key) {
                    None
                } else {
                    Some(self.attempt(&definition, None))
                };
                inner
                    .definitions
                    .insert(identity.clone(), definition.clone());
                if let Some(Entry::Ready(choices)) = inner.entries.get(&key).cloned() {
                    for model in &choices.models {
                        let model_key = (identity.clone(), Some(model.value.clone()));
                        if !inner.entries.contains_key(&model_key) {
                            if choices.model.as_ref() == Some(&model.value) {
                                inner
                                    .entries
                                    .insert(model_key, Entry::Ready(choices.clone()));
                            } else {
                                let pending = self.attempt(&definition, Some(model.value.clone()));
                                inner
                                    .entries
                                    .insert(model_key.clone(), Entry::Pending(pending.clone()));
                                started.push((model_key, pending));
                            }
                        }
                    }
                }
                if let Some(attempt) = attempt {
                    inner
                        .entries
                        .insert(key.clone(), Entry::Pending(attempt.clone()));
                    started.push((key, attempt));
                }
            }
        }
        if !started.is_empty() {
            self.publish();
        }
        for (key, attempt) in started {
            self.spawn(key, attempt);
        }
    }

    fn attempt(&self, definition: &Definition, model: Option<String>) -> Attempt {
        #[cfg(test)]
        if let Some((probe, model_probe)) = &self.probe {
            let future = match model {
                None => probe(definition.id.clone()),
                Some(model) => model_probe(definition.id.clone(), model),
            };
            return future
                .map_err(|error| Arc::<str>::from(format!("{error:#}")))
                .boxed()
                .shared();
        }
        crate::controller::profile_config::discover_for(
            (*definition.config).clone(),
            definition.id.clone(),
            model,
            definition.cancellation.clone(),
        )
        .map_err(|error| Arc::<str>::from(format!("{error:#}")))
        .boxed()
        .shared()
    }

    fn spawn(self: &Arc<Self>, key: CacheKey, attempt: Attempt) {
        tokio::spawn(self.clone().drive(key, attempt));
    }

    fn drive(self: Arc<Self>, key: CacheKey, mut attempt: Attempt) -> BoxFuture<'static, ()> {
        Box::pin(async move {
            let cancellation = {
                let inner = self.lock();
                if !inner
                    .entries
                    .get(&key)
                    .is_some_and(|entry| entry.owns(&attempt))
                {
                    return;
                }
                inner.definitions[&key.0].cancellation.clone()
            };
            let mut failures = 0u32;
            loop {
                let result = tokio::select! {
                    _ = cancellation.cancelled() => return,
                    result = attempt.clone() => result,
                };
                let mut started = Vec::new();
                {
                    let mut inner = self.lock();
                    if !inner
                        .entries
                        .get(&key)
                        .is_some_and(|entry| entry.owns(&attempt))
                    {
                        return;
                    }
                    match &result {
                        Ok(choices) => {
                            inner
                                .entries
                                .insert(key.clone(), Entry::Ready(choices.clone()));
                            if key.1.is_none() {
                                let definition = inner.definitions[&key.0].clone();
                                for model in &choices.models {
                                    let model_key = (key.0.clone(), Some(model.value.clone()));
                                    if inner.entries.contains_key(&model_key) {
                                        continue;
                                    }
                                    if choices.model.as_ref() == Some(&model.value) {
                                        inner
                                            .entries
                                            .insert(model_key, Entry::Ready(choices.clone()));
                                    } else {
                                        let pending =
                                            self.attempt(&definition, Some(model.value.clone()));
                                        inner.entries.insert(
                                            model_key.clone(),
                                            Entry::Pending(pending.clone()),
                                        );
                                        started.push((model_key, pending));
                                    }
                                }
                            }
                        }
                        Err(error) => {
                            inner.entries.insert(
                                key.clone(),
                                Entry::Failed {
                                    attempt: attempt.clone(),
                                    error: error.clone(),
                                },
                            );
                            tracing::warn!(profile = %inner.definitions[&key.0].id, model = ?key.1, %error, "profile capability hydration failed; retrying with backoff");
                        }
                    }
                }
                self.publish();
                for (key, attempt) in started {
                    self.spawn(key, attempt);
                }
                if result.is_ok() {
                    return;
                }
                let delay = Duration::from_secs([1, 5, 30, 60][failures.min(3) as usize]);
                failures = failures.saturating_add(1);
                tokio::select! {
                    _ = cancellation.cancelled() => return,
                    _ = tokio::time::sleep(delay) => {}
                }
                {
                    let mut inner = self.lock();
                    if !inner
                        .entries
                        .get(&key)
                        .is_some_and(|entry| entry.owns(&attempt))
                    {
                        return;
                    }
                    attempt = self.attempt(&inner.definitions[&key.0], key.1.clone());
                    inner
                        .entries
                        .insert(key.clone(), Entry::Pending(attempt.clone()));
                }
                self.publish();
            }
        })
    }

    async fn fetch(
        self: &Arc<Self>,
        config: &Config,
        id: &str,
        model: Option<String>,
    ) -> Result<ProfileConfig> {
        let profile = config.enabled_profile(id).context("profile unavailable")?;
        self.ensure(config);
        let key = (profile.capabilities_key(id), model);
        let cancellation = self.lock().definitions[&key.0].cancellation.clone();
        let mut revisions = self.revisions.subscribe();
        loop {
            // Subscribe before inspecting ownership, so completion cannot be
            // missed. Readers never drive or publish the supervisor's future.
            let (entry, started) = {
                let mut inner = self.lock();
                let default = inner
                    .entries
                    .get(&(key.0.clone(), None))
                    .cloned()
                    .expect("default claimed by ensure");
                if key.1.is_some() && !matches!(default, Entry::Ready(_)) {
                    (default, None)
                } else if let Some(entry) = inner.entries.get(&key) {
                    (entry.clone(), None)
                } else {
                    let attempt = self.attempt(&inner.definitions[&key.0], key.1.clone());
                    let entry = Entry::Pending(attempt.clone());
                    inner.entries.insert(key.clone(), entry.clone());
                    (entry, Some(attempt))
                }
            };
            if let Some(attempt) = started {
                self.spawn(key.clone(), attempt);
                self.publish();
            }
            match entry {
                Entry::Ready(choices) => return Ok(choices),
                Entry::Failed { error, .. } => bail!("{error}"),
                Entry::Pending(_) => {}
            }
            // Shutdown also cancels this definition; report the owning cause
            // consistently when both tokens are ready.
            tokio::select! {
                biased;
                _ = self.cancellation.cancelled() => bail!("profile discovery cancelled by daemon shutdown"),
                _ = cancellation.cancelled() => bail!("profile definition retired"),
                result = revisions.changed() => { result.context("profile capability publisher stopped")?; }
            }
        }
    }

    fn live(&self) -> Result<Config> {
        self.lock()
            .live
            .clone()
            .context("the profile catalogue has not adopted a configuration")
    }

    pub(crate) fn candidates(&self, parent: &str) -> Result<Vec<(String, HarnessKind)>> {
        let config = self.live()?;
        Ok(config
            .enabled_profiles()
            .filter(|(id, _)| config.subagents.profile_is_eligible(parent, id))
            .map(|(id, profile)| (id.into(), profile.kind))
            .collect())
    }

    pub(crate) fn configured_candidates(&self) -> Result<Vec<(String, HarnessKind)>> {
        Ok(self
            .live()?
            .enabled_profiles()
            .map(|(id, profile)| (id.into(), profile.kind))
            .collect())
    }

    pub(crate) async fn model_capabilities(
        self: &Arc<Self>,
        profile: String,
        model: String,
    ) -> Result<ProfileConfig> {
        self.live_capabilities(&profile, Some(model)).await
    }

    async fn live_capabilities(
        self: &Arc<Self>,
        id: &str,
        model: Option<String>,
    ) -> Result<ProfileConfig> {
        loop {
            let config = self.live()?;
            let identity = config
                .enabled_profile(id)
                .context("profile unavailable")?
                .capabilities_key(id);
            let result = self.fetch(&config, id, model.clone()).await;
            let current = self.live()?;
            if current
                .enabled_profile(id)
                .map(|profile| profile.capabilities_key(id))
                .as_ref()
                == Some(&identity)
            {
                return result;
            }
            // Only this profile's configured definition changed. Follow its
            // new owner; unrelated profile updates cannot retire this read.
        }
    }

    pub(crate) async fn capabilities(
        self: &Arc<Self>,
        profiles: &[String],
    ) -> Result<Vec<ProfileConfig>> {
        self.live()?;
        join_all(profiles.iter().map(|id| self.live_capabilities(id, None)))
            .await
            .into_iter()
            .collect()
    }

    pub(crate) async fn options_for(
        self: &Arc<Self>,
        config: &Config,
        parent: &str,
        model: Option<String>,
    ) -> Result<mj_core::subagent::SubagentOptions> {
        crate::controller::profile_config::subagent_options_with(
            config,
            parent,
            model,
            |id, model| async move { self.fetch(config, &id, model).await },
        )
        .await
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    #[cfg(test)]
    fn build(
        cancellation: CancellationToken,
        probe: Arc<Probe>,
        model_probe: Arc<ModelProbe>,
    ) -> Arc<Self> {
        let mut catalog = Self::new(cancellation);
        Arc::get_mut(&mut catalog).unwrap().probe = Some((probe, model_probe));
        catalog
    }
    #[cfg(test)]
    pub(crate) fn with_probe(probe: Arc<Probe>) -> Arc<Self> {
        let model_probe = probe.clone();
        Self::build(
            CancellationToken::new(),
            probe,
            Arc::new(move |profile, _| model_probe(profile)),
        )
    }
    #[cfg(test)]
    pub(crate) async fn sync_now(self: &Arc<Self>, config: &Config) {
        self.sync(config);
        let discoveries = config.enabled_profiles().map(|(id, _)| async move {
            if let Ok(choices) = self.fetch(config, id, None).await {
                for model in choices.models {
                    let _ = self.fetch(config, id, Some(model.value)).await;
                }
            }
        });
        join_all(discoveries).await;
    }
}

/// A configuration with the named profiles enabled and, when `eligible` names
/// them, admitted for every parent. Shared with the API's `list_profiles`
/// test so both drive the same candidate filter.
#[cfg(test)]
pub(crate) fn test_config(profiles: &[(&str, HarnessKind)], eligible: &[&str]) -> Config {
    Config {
        profiles: profiles
            .iter()
            .map(|(id, kind)| {
                (
                    (*id).to_owned(),
                    HarnessProfile {
                        enabled: true,
                        kind: *kind,
                        home: std::path::PathBuf::from("/home/agent").join(id),
                        environment: Default::default(),
                        context_window_bytes: None,
                        subagents: Default::default(),
                        guardian_review_model: None,
                    },
                )
            })
            .collect(),
        subagents: SubagentConfig {
            eligible_profiles: eligible.iter().map(|id| ((*id).to_owned(), true)).collect(),
            ..SubagentConfig::default()
        },
        ..Config::default()
    }
}

/// A probe that answers with the profile's own id and counts its calls, so a
/// test can prove that a warm answer runs none.
#[cfg(test)]
pub(crate) fn counting_probe(calls: Arc<std::sync::atomic::AtomicUsize>) -> Arc<Probe> {
    Arc::new(move |profile| {
        calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Box::pin(async move { Ok(test_choices(&profile)) })
    })
}

/// The capabilities a fixture probe reports for a profile.
#[cfg(test)]
fn test_choices(profile: &str) -> ProfileConfig {
    ProfileConfig {
        model: Some(format!("{profile}-model")),
        models: vec![mj_core::acp::SessionConfigChoice {
            value: format!("{profile}-model"),
            name: format!("{profile} model"),
            description: None,
        }],
        efforts: Vec::new(),
        observed_at: 1_700_000_000,
    }
}

/// A probe that fails while the flag is set and counts its calls, so a test
/// can drive the background pass, the call that retries after it, and the
/// cache in between.
#[cfg(test)]
fn flag_probe(
    fails: Arc<std::sync::atomic::AtomicBool>,
    calls: Arc<std::sync::atomic::AtomicUsize>,
) -> Arc<Probe> {
    Arc::new(move |profile| {
        let fails = fails.clone();
        let calls = calls.clone();
        Box::pin(async move {
            calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if fails.load(std::sync::atomic::Ordering::SeqCst) {
                anyhow::bail!("harness is not installed")
            }
            Ok(test_choices(&profile))
        })
    })
}

/// A probe that reports each discovery it starts and then waits for the test
/// to release it, so a test can hold a discovery in flight and see who shares
/// it. The release is a `watch` channel rather than a `Notify` so that a
/// release arriving before a waiter does is still observed.
#[cfg(test)]
fn gated_probe(
    calls: Arc<std::sync::atomic::AtomicUsize>,
    started: tokio::sync::mpsc::UnboundedSender<String>,
    gate: tokio::sync::watch::Receiver<bool>,
) -> Arc<Probe> {
    Arc::new(move |profile| {
        let calls = calls.clone();
        let started = started.clone();
        let mut gate = gate.clone();
        Box::pin(async move {
            calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let _ = started.send(profile.clone());
            if !*gate.borrow_and_update() {
                let _ = gate.changed().await;
            }
            Ok(test_choices(&profile))
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    fn calls() -> Arc<AtomicUsize> {
        Arc::new(AtomicUsize::new(0))
    }

    #[tokio::test]
    async fn pending_model_efforts_share_one_attempt_and_unchanged_profiles_stay_warm() {
        let calls = calls();
        let (started_tx, mut started) = tokio::sync::mpsc::unbounded_channel();
        let (release, gate) = tokio::sync::watch::channel(false);
        let model_calls = calls.clone();
        let catalog = ProfileCatalog::build(
            CancellationToken::new(),
            Arc::new(|id| {
                Box::pin(async move {
                    let mut choices = test_choices(&id);
                    choices.models.push(mj_core::acp::SessionConfigChoice {
                        value: "other".into(),
                        name: "Other".into(),
                        description: None,
                    });
                    Ok(choices)
                })
            }),
            Arc::new(move |id, model| {
                model_calls.fetch_add(1, Ordering::SeqCst);
                let started = started_tx.clone();
                let mut gate = gate.clone();
                Box::pin(async move {
                    started.send(id.clone()).unwrap();
                    if !*gate.borrow_and_update() {
                        gate.changed().await.unwrap();
                    }
                    let mut choices = test_choices(&id);
                    choices.models.push(mj_core::acp::SessionConfigChoice {
                        value: model.clone(),
                        name: model.clone(),
                        description: None,
                    });
                    choices.model = Some(model);
                    Ok(choices)
                })
            }),
        );
        let mut config = test_config(&[("parent", HarnessKind::Codex)], &[]);
        catalog.sync(&config);
        assert_eq!(started.recv().await.as_deref(), Some("parent"));
        assert!(
            catalog
                .snapshot()
                .options(&config, "parent", None)
                .is_some()
        );
        assert!(
            catalog
                .snapshot()
                .options(&config, "parent", Some("other"))
                .is_none()
        );
        let reader = {
            let catalog = catalog.clone();
            let config = config.clone();
            tokio::spawn(async move {
                catalog
                    .options_for(&config, "parent", Some("other".into()))
                    .await
            })
        };
        // Adding a different profile must leave this model's pending owner intact.
        config.profiles.insert(
            "sibling".into(),
            test_config(&[("sibling", HarnessKind::Claude)], &[])
                .profiles
                .remove("sibling")
                .unwrap(),
        );
        catalog.sync(&config);
        assert_eq!(started.recv().await.as_deref(), Some("sibling"));
        assert!(!reader.is_finished());
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        release.send(true).unwrap();
        reader.await.unwrap().unwrap();
        catalog.sync_now(&config).await;
        catalog
            .options_for(&config, "parent", Some("other".into()))
            .await
            .unwrap();
        config.profiles.remove("sibling");
        catalog.sync_now(&config).await;
        catalog
            .options_for(&config, "parent", Some("other".into()))
            .await
            .unwrap();
        assert_eq!(
            calls.load(Ordering::SeqCst),
            2,
            "membership changes retain unchanged efforts"
        );
    }

    #[tokio::test]
    async fn concurrent_readers_and_eligibility_edits_join_startup_hydration() {
        let calls = calls();
        let (started_tx, mut started) = tokio::sync::mpsc::unbounded_channel();
        let (release, gate) = tokio::sync::watch::channel(false);
        let catalog = ProfileCatalog::with_probe(gated_probe(calls.clone(), started_tx, gate));
        let mut config = test_config(
            &[
                ("parent", HarnessKind::Codex),
                ("helper", HarnessKind::Claude),
            ],
            &[],
        );
        catalog.sync(&config);
        started.recv().await.unwrap();
        started.recv().await.unwrap();
        let reader = {
            let catalog = catalog.clone();
            tokio::spawn(async move {
                catalog
                    .capabilities(&["parent".into(), "helper".into()])
                    .await
            })
        };
        config
            .subagents
            .eligible_profiles
            .insert("helper".into(), true);
        config.profiles.get_mut("parent").unwrap().subagents =
            mj_core::subagent::SubagentPolicy::SingleModel {
                model: "parent-model".into(),
                effort: None,
            };
        catalog.sync(&config);
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert_eq!(catalog.candidates("parent").unwrap().len(), 2);
        assert!(
            catalog
                .snapshot()
                .options(&config, "parent", None)
                .is_none()
        );
        release.send(true).unwrap();
        assert_eq!(reader.await.unwrap().unwrap().len(), 2);
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn profile_definition_changes_do_not_retire_unrelated_reads_or_publish_old_choices() {
        let calls = calls();
        let (started_tx, mut started) = tokio::sync::mpsc::unbounded_channel();
        let (release, gate) = tokio::sync::watch::channel(false);
        let probe_calls = calls.clone();
        let probe: Arc<Probe> = Arc::new(move |id| {
            let number = probe_calls.fetch_add(1, Ordering::SeqCst);
            let mut gate = gate.clone();
            let started = started_tx.clone();
            Box::pin(async move {
                started.send(id.clone()).unwrap();
                if number == 0 {
                    gate.changed().await.unwrap();
                }
                let mut choices = test_choices(&id);
                choices.observed_at = number as i64;
                Ok(choices)
            })
        });
        let catalog = ProfileCatalog::with_probe(probe);
        let mut config = test_config(&[("parent", HarnessKind::Codex)], &[]);
        catalog.sync(&config);
        started.recv().await.unwrap();
        let old_key = config.profiles["parent"].capabilities_key("parent");
        let pending = {
            let catalog = catalog.clone();
            tokio::spawn(async move { catalog.capabilities(&["parent".into()]).await })
        };
        tokio::task::yield_now().await;
        config.profiles.get_mut("parent").unwrap().home = "/changed".into();
        config.profiles.insert(
            "sibling".into(),
            test_config(&[("sibling", HarnessKind::Claude)], &[])
                .profiles
                .remove("sibling")
                .unwrap(),
        );
        catalog.sync(&config);
        let current = pending.await.unwrap().unwrap();
        assert_eq!(current[0].observed_at, 1);
        let choices = catalog.capabilities(&["sibling".into()]).await.unwrap();
        assert_eq!(choices[0].model.as_deref(), Some("sibling-model"));
        release.send(true).unwrap();
        assert_ne!(
            old_key,
            config.profiles["parent"].capabilities_key("parent")
        );
        assert_eq!(
            catalog.capabilities(&["parent".into()]).await.unwrap()[0].observed_at,
            1
        );
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn drafts_share_global_entries_without_adopting_their_profile_list() {
        let calls = calls();
        let catalog = ProfileCatalog::with_probe(counting_probe(calls.clone()));
        let live = test_config(&[("parent", HarnessKind::Codex)], &[]);
        catalog.sync_now(&live).await;
        let mut draft = live.clone();
        draft.profiles.get_mut("parent").unwrap().home = "/draft".into();
        catalog.ensure(&draft);
        catalog.ensure(&draft);
        let (left, right) = tokio::join!(
            catalog.options_for(&draft, "parent", None),
            catalog.options_for(&draft, "parent", None)
        );
        left.unwrap();
        right.unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert_eq!(
            catalog.live().unwrap().profiles["parent"].home,
            live.profiles["parent"].home
        );
        catalog.options_for(&live, "parent", None).await.unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn failures_are_reported_and_only_the_owner_retries_after_backoff() {
        let calls = calls();
        let failing = Arc::new(AtomicBool::new(true));
        let catalog = ProfileCatalog::with_probe(flag_probe(failing.clone(), calls.clone()));
        let config = test_config(&[("parent", HarnessKind::Codex)], &[]);
        catalog.sync_now(&config).await;
        for _ in 0..3 {
            assert!(catalog.capabilities(&["parent".into()]).await.is_err());
        }
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "failed UI queries cannot launch retries"
        );
        let options = catalog.snapshot().options(&config, "parent", None).unwrap();
        assert!(options.unavailable[0].contains("harness is not installed"));
        failing.store(false, Ordering::SeqCst);
        tokio::time::advance(Duration::from_secs(1)).await;
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }
        catalog.capabilities(&["parent".into()]).await.unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn shutdown_cancels_readers_and_background_hydration() {
        let calls = calls();
        let (started_tx, mut started) = tokio::sync::mpsc::unbounded_channel();
        let (_release, gate) = tokio::sync::watch::channel(false);
        let cancellation = CancellationToken::new();
        let probe = gated_probe(calls, started_tx, gate);
        let model_probe = probe.clone();
        let catalog = ProfileCatalog::build(
            cancellation.clone(),
            probe,
            Arc::new(move |id, _| model_probe(id)),
        );
        catalog.sync(&test_config(&[("parent", HarnessKind::Codex)], &[]));
        started.recv().await.unwrap();
        cancellation.cancel();
        assert!(
            catalog
                .capabilities(&["parent".into()])
                .await
                .unwrap_err()
                .to_string()
                .contains("shutdown")
        );
    }

    #[tokio::test]
    async fn answers_before_adoption_report_that_without_launching_a_probe() {
        let calls = calls();
        let catalog = ProfileCatalog::with_probe(counting_probe(calls.clone()));
        assert!(
            catalog
                .candidates("parent")
                .unwrap_err()
                .to_string()
                .contains("not adopted")
        );
        assert!(catalog.capabilities(&["parent".into()]).await.is_err());
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn configured_candidates_include_enabled_profiles_outside_subagent_eligibility() {
        let catalog = ProfileCatalog::new(CancellationToken::new());
        let mut config = test_config(
            &[
                ("codex", HarnessKind::Codex),
                ("helper", HarnessKind::Codex),
                ("claude4", HarnessKind::Claude),
                ("disabled", HarnessKind::Kimi),
            ],
            &["helper"],
        );
        config.profiles.get_mut("disabled").unwrap().enabled = false;
        catalog.sync(&config);

        let subagent_ids = catalog
            .candidates("codex")
            .unwrap()
            .into_iter()
            .map(|(id, _)| id)
            .collect::<Vec<_>>();
        let configured_ids = catalog
            .configured_candidates()
            .unwrap()
            .into_iter()
            .map(|(id, _)| id)
            .collect::<Vec<_>>();

        assert_eq!(subagent_ids, vec!["codex".to_owned(), "helper".to_owned()]);
        assert_eq!(
            configured_ids,
            vec![
                "claude4".to_owned(),
                "codex".to_owned(),
                "helper".to_owned()
            ]
        );
    }
}
