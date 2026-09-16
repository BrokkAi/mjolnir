//! A background-refreshed catalogue of what `list_profiles` can answer with.
//!
//! The sub-agent `list_profiles` tool is called by a model in the middle of a
//! turn, so everything it waits for shows up as a stalled tool call. Its answer
//! names the profiles a parent may delegate to and the models and efforts each
//! one offers, and discovering those capabilities launches the harness in a
//! scratch home, which takes seconds. This module keeps that work off the call:
//! one background pass per configuration generation discovers every profile the
//! answer could name, and a call that finds a warm catalogue only filters and
//! ranks what it already holds.
//!
//! A pass belongs to a generation. Changing the profiles or the sub-agent
//! policy bumps the generation, drops the published answers, and starts a new
//! pass; a pass that finishes after that finds its generation gone and
//! publishes nothing. Discovery failures are reported and never cached, so the
//! call that needs them retries through its own fallback path.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use anyhow::Result;
use mj_client::session::BoxFuture;
use mj_core::config::{Config, HarnessKind, HarnessProfile, SubagentConfig};
use mj_core::worker_launch::ProfileConfig;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

/// The profiles one parent may delegate to: every enabled profile the
/// sub-agent policy admits for it, in configuration order. This is the whole
/// definition of the answer `list_profiles` gives, so the warm catalogue and
/// the fallback path both call it.
pub(crate) fn candidates(
    profiles: &BTreeMap<String, HarnessProfile>,
    subagents: &SubagentConfig,
    parent: &str,
) -> Vec<(String, HarnessKind)> {
    profiles
        .iter()
        .filter(|(id, profile)| profile.enabled && subagents.profile_is_eligible(parent, id))
        .map(|(id, profile)| (id.clone(), profile.kind))
        .collect()
}

/// The configuration inputs an answer depends on: which profiles exist and
/// what the sub-agent policy admits. A pass is identified by these, so an
/// unrelated settings edit does not restart every discovery.
#[derive(Clone, PartialEq)]
struct ProfilesKey {
    profiles: BTreeMap<String, HarnessProfile>,
    subagents: SubagentConfig,
}

impl ProfilesKey {
    fn of(config: &Config) -> Self {
        Self {
            profiles: config.profiles.clone(),
            subagents: config.subagents.clone(),
        }
    }

    fn matches(&self, config: &Config) -> bool {
        self.profiles == config.profiles && self.subagents == config.subagents
    }

    /// Every profile a pass must discover. Any enabled profile can host a
    /// session and is therefore its own first candidate, so the union of
    /// candidates over all parents is the enabled profiles. With the policy
    /// disabled no parent is offered anything, so no harness is started.
    fn warm_set(&self) -> Vec<String> {
        if !self.subagents.enabled {
            return Vec::new();
        }
        self.profiles
            .iter()
            .filter(|(_, profile)| profile.enabled)
            .map(|(id, _)| id.clone())
            .collect()
    }

    fn candidates(&self, parent: &str) -> Vec<(String, HarnessKind)> {
        candidates(&self.profiles, &self.subagents, parent)
    }
}

/// Discovers one profile's capabilities. Production discovers through the
/// shared per-profile discovery; tests substitute a hand-written probe.
pub(crate) type Probe = dyn Fn(String) -> BoxFuture<'static, Result<ProfileConfig>> + Send + Sync;

#[derive(Default)]
struct Inner {
    /// Identifies the configuration the published answers belong to. A pass
    /// publishes only while this still matches the generation it was for.
    generation: u64,
    /// The configuration inputs the published answers were derived from, or
    /// `None` while nothing has been adopted.
    key: Option<ProfilesKey>,
    /// What the current generation's pass has discovered, by profile id.
    configs: BTreeMap<String, ProfileConfig>,
}

/// One consistent answer for a parent profile.
pub(crate) struct CatalogView {
    /// The generation the caller must name to have a fresh discovery recorded,
    /// and that is dropped if the configuration changed first.
    pub(crate) generation: u64,
    pub(crate) candidates: Vec<(String, HarnessKind)>,
    /// The discovered capabilities, for the candidates the pass finished.
    pub(crate) configs: BTreeMap<String, ProfileConfig>,
}

/// Answers `list_profiles` from memory whenever a warm pass has run.
pub(crate) struct ProfileCatalog {
    cancellation: CancellationToken,
    probe: Arc<Probe>,
    inner: Mutex<Inner>,
}

impl ProfileCatalog {
    /// Build the catalogue the daemon serves, discovering through the shared
    /// per-profile discovery every other profile-configuration caller uses.
    pub(crate) fn new(cancellation: CancellationToken) -> Arc<Self> {
        Self::build(
            cancellation,
            Arc::new(|profile| {
                Box::pin(crate::controller::profile_config::discover(
                    profile, None, false,
                ))
            }),
        )
    }

    fn build(cancellation: CancellationToken, probe: Arc<Probe>) -> Arc<Self> {
        Arc::new(Self {
            cancellation,
            probe,
            inner: Mutex::new(Inner::default()),
        })
    }

    /// Adopt the configuration and warm the catalogue when it changed. The
    /// pass runs in the background; this returns immediately, which is what
    /// keeps a configuration change from stalling the control loop that
    /// noticed it.
    pub(crate) fn sync(self: &Arc<Self>, config: &Config) {
        let Some((generation, key)) = self.claim_pass(config) else {
            return;
        };
        let catalog = self.clone();
        tokio::spawn(async move { catalog.warm_pass(generation, key).await });
    }

    /// The bookkeeping half of [`ProfileCatalog::sync`]: adopt `config`, bump
    /// the generation, and claim its pass. `None` means the catalogue already
    /// answers for this configuration.
    fn claim_pass(&self, config: &Config) -> Option<(u64, ProfilesKey)> {
        let mut inner = self.lock();
        // Comparing in place keeps an unchanged configuration — the common
        // case, since every controller reload lands here — from copying the
        // profiles it already holds.
        if inner.key.as_ref().is_some_and(|key| key.matches(config)) {
            return None;
        }
        let key = ProfilesKey::of(config);
        inner.generation = inner.generation.wrapping_add(1);
        inner.configs.clear();
        inner.key = Some(key.clone());
        Some((inner.generation, key))
    }

    /// The warm answer for one parent, or `None` while the catalogue is cold
    /// and the caller must build the answer itself.
    pub(crate) fn view(&self, parent: &str) -> Option<CatalogView> {
        let inner = self.lock();
        let key = inner.key.as_ref()?;
        let candidates = key.candidates(parent);
        let configs = candidates
            .iter()
            .filter_map(|(id, _)| {
                inner
                    .configs
                    .get(id)
                    .map(|config| (id.clone(), config.clone()))
            })
            .collect();
        Some(CatalogView {
            generation: inner.generation,
            candidates,
            configs,
        })
    }

    /// Discover one profile now, as the call path does when the catalogue
    /// does not hold it, and remember the answer for `generation` when that is
    /// still the current one.
    pub(crate) async fn discover_profile(
        &self,
        profile: &str,
        generation: Option<u64>,
    ) -> Result<ProfileConfig> {
        let config = (self.probe)(profile.to_owned()).await?;
        if let Some(generation) = generation {
            self.remember(generation, profile, &config);
        }
        Ok(config)
    }

    /// Keep a discovery the current generation was missing. A configuration
    /// change since the discovery makes it stale, so it is dropped.
    fn remember(&self, generation: u64, profile: &str, config: &ProfileConfig) {
        let mut inner = self.lock();
        if inner.generation != generation {
            return;
        }
        inner.configs.insert(profile.to_owned(), config.clone());
    }

    /// Discover every profile the adopted configuration can offer. Results are
    /// published one at a time, so a call between two slow probes still avoids
    /// the discovery the pass already did.
    async fn warm_pass(self: Arc<Self>, generation: u64, key: ProfilesKey) {
        let mut probes = JoinSet::new();
        for profile in key.warm_set() {
            // A pass whose configuration is already replaced, or whose server
            // is shutting down, starts no harness at all.
            if !self.is_current(generation) || self.cancellation.is_cancelled() {
                break;
            }
            let probe = self.probe.clone();
            probes.spawn(async move { (profile.clone(), probe(profile).await) });
        }
        loop {
            let finished = tokio::select! {
                _ = self.cancellation.cancelled() => break,
                finished = probes.join_next() => finished,
            };
            let Some(finished) = finished else {
                break;
            };
            if !self.is_current(generation) {
                // A newer configuration owns the catalogue now, and its own
                // pass is already discovering what it needs.
                break;
            }
            match finished {
                Ok((profile, Ok(config))) => self.remember(generation, &profile, &config),
                Ok((profile, Err(error))) => tracing::warn!(
                    profile,
                    error = format!("{error:#}"),
                    "profile catalog discovery failed; list_profiles will retry it on demand"
                ),
                Err(error) => tracing::warn!(%error, "profile catalog discovery task failed"),
            }
        }
        // Remaining probes stop here rather than staying detached; the
        // discovery a caller starts itself is unaffected.
        probes.shutdown().await;
    }

    fn is_current(&self, generation: u64) -> bool {
        self.lock().generation == generation
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    #[cfg(test)]
    pub(crate) fn with_probe(probe: Arc<Probe>) -> Arc<Self> {
        Self::build(CancellationToken::new(), probe)
    }

    /// [`ProfileCatalog::sync`] without the spawn, so a test can await the
    /// pass it claims instead of polling for one.
    #[cfg(test)]
    pub(crate) async fn sync_now(self: &Arc<Self>, config: &Config) {
        if let Some((generation, key)) = self.claim_pass(config) {
            self.clone().warm_pass(generation, key).await;
        }
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
                        environment: BTreeMap::new(),
                        context_window_bytes: None,
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

/// A probe that fails while the flag is set, so a test can drive both the
/// background pass and a later on-demand discovery.
#[cfg(test)]
fn flag_probe(fails: Arc<std::sync::atomic::AtomicBool>) -> Arc<Probe> {
    Arc::new(move |profile| {
        let fails = fails.clone();
        Box::pin(async move {
            if fails.load(std::sync::atomic::Ordering::SeqCst) {
                anyhow::bail!("harness is not installed")
            }
            Ok(test_choices(&profile))
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn calls() -> Arc<AtomicUsize> {
        Arc::new(AtomicUsize::new(0))
    }

    fn fails() -> Arc<std::sync::atomic::AtomicBool> {
        Arc::new(std::sync::atomic::AtomicBool::new(true))
    }

    #[tokio::test]
    async fn a_warm_pass_discovers_every_enabled_profile_for_every_parent() {
        let calls = calls();
        let catalog = ProfileCatalog::with_probe(counting_probe(calls.clone()));
        let config = test_config(
            &[
                ("parent", HarnessKind::Codex),
                ("helper", HarnessKind::Claude),
                ("private", HarnessKind::Kimi),
            ],
            &["helper"],
        );

        catalog.sync_now(&config).await;

        let view = catalog
            .view("parent")
            .expect("an adopted configuration is warm");
        assert_eq!(
            view.candidates
                .iter()
                .map(|(id, kind)| (id.as_str(), *kind))
                .collect::<Vec<_>>(),
            vec![
                ("helper", HarnessKind::Claude),
                ("parent", HarnessKind::Codex)
            ],
            "a parent may delegate to itself and to the profiles listed as eligible"
        );
        assert_eq!(
            view.configs["parent"].model.as_deref(),
            Some("parent-model")
        );
        assert_eq!(
            view.configs["helper"].model.as_deref(),
            Some("helper-model")
        );
        assert!(
            !view.configs.contains_key("private"),
            "a profile no parent may offer is discovered as its own parent only"
        );
        assert_eq!(
            calls.load(Ordering::SeqCst),
            3,
            "every enabled profile is discovered once"
        );

        let again = catalog.view("parent").expect("still warm");
        assert_eq!(again.configs.len(), 2);
        assert_eq!(
            calls.load(Ordering::SeqCst),
            3,
            "a later answer must not discover anything"
        );
    }

    #[tokio::test]
    async fn a_warm_pass_probes_nothing_when_the_sub_agent_policy_is_disabled() {
        let calls = calls();
        let catalog = ProfileCatalog::with_probe(counting_probe(calls.clone()));
        let mut config = test_config(&[("parent", HarnessKind::Codex)], &[]);
        config.subagents.enabled = false;

        catalog.sync_now(&config).await;

        let view = catalog
            .view("parent")
            .expect("an adopted configuration is warm");
        assert!(
            view.candidates.is_empty(),
            "the policy offers no child profiles"
        );
        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "no parent can be offered anything, so no harness may be started"
        );
    }

    #[tokio::test]
    async fn a_configuration_change_invalidates_and_re_warms_the_catalogue() {
        let calls = calls();
        let catalog = ProfileCatalog::with_probe(counting_probe(calls.clone()));
        let config = test_config(
            &[
                ("parent", HarnessKind::Codex),
                ("helper", HarnessKind::Claude),
            ],
            &["helper"],
        );
        catalog.sync_now(&config).await;
        let generation = catalog.view("parent").unwrap().generation;

        assert!(
            catalog.claim_pass(&config).is_none(),
            "an unchanged configuration must not restart every discovery"
        );

        let changed = test_config(
            &[
                ("parent", HarnessKind::Codex),
                ("helper", HarnessKind::Claude),
                ("sibling", HarnessKind::Kimi),
            ],
            &["helper", "sibling"],
        );
        catalog.sync_now(&changed).await;

        let view = catalog
            .view("parent")
            .expect("the replacement pass is warm");
        assert!(view.generation > generation);
        assert_eq!(
            view.candidates
                .iter()
                .map(|(id, _)| id.as_str())
                .collect::<Vec<_>>(),
            vec!["helper", "parent", "sibling"]
        );
        assert_eq!(
            view.configs["sibling"].model.as_deref(),
            Some("sibling-model")
        );
        assert_eq!(calls.load(Ordering::SeqCst), 5, "both generations discover");
    }

    #[tokio::test]
    async fn a_pass_of_a_superseded_configuration_publishes_nothing() {
        let calls = calls();
        let catalog = ProfileCatalog::with_probe(counting_probe(calls.clone()));
        let first = test_config(&[("parent", HarnessKind::Codex)], &[]);
        let second = test_config(
            &[
                ("parent", HarnessKind::Codex),
                ("helper", HarnessKind::Claude),
            ],
            &["helper"],
        );
        let (generation, key) = catalog
            .claim_pass(&first)
            .expect("the first pass is claimed");
        catalog
            .claim_pass(&second)
            .expect("the change claims a new pass");

        catalog.clone().warm_pass(generation, key).await;

        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "a superseded pass must stop before it starts a harness"
        );
        let view = catalog
            .view("parent")
            .expect("the newer configuration is adopted");
        assert!(
            view.configs.is_empty(),
            "nothing from the superseded configuration may be published"
        );
    }

    #[tokio::test]
    async fn a_failed_probe_leaves_the_profile_cold_and_the_next_lookup_remembers_it() {
        let probes_fail = fails();
        let catalog = ProfileCatalog::with_probe(flag_probe(probes_fail.clone()));
        let config = test_config(&[("parent", HarnessKind::Codex)], &[]);

        catalog.sync_now(&config).await;

        let view = catalog
            .view("parent")
            .expect("the configuration is adopted");
        assert!(
            !view.configs.contains_key("parent"),
            "a failed discovery must not be published"
        );

        probes_fail.store(false, Ordering::SeqCst);
        let choices = catalog
            .discover_profile("parent", Some(view.generation))
            .await
            .expect("the retry succeeds");
        assert_eq!(choices.model.as_deref(), Some("parent-model"));
        assert_eq!(
            catalog.view("parent").unwrap().configs["parent"]
                .model
                .as_deref(),
            Some("parent-model"),
            "the fallback's discovery must serve the next call"
        );
    }

    #[tokio::test]
    async fn a_discovery_of_a_superseded_generation_is_dropped() {
        let probes_fail = fails();
        let catalog = ProfileCatalog::with_probe(flag_probe(probes_fail.clone()));
        let config = test_config(&[("parent", HarnessKind::Codex)], &[]);
        // The pass for this configuration never runs: the call path is the
        // first thing that asks for its profile.
        let (stale, _key) = catalog
            .claim_pass(&config)
            .expect("the first pass is claimed");
        let changed = test_config(
            &[
                ("parent", HarnessKind::Codex),
                ("helper", HarnessKind::Claude),
            ],
            &["helper"],
        );
        // The replacement pass discovers nothing of its own, because every
        // probe fails while the flag is set.
        catalog.sync_now(&changed).await;

        probes_fail.store(false, Ordering::SeqCst);
        catalog
            .discover_profile("parent", Some(stale))
            .await
            .expect("the probe itself succeeds");
        let view = catalog.view("parent").unwrap();
        assert!(
            view.configs.is_empty(),
            "a discovery named by a superseded generation is stale by then, \
             and the replacement pass published nothing either"
        );
    }
}
