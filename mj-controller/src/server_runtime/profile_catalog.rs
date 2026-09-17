//! A background-warmed catalogue of what `list_profiles` can answer with.
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
//! A call never starts a discovery of its own. When the pass has not published
//! a profile the call needs — the seconds between a start or a configuration
//! change and the pass finishing, or a probe still in flight — the call waits
//! on that profile's one discovery, the same shared future the pass polls, so
//! the harness is launched once whatever the ordering. A call that arrives
//! before any configuration has been adopted is told so and discovers nothing:
//! the daemon adopts a configuration before it serves.
//!
//! A discovery belongs to a generation, the integer that identifies one
//! adopted configuration. Changing the profiles or the sub-agent policy bumps
//! the generation, drops everything the catalogue holds, and starts a new pass;
//! a discovery of a superseded generation is not published, because the profile
//! it describes may have changed under the same id. Failures are reported and
//! never cached, so the next call that needs the profile tries again.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use anyhow::{Context, Result, bail};
use futures::future::{FutureExt, Shared, TryFutureExt, join_all};
use futures::stream::{FuturesUnordered, StreamExt};
use mj_client::session::BoxFuture;
use mj_core::config::{Config, HarnessKind, HarnessProfile, SubagentConfig};
use mj_core::worker_launch::ProfileConfig;
use tokio_util::sync::CancellationToken;

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

    /// Whether `config` holds the inputs this key was derived from. Comparing
    /// in place keeps an unchanged configuration — the common case, since
    /// every reload lands here — from copying what the catalogue already
    /// holds.
    fn matches(&self, config: &Config) -> bool {
        self.profiles == config.profiles && self.subagents == config.subagents
    }

    /// The profiles one parent may delegate to: every enabled profile the
    /// sub-agent policy admits for it, in configuration order. This is the
    /// whole definition of the answer `list_profiles` gives.
    fn candidates(&self, parent: &str) -> Vec<(String, HarnessKind)> {
        self.profiles
            .iter()
            .filter(|(id, profile)| {
                profile.enabled && self.subagents.profile_is_eligible(parent, id)
            })
            .map(|(id, profile)| (id.clone(), profile.kind))
            .collect()
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
}

/// Discovers one profile's capabilities. Production discovers through the
/// shared per-profile discovery; tests substitute a hand-written probe.
pub(crate) type Probe = dyn Fn(String) -> BoxFuture<'static, Result<ProfileConfig>> + Send + Sync;

/// Why a shared discovery failed. Every waiter receives a clone of the error,
/// so it carries the formatted cause rather than the original error value.
#[derive(Clone, Debug)]
struct DiscoveryFailure(Arc<str>);

impl fmt::Display for DiscoveryFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for DiscoveryFailure {}

/// One profile's discovery in flight. Every waiter polls this one future — the
/// background pass and any call that needs the profile before it lands — so
/// the harness is launched once however many waiters there are.
type Attempt = Shared<BoxFuture<'static, Result<ProfileConfig, DiscoveryFailure>>>;

/// What the catalogue holds for one profile of its adopted configuration.
enum Entry {
    /// The capabilities the discovery reported.
    Ready(ProfileConfig),
    /// The discovery now running, shared with every waiter.
    Pending(Attempt),
}

#[derive(Default)]
struct Inner {
    /// Identifies the adopted configuration. A discovery publishes only while
    /// this still matches the generation it was started for.
    generation: u64,
    /// The configuration inputs the catalogue answers from, or `None` while
    /// nothing has been adopted.
    key: Option<ProfilesKey>,
    /// What the current generation holds for the profiles it was asked about.
    entries: BTreeMap<String, Entry>,
}

impl Inner {
    /// The adopted configuration, or the error that there is none.
    fn adopted(&self) -> Result<&ProfilesKey> {
        self.key
            .as_ref()
            .context("the profile catalogue has not adopted a configuration")
    }

    /// The generation of the adopted configuration, with the profiles an
    /// answer is being built for checked against it.
    fn generation_for(&self, profiles: &[String]) -> Result<u64> {
        let key = self.adopted()?;
        for profile in profiles {
            if !key.profiles.contains_key(profile) {
                bail!("the adopted configuration holds no '{profile}' profile");
            }
        }
        Ok(self.generation)
    }
}

/// Answers `list_profiles` from a catalogue the daemon keeps warm.
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
        if inner.key.as_ref().is_some_and(|key| key.matches(config)) {
            return None;
        }
        let key = ProfilesKey::of(config);
        inner.generation = inner.generation.wrapping_add(1);
        // The discoveries of the previous configuration describe profiles
        // that may have changed under the same id, so they are dropped rather
        // than reused.
        inner.entries.clear();
        inner.key = Some(key.clone());
        Some((inner.generation, key))
    }

    /// The profiles `parent` may delegate to, in configuration order. Fails
    /// while nothing has been adopted, which the daemon never answers with: it
    /// adopts a configuration before it serves.
    pub(crate) fn candidates(&self, parent: &str) -> Result<Vec<(String, HarnessKind)>> {
        let inner = self.lock();
        Ok(inner.adopted()?.candidates(parent))
    }

    /// What the catalogue already holds for one profile, without waiting and
    /// without starting a discovery. `None` means the answer is not there to
    /// be had cheaply: nothing has been adopted, the adopted configuration has
    /// no such profile, or its discovery is still in flight. A caller that
    /// cannot afford a harness launch — `spawn`, which a model waits on —
    /// treats `None` as "not checked here" rather than as a failure.
    pub(crate) fn published(&self, profile: &str) -> Option<ProfileConfig> {
        let inner = self.lock();
        inner.key.as_ref()?;
        match inner.entries.get(profile) {
            Some(Entry::Ready(config)) => Some(config.clone()),
            _ => None,
        }
    }

    /// The capabilities of the named candidate profiles, in the order given.
    /// Waits on the discovery the background pass is running wherever it has
    /// not published one yet, and keeps a discovery it waited for so the next
    /// call does not wait again.
    pub(crate) async fn capabilities(&self, profiles: &[String]) -> Result<Vec<ProfileConfig>> {
        let generation = {
            let inner = self.lock();
            inner.generation_for(profiles)?
        };
        let mut discoveries: Vec<BoxFuture<'_, Result<ProfileConfig>>> =
            Vec::with_capacity(profiles.len());
        for profile in profiles {
            match self.entry(generation, profile) {
                Some(Entry::Ready(config)) => discoveries.push(async move { Ok(config) }.boxed()),
                Some(Entry::Pending(attempt)) => {
                    let catalog = self;
                    let profile = profile.clone();
                    discoveries.push(
                        async move {
                            match attempt.await {
                                Ok(config) => {
                                    catalog.remember(generation, &profile, &config);
                                    Ok(config)
                                }
                                Err(failure) => {
                                    catalog.forget(generation, &profile);
                                    Err(anyhow::anyhow!(
                                        "could not discover the capabilities of the \
                                         '{profile}' profile: {failure}"
                                    ))
                                }
                            }
                        }
                        .boxed(),
                    );
                }
                None => bail!(
                    "the profile catalogue adopted a new configuration before it could \
                     answer for the '{profile}' profile"
                ),
            }
        }
        let results = tokio::select! {
            _ = self.cancellation.cancelled() => bail!(
                "the server stopped before the profile catalogue discovered what \
                 list_profiles needs"
            ),
            results = join_all(discoveries) => results,
        };
        results.into_iter().collect()
    }

    /// The state of one profile of `generation`: what the pass has already
    /// published, or the discovery it is running — taken rather than started,
    /// so every waiter shares one harness launch. `None` means the generation
    /// was superseded, so the caller stops.
    fn entry(&self, generation: u64, profile: &str) -> Option<Entry> {
        let mut inner = self.lock();
        if inner.generation != generation {
            return None;
        }
        Some(match inner.entries.get(profile) {
            Some(Entry::Ready(config)) => Entry::Ready(config.clone()),
            Some(Entry::Pending(attempt)) => Entry::Pending(attempt.clone()),
            None => {
                let attempt = (self.probe)(profile.to_owned())
                    .map_err(|error| DiscoveryFailure(format!("{error:#}").into()))
                    .boxed()
                    .shared();
                inner
                    .entries
                    .insert(profile.to_owned(), Entry::Pending(attempt.clone()));
                Entry::Pending(attempt)
            }
        })
    }

    /// Keep a discovery the current generation was missing. A configuration
    /// change since the discovery was started made it stale, so it is dropped
    /// rather than published under the wrong configuration.
    fn remember(&self, generation: u64, profile: &str, config: &ProfileConfig) {
        let mut inner = self.lock();
        if inner.generation != generation {
            return;
        }
        inner
            .entries
            .insert(profile.to_owned(), Entry::Ready(config.clone()));
    }

    /// Drop a failed discovery so the next call that needs the profile tries
    /// again. A failure is never published: caching one would freeze a
    /// transient harness failure into the tool's answer until the
    /// configuration changed or the daemon restarted.
    fn forget(&self, generation: u64, profile: &str) {
        let mut inner = self.lock();
        if inner.generation != generation {
            return;
        }
        inner.entries.remove(profile);
    }

    /// Discover every profile the adopted configuration can offer, publishing
    /// each as it lands so a call that arrives between two slow probes still
    /// avoids the discovery the pass already did.
    async fn warm_pass(self: Arc<Self>, generation: u64, key: ProfilesKey) {
        let mut discoveries = FuturesUnordered::new();
        for profile in key.warm_set() {
            // A pass whose configuration is already replaced, or whose server
            // is shutting down, starts no harness at all.
            if self.cancellation.is_cancelled() {
                break;
            }
            match self.entry(generation, &profile) {
                None => break,
                Some(Entry::Ready(_)) => {}
                Some(Entry::Pending(attempt)) => {
                    discoveries.push(async move { (profile, attempt.await) });
                }
            }
        }
        loop {
            let next = tokio::select! {
                _ = self.cancellation.cancelled() => break,
                next = discoveries.next() => next,
            };
            let Some((profile, result)) = next else {
                break;
            };
            if !self.is_current(generation) {
                // A newer configuration owns the catalogue now, and its own
                // pass is already discovering what it needs.
                break;
            }
            match result {
                Ok(config) => self.remember(generation, &profile, &config),
                Err(failure) => {
                    // The failure is reported and dropped rather than
                    // published; the next call that needs the profile retries.
                    self.forget(generation, &profile);
                    tracing::warn!(
                        profile,
                        error = %failure,
                        "profile catalog discovery failed; list_profiles will retry it on demand"
                    );
                }
            }
        }
        // Dropping the remaining attempts stops the discoveries no caller is
        // waiting on; a caller that holds one drives it to the end itself.
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

        assert_eq!(
            catalog
                .candidates("parent")
                .expect("a configuration is adopted"),
            vec![
                ("helper".to_owned(), HarnessKind::Claude),
                ("parent".to_owned(), HarnessKind::Codex)
            ],
            "a parent may delegate to itself and to the profiles listed as eligible"
        );
        assert_eq!(
            calls.load(Ordering::SeqCst),
            3,
            "every enabled profile is discovered once, whether or not a parent may offer it"
        );

        let choices = catalog
            .capabilities(&["parent".to_owned(), "helper".to_owned()])
            .await
            .expect("the pass discovered both");
        assert_eq!(choices[0].model.as_deref(), Some("parent-model"));
        assert_eq!(choices[1].model.as_deref(), Some("helper-model"));
        assert_eq!(
            calls.load(Ordering::SeqCst),
            3,
            "an answer from the warm catalogue discovers nothing"
        );
    }

    #[tokio::test]
    async fn a_warm_pass_probes_nothing_when_the_sub_agent_policy_is_disabled() {
        let calls = calls();
        let catalog = ProfileCatalog::with_probe(counting_probe(calls.clone()));
        let mut config = test_config(&[("parent", HarnessKind::Codex)], &[]);
        config.subagents.enabled = false;

        catalog.sync_now(&config).await;

        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "no parent is offered anything, so no harness is started"
        );
        assert!(
            catalog
                .candidates("parent")
                .expect("a configuration is adopted")
                .is_empty()
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

        // The same configuration again claims no pass and starts no harness.
        catalog.sync_now(&config).await;
        assert_eq!(calls.load(Ordering::SeqCst), 2);

        let changed = test_config(
            &[
                ("parent", HarnessKind::Codex),
                ("helper", HarnessKind::Claude),
                ("sibling", HarnessKind::Kimi),
            ],
            &["helper", "sibling"],
        );
        catalog.sync_now(&changed).await;

        assert_eq!(
            catalog
                .candidates("parent")
                .expect("the replacement configuration is adopted")
                .iter()
                .map(|(id, _)| id.as_str())
                .collect::<Vec<_>>(),
            vec!["helper", "parent", "sibling"]
        );
        assert_eq!(
            calls.load(Ordering::SeqCst),
            5,
            "the new configuration is discovered from scratch"
        );
        let choices = catalog
            .capabilities(&["sibling".to_owned()])
            .await
            .expect("the new pass discovered the profile it added");
        assert_eq!(choices[0].model.as_deref(), Some("sibling-model"));
    }

    #[tokio::test]
    async fn a_superseded_pass_starts_no_harness() {
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
    }

    #[tokio::test]
    async fn a_discovery_of_a_superseded_generation_is_not_published() {
        let calls = calls();
        let probes_fail = fails();
        let catalog = ProfileCatalog::with_probe(flag_probe(probes_fail.clone(), calls.clone()));
        let first = test_config(&[("parent", HarnessKind::Codex)], &[]);
        let (stale, _key) = catalog
            .claim_pass(&first)
            .expect("the first pass is claimed");
        let stale_entry = catalog
            .entry(stale, "parent")
            .expect("the superseded pass planned its discovery");

        // A new configuration is adopted while the first discovery is in
        // flight, and its own pass finds every probe failing, so the catalogue
        // holds nothing when the stale discovery below finishes.
        let second = test_config(
            &[
                ("parent", HarnessKind::Codex),
                ("helper", HarnessKind::Claude),
            ],
            &["helper"],
        );
        catalog.sync_now(&second).await;
        probes_fail.store(false, Ordering::SeqCst);

        let Entry::Pending(attempt) = stale_entry else {
            panic!("the pending discovery of the superseded generation is at hand")
        };
        let config = attempt.await.expect("the probe itself succeeds");
        catalog.remember(stale, "parent", &config);

        let before = calls.load(Ordering::SeqCst);
        let choices = catalog
            .capabilities(&["parent".to_owned()])
            .await
            .expect("the adopted configuration answers");
        assert_eq!(choices[0].model.as_deref(), Some("parent-model"));
        assert_eq!(
            calls.load(Ordering::SeqCst),
            before + 1,
            "the superseded discovery was dropped, so this call discovers the profile itself"
        );
    }

    #[tokio::test]
    async fn a_failed_discovery_is_not_cached_and_the_next_call_retries_it() {
        let calls = calls();
        let probes_fail = fails();
        let catalog = ProfileCatalog::with_probe(flag_probe(probes_fail.clone(), calls.clone()));
        let config = test_config(&[("parent", HarnessKind::Codex)], &[]);

        catalog.sync_now(&config).await;
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "the pass tries the profile once"
        );

        let error = catalog
            .capabilities(&["parent".to_owned()])
            .await
            .expect_err("the harness is still broken");
        assert!(
            format!("{error:#}").contains("harness is not installed"),
            "{error:#}"
        );

        probes_fail.store(false, Ordering::SeqCst);
        let choices = catalog
            .capabilities(&["parent".to_owned()])
            .await
            .expect("the retry succeeds");
        assert_eq!(choices[0].model.as_deref(), Some("parent-model"));

        let discoveries = calls.load(Ordering::SeqCst);
        catalog
            .capabilities(&["parent".to_owned()])
            .await
            .expect("the retry's answer is kept");
        assert_eq!(
            calls.load(Ordering::SeqCst),
            discoveries,
            "a discovery the call waited for serves the calls after it"
        );
    }

    /// The behaviour the whole change exists for: a call that arrives while
    /// the background pass is discovering waits for that discovery instead of
    /// starting one of its own, so the harness is launched once.
    #[tokio::test]
    async fn a_call_waits_for_the_discovery_the_background_pass_is_running() {
        let calls = calls();
        let (started_tx, mut started) = tokio::sync::mpsc::unbounded_channel();
        let (release, gate) = tokio::sync::watch::channel(false);
        let catalog =
            ProfileCatalog::with_probe(gated_probe(calls.clone(), started_tx, gate.clone()));
        let config = test_config(
            &[
                ("parent", HarnessKind::Codex),
                ("helper", HarnessKind::Claude),
            ],
            &["helper"],
        );

        catalog.sync(&config);
        let mut in_flight = Vec::new();
        while in_flight.len() < 2 {
            in_flight.push(started.recv().await.expect("the pass starts its probes"));
        }
        assert_eq!(
            calls.load(Ordering::SeqCst),
            2,
            "the pass discovers each enabled profile once: {in_flight:?}"
        );

        let call = {
            let catalog = catalog.clone();
            tokio::spawn(async move {
                catalog
                    .capabilities(&["parent".to_owned(), "helper".to_owned()])
                    .await
            })
        };
        // Let the call reach the discoveries before checking what they are:
        // they are the pass's, still running, not new ones of its own.
        tokio::task::yield_now().await;
        assert_eq!(
            calls.load(Ordering::SeqCst),
            2,
            "the call must not start a second discovery for either profile"
        );

        release.send(true).expect("the gate is held by the test");
        let choices = call
            .await
            .expect("the call task is not cancelled")
            .expect("the pass's discoveries answer the call");
        assert_eq!(choices[0].model.as_deref(), Some("parent-model"));
        assert_eq!(choices[1].model.as_deref(), Some("helper-model"));
        catalog
            .capabilities(&["parent".to_owned()])
            .await
            .expect("the pass's discovery is now the catalogue's answer");
        assert_eq!(
            calls.load(Ordering::SeqCst),
            2,
            "the discovery the call waited for is kept"
        );
    }

    /// What `spawn` depends on: a lookup that answers only from what the pass
    /// has already published, never starting a discovery and never waiting.
    #[tokio::test]
    async fn published_answers_only_from_what_the_pass_has_already_published() {
        let calls = calls();
        let (started_tx, mut started) = tokio::sync::mpsc::unbounded_channel();
        let (release, gate) = tokio::sync::watch::channel(false);
        let catalog = ProfileCatalog::with_probe(gated_probe(calls.clone(), started_tx, gate));
        let config = test_config(&[("parent", HarnessKind::Codex)], &[]);

        assert!(
            catalog.published("parent").is_none(),
            "nothing has been adopted yet"
        );
        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "the lookup must not start a discovery"
        );

        catalog.sync(&config);
        started.recv().await.expect("the pass starts its probe");
        assert!(
            catalog.published("parent").is_none(),
            "the discovery is still in flight, so there is nothing to answer with"
        );
        assert!(
            catalog.published("unknown").is_none(),
            "the adopted configuration holds no such profile"
        );
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "neither lookup started a discovery of its own"
        );

        release.send(true).expect("the gate is held by the test");
        catalog
            .capabilities(&["parent".to_owned()])
            .await
            .expect("the discovery lands");
        assert_eq!(
            catalog
                .published("parent")
                .expect("the discovery is published now")
                .model
                .as_deref(),
            Some("parent-model")
        );
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "the published answer costs no discovery"
        );
    }

    #[tokio::test]
    async fn an_answer_before_a_configuration_is_adopted_reports_that() {
        let calls = calls();
        let catalog = ProfileCatalog::with_probe(counting_probe(calls.clone()));

        let error = catalog
            .candidates("parent")
            .expect_err("nothing has been adopted");
        assert!(
            format!("{error:#}").contains("has not adopted a configuration"),
            "{error:#}"
        );
        let error = catalog
            .capabilities(&["parent".to_owned()])
            .await
            .expect_err("nothing has been adopted");
        assert!(
            format!("{error:#}").contains("has not adopted a configuration"),
            "{error:#}"
        );
        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "a call that cannot be answered starts no harness"
        );
    }
}
