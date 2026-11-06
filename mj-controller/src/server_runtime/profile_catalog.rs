//! One daemon-owned cache, warmed independently of settings and tool calls.
//! Entries belong to configured profile identities, never a shared generation.
//! Every reader joins the owner's attempt; only its supervisor retries failures.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::{
    Arc, Mutex, MutexGuard, PoisonError,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use futures::future::{FutureExt, Shared, TryFutureExt, join_all};
use mj_client::session::BoxFuture;
#[cfg(test)]
use mj_core::config::HarnessProfile;
use mj_core::config::{Config, HarnessKind};
use mj_core::profile_capabilities::{
    CapabilityState, ProfileCapabilities, ProfileCapabilitiesSnapshot,
};
use mj_core::worker_launch::ProfileConfig;

use crate::quota::ProfileQuota;
use tokio_util::sync::CancellationToken;

#[cfg(test)]
pub(crate) type Probe = dyn Fn(String) -> BoxFuture<'static, Result<ProfileConfig>> + Send + Sync;
#[cfg(test)]
type ModelProbe = dyn Fn(String, String) -> BoxFuture<'static, Result<ProfileConfig>> + Send + Sync;
type Attempt = Shared<BoxFuture<'static, Result<ProfileConfig, AttemptError>>>;
type CacheKey = (String, Option<String>);
type NoticeCallback = Arc<dyn Fn(String) + Send + Sync>;
pub(crate) type QuotaReports = Arc<Mutex<BTreeMap<String, ProfileQuota>>>;

/// How long the legacy upgrade waits for every offering profile's first
/// quota report before treating profiles without one as subscriptions.
const QUOTA_REPORT_WAIT: Duration = Duration::from_secs(30);

/// How often ready entries are checked against the choice lifetime. An
/// entry is probed again at most this long after it expires.
const REFRESH_CHECK_INTERVAL: Duration = Duration::from_secs(5 * 60);

/// Delays before the owner of a failed entry tries again. Every attempt
/// stages the profile and launches its harness, so a failure that persists,
/// such as a full disk or a missing login, is retried rarely rather than
/// every minute. A changed definition or a session's report starts sooner.
const RETRY_DELAYS: [Duration; 6] = [
    Duration::from_secs(1),
    Duration::from_secs(5),
    Duration::from_secs(30),
    Duration::from_secs(60),
    Duration::from_secs(5 * 60),
    Duration::from_secs(15 * 60),
];

/// A failed attempt: its reason, and whether a later attempt could succeed.
#[derive(Clone)]
struct AttemptError {
    message: Arc<str>,
    retryable: bool,
}
impl AttemptError {
    fn new(error: &anyhow::Error) -> Self {
        Self {
            message: format!("{error:#}").into(),
            retryable: !crate::controller::profile_config::discovery_is_unsupported(error),
        }
    }
}

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
    publication: Option<mj_core::snapshot_map::SharedSnapshot<ProfileCapabilitiesSnapshot>>,
    definitions: BTreeMap<String, Definition>,
    entries: BTreeMap<CacheKey, Entry>,
    /// Models a configuration selected, by profile identity, from every
    /// configuration ensured, live or draft. Their efforts are discovered as
    /// soon as the profile lists them; other models wait for a reader.
    wanted: BTreeMap<String, BTreeSet<String>>,
    /// Probes replacing out-of-date ready entries, which stay served
    /// until a probe succeeds.
    refreshing: BTreeMap<CacheKey, Attempt>,
}

impl Inner {
    /// The `models` allow-list of the profile `identity` names: the live
    /// configuration's when it has that profile, else the latest draft's.
    /// Read at publication, so an edit applies without rediscovery.
    fn allowed_models(&self, identity: &str) -> Vec<String> {
        self.live
            .iter()
            .flat_map(Config::enabled_profiles)
            .find(|(id, profile)| profile.capabilities_key(id) == identity)
            .map(|(_, profile)| profile.models.clone())
            .or_else(|| {
                let definition = self.definitions.get(identity)?;
                Some(
                    definition
                        .config
                        .profiles
                        .get(&definition.id)?
                        .models
                        .clone(),
                )
            })
            .unwrap_or_default()
    }
}

pub(crate) struct ProfileCatalog {
    cancellation: CancellationToken,
    inner: Mutex<Inner>,
    publisher: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
    notice: Mutex<Option<NoticeCallback>>,
    revisions: tokio::sync::watch::Sender<u64>,
    legacy_review_upgrade_started: AtomicBool,
    config_path: Option<PathBuf>,
    /// The daemon's latest quota report per profile. Without it every
    /// profile counts as a subscription when the legacy upgrade picks models.
    quota_reports: Mutex<Option<QuotaReports>>,
    #[cfg(test)]
    probe: Option<(Arc<Probe>, Arc<ModelProbe>)>,
}
impl ProfileCatalog {
    pub(crate) fn new(cancellation: CancellationToken) -> Arc<Self> {
        Arc::new(Self {
            cancellation,
            inner: Mutex::new(Inner::default()),
            publisher: Mutex::new(None),
            notice: Mutex::new(None),
            revisions: tokio::sync::watch::channel(0).0,
            legacy_review_upgrade_started: AtomicBool::new(false),
            config_path: None,
            quota_reports: Mutex::new(None),
            #[cfg(test)]
            probe: None,
        })
    }

    pub(crate) fn new_with_config_path(
        cancellation: CancellationToken,
        config_path: PathBuf,
    ) -> Arc<Self> {
        let mut catalog = Self::new(cancellation);
        Arc::get_mut(&mut catalog).unwrap().config_path = Some(config_path);
        catalog
    }

    pub(crate) fn set_publisher(&self, publisher: Arc<dyn Fn() + Send + Sync>) {
        *self
            .publisher
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(publisher);
    }

    pub(crate) fn set_quota_reports(&self, reports: QuotaReports) {
        *self
            .quota_reports
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(reports);
    }

    /// The profiles in `profiles` billed per use, and those whose billing is
    /// not known yet: no report classifies them and their harness does not
    /// decide. The upgrade writes its choice once, so it waits briefly for
    /// reports to arrive and refuses to guess afterwards. Without a report
    /// source (no daemon), every profile counts as a subscription.
    fn billing_among(
        &self,
        profiles: &[(String, HarnessKind)],
    ) -> (BTreeSet<String>, BTreeSet<String>) {
        use mj_client::quota::QuotaBilling;
        let reports = self
            .quota_reports
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        let Some(reports) = reports else {
            return (BTreeSet::new(), BTreeSet::new());
        };
        let reports = reports.lock().unwrap_or_else(PoisonError::into_inner);
        let mut priced = BTreeSet::new();
        let mut unknown = BTreeSet::new();
        for (id, harness) in profiles {
            match ProfileQuota::billing_for(*harness, reports.get(id)) {
                QuotaBilling::Usage => {
                    priced.insert(id.clone());
                }
                QuotaBilling::Subscription => {}
                QuotaBilling::Unknown => {
                    unknown.insert(id.clone());
                }
            }
        }
        (priced, unknown)
    }

    pub(crate) fn set_notice(&self, notice: NoticeCallback) {
        *self.notice.lock().unwrap_or_else(PoisonError::into_inner) = Some(notice);
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

    pub(crate) fn snapshot(
        &self,
    ) -> mj_core::snapshot_map::SharedSnapshot<ProfileCapabilitiesSnapshot> {
        let mut inner = self.lock();
        if let Some(snapshot) = &inner.publication {
            return snapshot.clone();
        }
        let snapshot = mj_core::snapshot_map::SharedSnapshot::from(ProfileCapabilitiesSnapshot {
            profiles: inner
                .definitions
                .keys()
                .map(|identity| {
                    let allowed = inner.allowed_models(identity);
                    let choices = match inner
                        .entries
                        .get(&(identity.clone(), None))
                        .map_or(CapabilityState::Pending, Entry::state)
                    {
                        CapabilityState::Ready(choices) => {
                            CapabilityState::Ready(choices.restricted_to(&allowed))
                        }
                        other => other,
                    };
                    // Only models whose efforts were claimed are listed.
                    let efforts = match &choices {
                        CapabilityState::Ready(choices) => choices
                            .models
                            .iter()
                            .filter_map(|model| {
                                let state = match inner
                                    .entries
                                    .get(&(identity.clone(), Some(model.value.clone())))?
                                {
                                    Entry::Ready(choices) => {
                                        CapabilityState::Ready(choices.efforts.clone())
                                    }
                                    Entry::Failed { error, .. } => {
                                        CapabilityState::Failed(error.to_string())
                                    }
                                    Entry::Pending(_) => CapabilityState::Pending,
                                };
                                Some((model.value.clone(), state))
                            })
                            .collect(),
                        _ => BTreeMap::new(),
                    };
                    (identity.clone(), ProfileCapabilities { choices, efforts })
                })
                .collect(),
        });
        inner.publication = Some(snapshot.clone());
        snapshot
    }

    /// Adoption updates which profiles are enabled without invalidating capability entries.
    pub(crate) fn sync(self: &Arc<Self>, config: &Config) {
        let (changed, first) = {
            let mut inner = self.lock();
            let first = inner.live.is_none();
            let changed = inner.live.as_ref() != Some(config);
            // The allow-list is applied when the snapshot is built, so a
            // cached snapshot would keep serving the old one.
            let models = |config: &Config| {
                config
                    .profiles
                    .iter()
                    .map(|(id, profile)| (id.clone(), profile.models.clone()))
                    .collect::<BTreeMap<_, _>>()
            };
            if inner.live.as_ref().map(models) != Some(models(config)) {
                inner.publication = None;
            }
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
            (changed, first)
        };
        if first {
            self.start_refreshing();
        }
        if config.review.is_legacy()
            && self.config_path.is_some()
            && !self
                .legacy_review_upgrade_started
                .swap(true, Ordering::AcqRel)
        {
            let catalog = Arc::clone(self);
            tokio::spawn(async move {
                for attempt in 1.. {
                    let result = tokio::select! {
                        _ = catalog.cancellation.cancelled() => return,
                        result = catalog.upgrade_legacy_review(attempt) => result,
                    };
                    match result {
                        Ok(()) => return,
                        Err(error) => {
                            tracing::warn!(attempt, %error, "legacy review autoconfiguration did not complete")
                        }
                    }
                    tokio::select! {
                        _ = catalog.cancellation.cancelled() => return,
                        _ = tokio::time::sleep(Duration::from_secs((attempt as u64).saturating_mul(5).min(300))) => {},
                    }
                }
            });
        }
        self.ensure(config);
        if changed {
            self.publish();
        }
    }

    async fn upgrade_legacy_review(self: &Arc<Self>, _attempt: usize) -> Result<()> {
        let discovery_config = self.live()?;
        let definitions = upgrade_definitions(&discovery_config);
        let legacy = discovery_config
            .review
            .legacy
            .clone()
            .context("review is no longer legacy")?;
        let harnesses = self.candidates()?;
        let ids = harnesses
            .iter()
            .map(|(id, _)| id.clone())
            .collect::<Vec<_>>();
        // Each profile has its own deadline so one stuck probe cannot hide
        // results from profiles that answered. Partial results remain transient
        // until all profiles answer or the retry budget is exhausted.
        let choices = join_all(ids.iter().map(|id| async {
            tokio::time::timeout(Duration::from_secs(30), self.live_capabilities(id, None)).await
        }))
        .await;
        let mut omitted = Vec::new();
        let available = ids.into_iter().zip(choices).filter_map(|(id, result)| {
            match result {
                Ok(Ok(choices)) => Some((id, choices)),
                Ok(Err(error)) => {
                    tracing::warn!(profile = %id, %error, "profile discovery failed during legacy review upgrade");
                    omitted.push(id);
                    None
                }
                Err(_) => {
                    tracing::warn!(profile = %id, "profile discovery timed out during legacy review upgrade");
                    omitted.push(id);
                    None
                }
            }
        }).collect::<Vec<_>>();
        if !omitted.is_empty()
            && !legacy
                .profile
                .as_ref()
                .is_some_and(|profile| available.iter().any(|(id, _)| id == profile))
        {
            anyhow::bail!(
                "legacy review upgrade waiting for profiles: {}",
                omitted.join(", ")
            );
        }
        let offering = harnesses
            .into_iter()
            .filter(|(id, _)| available.iter().any(|(available, _)| available == id))
            .collect::<Vec<_>>();
        let deadline = tokio::time::Instant::now() + QUOTA_REPORT_WAIT;
        let (usage_priced, unclassified) = loop {
            let (priced, unknown) = self.billing_among(&offering);
            if unknown.is_empty() || tokio::time::Instant::now() >= deadline {
                break (priced, unknown);
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        };
        // A profile with no quota report yet may be a paid API. Persisting a
        // guess would keep its models in the reviewers for good, so wait for a
        // later attempt whenever such a profile offers a drafted family.
        let waiting = crate::review_selection::unclassified_in_draft(
            &legacy,
            &available,
            &usage_priced,
            &unclassified,
        );
        if !waiting.is_empty() {
            anyhow::bail!(
                "legacy review upgrade waiting for quota reports: {}",
                waiting.join(", ")
            );
        }
        let (reviewers, known, adjustments) = crate::review_selection::upgrade_reviewers(
            &legacy,
            &available,
            &usage_priced,
            |profile, model| {
                let catalog = Arc::clone(self);
                async move { catalog.model_capabilities(profile, model).await }
            },
            None,
        )
        .await?;
        if !known {
            anyhow::bail!("reviewer efforts remain unknown");
        }
        let disable_auto = reviewers.is_empty();
        if disable_auto
            && (legacy
                .profile
                .as_ref()
                .is_some_and(|profile| available.iter().any(|(id, _)| id == profile))
                || !omitted.is_empty())
        {
            anyhow::bail!("legacy review upgrade found no reviewers; waiting for matching models");
        }
        let count = reviewers.len();
        let configured_notice = (!reviewers.is_empty()).then(|| {
            let rows = reviewers
                .iter()
                .map(|choice| match &choice.effort {
                    Some(effort) => format!("{} {effort}", choice.model),
                    None => format!("{} default", choice.model),
                })
                .collect::<Vec<_>>()
                .join(", ");
            let changes = adjustments
                .iter()
                .map(|adjustment| {
                    format!(
                        "{} uses {} ({} is not offered)",
                        adjustment.model,
                        adjustment.used.as_deref().unwrap_or("default"),
                        adjustment.requested
                    )
                })
                .collect::<Vec<_>>();
            if changes.is_empty() {
                format!("Code review reviewers were configured from your profiles: {rows}.")
            } else {
                format!(
                    "Code review reviewers were configured from your profiles: {rows}; {}.",
                    changes.join("; ")
                )
            }
        });
        let path = self
            .config_path
            .clone()
            .context("catalog has no config path for review upgrade")?;
        let wrote = tokio::task::spawn_blocking(move || {
            Self::persist_legacy_review_to(&path, reviewers, disable_auto, &definitions, &legacy)
        })
        .await??;
        if wrote {
            if disable_auto {
                let message = "Automatic code review was turned off: none of your profiles offers a model Autoconfigure supports. Choose reviewers in Settings › Code Review.";
                tracing::warn!("{message}");
                if let Some(notice) = self
                    .notice
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .clone()
                {
                    notice(message.to_owned());
                }
            }
            if let Some(message) = configured_notice
                && let Some(notice) = self
                    .notice
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .clone()
            {
                notice(message);
            }
            tracing::info!(
                count,
                "persisted legacy Auto review configuration to config.toml"
            );
        }
        Ok(())
    }

    fn persist_legacy_review_to(
        path: &Path,
        reviewers: Vec<mj_core::config::SubagentChoice>,
        disable_auto: bool,
        definitions: &[UpgradeDefinition],
        legacy: &mj_core::config::LegacyReview,
    ) -> Result<bool> {
        if reviewers.is_empty() && !disable_auto {
            anyhow::bail!("cannot persist an empty reviewer upgrade");
        }
        let (_, wrote) = Config::update_to(path, |config| {
            if !config.review.is_legacy() {
                return Ok(false);
            }
            if config.review.legacy.as_ref() != Some(legacy) {
                anyhow::bail!("legacy review settings changed during reviewer discovery");
            }
            if upgrade_definitions(config) != definitions {
                anyhow::bail!("profile definitions changed during reviewer discovery");
            }
            config.review.reviewers = reviewers;
            if disable_auto {
                config.review.trigger = mj_core::config::ReviewTrigger::Off;
            }
            config.review.clear_legacy();
            Ok(true)
        })?;
        Ok(wrote)
    }

    /// Claim new definitions, default probes and the efforts of the models
    /// `config` selects under one lock. A Settings draft uses this same path
    /// without becoming the live configuration.
    pub(crate) fn ensure(self: &Arc<Self>, config: &Config) {
        let config = Arc::new(config.clone());
        let mut started = Vec::new();
        {
            let mut inner = self.lock();
            for (id, profile) in config.enabled_profiles() {
                let identity = profile.capabilities_key(id);
                inner
                    .wanted
                    .entry(identity.clone())
                    .or_default()
                    .extend(config.selected_models(id));
                // Muse catalogues change server-side without a managed-pin
                // bump, so a retained entry hides models and efforts people
                // expect. Every warm pass re-probes a Muse profile instead of
                // keeping its entries; reads below never serve them either.
                let live = profile.kind == HarnessKind::Muse;
                if let Some(definition) = inner.definitions.get_mut(&identity) {
                    if !live && !definition.cancellation.is_cancelled() {
                        // Keep successful entries and the in-flight attempt.
                        // A failed owner's next retry uses refreshed credentials.
                        let models_changed = definition
                            .config
                            .profiles
                            .get(&definition.id)
                            .map(|old| &old.models)
                            != Some(&profile.models);
                        definition.config = config.clone();
                        let definition = definition.clone();
                        if models_changed {
                            inner.publication = None;
                        }
                        if let Some(Entry::Ready(choices)) =
                            inner.entries.get(&(identity.clone(), None)).cloned()
                        {
                            started.extend(self.claim_models(
                                &mut inner,
                                &definition,
                                &identity,
                                &choices,
                            ));
                        }
                        continue;
                    }
                    if !live {
                        inner.entries.retain(|(entry_identity, _), entry| {
                            entry_identity != &identity || matches!(entry, Entry::Ready(_))
                        });
                    }
                }
                inner.publication = None;
                if live {
                    // End the previous owner's supervisors so only the fresh
                    // attempt below can publish for this identity.
                    if let Some(definition) = inner.definitions.get(&identity) {
                        definition.cancellation.cancel();
                    }
                    inner
                        .entries
                        .retain(|(entry_identity, _), _| entry_identity != &identity);
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
                    started.extend(self.claim_models(&mut inner, &definition, &identity, &choices));
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

    /// Read a live profile's failed entries again, after a session's worker
    /// put choices in the profile cache that discovery reads first. A
    /// replaced entry ends its old supervisor, which no longer owns it.
    pub(crate) fn reread_failed(self: &Arc<Self>, profile_id: &str) {
        let mut started = Vec::new();
        {
            let mut inner = self.edit();
            let Some(identity) = inner
                .live
                .as_ref()
                .and_then(|config| config.enabled_profile(profile_id))
                .map(|profile| profile.capabilities_key(profile_id))
            else {
                return;
            };
            let Some(definition) = inner
                .definitions
                .get(&identity)
                .filter(|definition| !definition.cancellation.is_cancelled())
                .cloned()
            else {
                return;
            };
            let failed = inner
                .entries
                .iter()
                .filter(|((entry_identity, _), entry)| {
                    *entry_identity == identity && matches!(entry, Entry::Failed { .. })
                })
                .map(|(key, _)| key.clone())
                .collect::<Vec<_>>();
            for key in failed {
                let attempt = self.attempt(&definition, key.1.clone());
                inner
                    .entries
                    .insert(key.clone(), Entry::Pending(attempt.clone()));
                started.push((key, attempt));
            }
        }
        if !started.is_empty() {
            self.publish();
        }
        for (key, attempt) in started {
            self.spawn(key, attempt);
        }
    }

    /// Claim an entry for the current model and each selected model `choices`
    /// lists that has none. The current model's efforts are already known; a
    /// selected model is probed. Every probe launches the harness, and
    /// aggregating harnesses such as OpenCode advertise hundreds of models,
    /// so a model nobody selected is probed only when a reader asks for it.
    fn claim_models(
        &self,
        inner: &mut Inner,
        definition: &Definition,
        identity: &str,
        choices: &ProfileConfig,
    ) -> Vec<(CacheKey, Attempt)> {
        let mut started = Vec::new();
        for model in &choices.models {
            let model_key = (identity.to_owned(), Some(model.value.clone()));
            let current = choices.model.as_ref() == Some(&model.value);
            let selected = inner
                .wanted
                .get(identity)
                .is_some_and(|wanted| wanted.contains(&model.value));
            if inner.entries.contains_key(&model_key) || !(current || selected) {
                continue;
            }
            inner.publication = None;
            if current {
                inner
                    .entries
                    .insert(model_key, Entry::Ready(choices.clone()));
            } else {
                let pending = self.attempt(definition, Some(model.value.clone()));
                inner
                    .entries
                    .insert(model_key.clone(), Entry::Pending(pending.clone()));
                started.push((model_key, pending));
            }
        }
        started
    }

    /// Check ready entries against the choice lifetime until shutdown.
    fn start_refreshing(self: &Arc<Self>) {
        let catalog = Arc::downgrade(self);
        let cancellation = self.cancellation.clone();
        tokio::spawn(async move {
            // Startup discovery is already current; the first check waits.
            let mut tick = tokio::time::interval_at(
                tokio::time::Instant::now() + REFRESH_CHECK_INTERVAL,
                REFRESH_CHECK_INTERVAL,
            );
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tokio::select! {
                    _ = cancellation.cancelled() => return,
                    _ = tick.tick() => {}
                }
                let Some(catalog) = catalog.upgrade() else {
                    return;
                };
                catalog.refresh(|choices| !crate::controller::profile_config::is_current(choices));
            }
        });
    }

    /// Probe again every ready entry that `expired` selects. Readers keep
    /// the entry until a probe succeeds; a failed probe leaves it in place.
    fn refresh(self: &Arc<Self>, expired: impl Fn(&ProfileConfig) -> bool) {
        let mut started = Vec::new();
        {
            let mut inner = self.lock();
            let keys = inner
                .entries
                .iter()
                .filter(|(key, entry)| {
                    matches!(entry, Entry::Ready(choices) if expired(choices))
                        && !inner.refreshing.contains_key(*key)
                })
                .map(|(key, _)| key.clone())
                .collect::<Vec<_>>();
            for key in keys {
                let Some(definition) = inner
                    .definitions
                    .get(&key.0)
                    .filter(|definition| !definition.cancellation.is_cancelled())
                    .cloned()
                else {
                    continue;
                };
                let attempt = self.attempt(&definition, key.1.clone());
                inner.refreshing.insert(key.clone(), attempt.clone());
                started.push((key, attempt, definition.cancellation));
            }
        }
        for (key, attempt, cancellation) in started {
            tokio::spawn(self.clone().drive_refresh(key, attempt, cancellation));
        }
    }

    async fn drive_refresh(
        self: Arc<Self>,
        key: CacheKey,
        attempt: Attempt,
        cancellation: CancellationToken,
    ) {
        let result = tokio::select! {
            _ = cancellation.cancelled() => None,
            result = attempt.clone() => Some(result),
        };
        let (changed, started) = {
            let mut inner = self.edit();
            if !inner
                .refreshing
                .get(&key)
                .is_some_and(|current| current.ptr_eq(&attempt))
            {
                return;
            }
            inner.refreshing.remove(&key);
            let Some(Ok(choices)) = result else {
                return;
            };
            let Some(Entry::Ready(previous)) = inner.entries.get(&key).cloned() else {
                return;
            };
            inner
                .entries
                .insert(key.clone(), Entry::Ready(choices.clone()));
            let mut started = Vec::new();
            if key.1.is_none() {
                let definition = inner.definitions[&key.0].clone();
                started = self.claim_models(&mut inner, &definition, &key.0, &choices);
            }
            let changed = !started.is_empty()
                || ProfileConfig {
                    observed_at: choices.observed_at,
                    ..previous
                } != choices;
            (changed, started)
        };
        if changed {
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
                .map_err(|error| AttemptError::new(&error))
                .boxed()
                .shared();
        }
        crate::controller::profile_config::discover_for(
            (*definition.config).clone(),
            definition.id.clone(),
            model,
            definition.cancellation.clone(),
        )
        .map_err(|error| AttemptError::new(&error))
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
                    let mut inner = self.edit();
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
                                started.extend(self.claim_models(
                                    &mut inner,
                                    &definition,
                                    &key.0,
                                    choices,
                                ));
                            }
                        }
                        Err(AttemptError { message, retryable }) => {
                            inner.entries.insert(
                                key.clone(),
                                Entry::Failed {
                                    attempt: attempt.clone(),
                                    error: message.clone(),
                                },
                            );
                            let profile = &inner.definitions[&key.0].id;
                            if *retryable {
                                tracing::warn!(%profile, model = ?key.1, error = %message, "profile capability hydration failed; retrying with backoff");
                            } else {
                                tracing::info!(%profile, model = ?key.1, error = %message, "profile capabilities stay unknown");
                            }
                        }
                    }
                }
                self.publish();
                for (key, attempt) in started {
                    self.spawn(key, attempt);
                }
                // A failure no retry can fix stays the entry's state until a
                // changed definition replaces it.
                if !matches!(
                    &result,
                    Err(AttemptError {
                        retryable: true,
                        ..
                    })
                ) {
                    return;
                }
                let delay = RETRY_DELAYS[(failures as usize).min(RETRY_DELAYS.len() - 1)];
                failures = failures.saturating_add(1);
                tokio::select! {
                    _ = cancellation.cancelled() => return,
                    _ = tokio::time::sleep(delay) => {}
                }
                {
                    let mut inner = self.edit();
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

    /// What `id` offers under its `models` allow-list. Every reader of a
    /// profile's choices comes through here or through [`Self::snapshot`].
    async fn fetch(
        self: &Arc<Self>,
        config: &Config,
        id: &str,
        model: Option<String>,
    ) -> Result<ProfileConfig> {
        let profile = config.enabled_profile(id).context("profile unavailable")?;
        let identity = profile.capabilities_key(id);
        // Model-specific probes select that model to discover its efforts. Only
        // the default probe can grant the default-model exemption.
        let specific = model.is_some();
        let mut choices = self.fetch_unrestricted(config, id, model).await?;
        let allowed = self.lock().allowed_models(&identity);
        if allowed.is_empty() {
            return Ok(choices);
        }
        let selected = choices.model.clone();
        // A default read already carries the default model. A model-specific
        // read needs the default probe for the exemption, and only when a
        // list applies: Muse probes fresh on every read.
        let default = if specific {
            self.fetch_unrestricted(config, id, None).await?.model
        } else {
            selected.clone()
        };
        choices.model = default;
        // The list may have changed during the probes; apply the current one.
        let allowed = self.lock().allowed_models(&identity);
        choices = choices.restricted_to(&allowed);
        choices.model = selected;
        Ok(choices)
    }

    /// What `id`'s harness advertises, before the allow-list. Only `mj doctor`
    /// asks for this, to compare the allow-list with the catalogue.
    async fn fetch_unrestricted(
        self: &Arc<Self>,
        config: &Config,
        id: &str,
        model: Option<String>,
    ) -> Result<ProfileConfig> {
        let profile = config.enabled_profile(id).context("profile unavailable")?;
        let live = profile.kind == HarnessKind::Muse;
        self.ensure(config);
        let key = (profile.capabilities_key(id), model);
        let cancellation = self.lock().definitions[&key.0].cancellation.clone();
        let mut revisions = self.revisions.subscribe();
        if live {
            return self
                .fetch_live(&key, key.1.clone(), cancellation, &mut revisions)
                .await;
        }
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
                    inner.publication = None;
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

    /// Read a Muse profile live instead of serving a retained entry. New
    /// models and effort tiers appear server-side at any time, so a retained
    /// answer is never served: every read probes the harness that actually
    /// runs. An in-flight probe is still joined so concurrent readers share
    /// it, and failures are served as-is so a broken harness keeps the
    /// supervisor's backoff instead of being hammered on every read. The
    /// supervisor still publishes each result, so the snapshot converges.
    async fn fetch_live(
        self: &Arc<Self>,
        key: &CacheKey,
        model: Option<String>,
        cancellation: CancellationToken,
        revisions: &mut tokio::sync::watch::Receiver<u64>,
    ) -> Result<ProfileConfig> {
        if model.is_some() {
            // Efforts are discovered against the default catalogue; wait for
            // it the way a shared reader would.
            loop {
                match self.lock().entries.get(&(key.0.clone(), None)) {
                    Some(Entry::Ready(_)) => break,
                    Some(Entry::Failed { error, .. }) => {
                        return Err(anyhow::anyhow!("{error}"));
                    }
                    _ => {}
                }
                tokio::select! {
                    biased;
                    _ = self.cancellation.cancelled() => bail!("profile discovery cancelled by daemon shutdown"),
                    _ = cancellation.cancelled() => bail!("profile definition retired"),
                    result = revisions.changed() => { result.context("profile capability publisher stopped")?; }
                }
            }
        }
        let (attempt, started) = {
            let mut inner = self.lock();
            match inner.entries.get(key) {
                Some(Entry::Pending(attempt)) => (attempt.clone(), false),
                Some(Entry::Failed { error, .. }) => {
                    return Err(anyhow::anyhow!("{error}"));
                }
                _ => {
                    let attempt = self.attempt(&inner.definitions[&key.0], key.1.clone());
                    inner.publication = None;
                    inner
                        .entries
                        .insert(key.clone(), Entry::Pending(attempt.clone()));
                    (attempt, true)
                }
            }
        };
        if started {
            self.spawn(key.clone(), attempt.clone());
            self.publish();
        }
        attempt
            .await
            .map_err(|error| anyhow::anyhow!("{}", error.message))
    }

    pub(crate) fn live(&self) -> Result<Config> {
        self.lock()
            .live
            .clone()
            .context("the profile catalogue has not adopted a configuration")
    }

    /// The live `models` allow-list of profile `id`; empty offers everything.
    pub(crate) fn allowed_models(&self, id: &str) -> Vec<String> {
        self.lock()
            .live
            .as_ref()
            .and_then(|config| config.enabled_profile(id))
            .map(|profile| profile.models.clone())
            .unwrap_or_default()
    }

    pub(crate) fn candidates(&self) -> Result<Vec<(String, HarnessKind)>> {
        let config = self.live()?;
        Ok(config
            .enabled_profiles()
            .map(|(id, profile)| (id.into(), profile.kind))
            .collect())
    }

    pub(crate) fn ready_candidates(&self, model: &str) -> Result<Vec<(String, HarnessKind)>> {
        let config = self.live()?;
        let snapshot = self.snapshot();
        Ok(config
            .enabled_profiles()
            .filter(|(id, profile)| {
                snapshot
                    .profiles
                    .get(&profile.capabilities_key(id))
                    .is_some_and(|entry| {
                        matches!(&entry.choices, CapabilityState::Ready(choices)
                        if model.is_empty() || choices.models.iter().any(|choice| choice.value == model))
                    })
            })
            .map(|(id, profile)| (id.to_owned(), profile.kind))
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

    /// The catalogue `id`'s harness advertises, ignoring its `models`
    /// allow-list. See [`Self::fetch_unrestricted`].
    pub(crate) async fn unrestricted_capabilities(
        self: &Arc<Self>,
        id: &str,
    ) -> Result<ProfileConfig> {
        self.read_live(id, None, true).await
    }

    async fn live_capabilities(
        self: &Arc<Self>,
        id: &str,
        model: Option<String>,
    ) -> Result<ProfileConfig> {
        self.read_live(id, model, false).await
    }

    async fn read_live(
        self: &Arc<Self>,
        id: &str,
        model: Option<String>,
        unrestricted: bool,
    ) -> Result<ProfileConfig> {
        loop {
            let config = self.live()?;
            let identity = config
                .enabled_profile(id)
                .context("profile unavailable")?
                .capabilities_key(id);
            let result = if unrestricted {
                self.fetch_unrestricted(&config, id, model.clone()).await
            } else {
                self.fetch(&config, id, model.clone()).await
            };
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

    fn edit(&self) -> MutexGuard<'_, Inner> {
        let mut inner = self.lock();
        inner.publication = None;
        inner
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

/// One enabled profile as the legacy review upgrade saw it: its id, its
/// capability key, and its `models` list. The key leaves `models` out, so
/// the save also compares the list: a profile that stopped offering a
/// chosen model must not get it saved as a reviewer.
type UpgradeDefinition = (String, String, Vec<String>);

fn upgrade_definitions(config: &Config) -> Vec<UpgradeDefinition> {
    config
        .enabled_profiles()
        .map(|(id, profile)| {
            (
                id.to_owned(),
                profile.capabilities_key(id),
                profile.models.clone(),
            )
        })
        .collect()
}

/// A configuration with the named profiles enabled.
#[cfg(test)]
pub(crate) fn test_config(profiles: &[(&str, HarnessKind)]) -> Config {
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
                        models: Default::default(),
                        legacy_subagents: Default::default(),
                        guardian_review_model: None,
                        codex_profile: None,
                        muse_auto_review: None,
                    },
                )
            })
            .collect(),
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

    // Hard-won: 8c88175f7: reads rebuilt the entire unchanged model catalog.
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
        let mut config = test_config(&[("parent", HarnessKind::Codex)]);
        catalog.sync(&config);
        let reader = {
            let catalog = catalog.clone();
            let config = config.clone();
            tokio::spawn(async move {
                catalog
                    .options_for(&config, "parent", Some("other".into()))
                    .await
            })
        };
        assert_eq!(started.recv().await.as_deref(), Some("parent"));
        assert!(catalog.snapshot().options(&config, None).is_some());
        let partial = catalog.snapshot().options(&config, Some("other")).unwrap();
        assert!(partial.efforts.is_empty());
        assert!(
            partial
                .unavailable
                .iter()
                .any(|reason| reason.contains("effort discovery pending"))
        );
        // Adding a different profile must leave this model's pending owner intact.
        config.profiles.insert(
            "sibling".into(),
            test_config(&[("sibling", HarnessKind::Claude)])
                .profiles
                .remove("sibling")
                .unwrap(),
        );
        catalog.sync(&config);
        assert!(!reader.is_finished());
        assert_eq!(calls.load(Ordering::SeqCst), 1);
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
        let held = catalog.snapshot();
        catalog
            .options_for(&config, "parent", Some("other".into()))
            .await
            .unwrap();
        assert!(held.ptr_eq(&catalog.snapshot()));
    }

    // Hard-won: #1380: warming launched one harness per advertised model.
    #[tokio::test]
    async fn only_selected_models_launch_effort_probes() {
        let probed = Arc::new(Mutex::new(Vec::<String>::new()));
        let model_probe: Arc<ModelProbe> = {
            let probed = probed.clone();
            Arc::new(move |id, model| {
                probed.lock().unwrap().push(model.clone());
                Box::pin(async move {
                    let mut choices = test_choices(&id);
                    choices.model = Some(model);
                    Ok(choices)
                })
            })
        };
        // An aggregating harness advertises hundreds of provider models.
        let catalog = ProfileCatalog::build(
            CancellationToken::new(),
            Arc::new(|id| {
                Box::pin(async move {
                    let mut choices = test_choices(&id);
                    choices
                        .models
                        .extend((0..300).map(|n| mj_core::acp::SessionConfigChoice {
                            value: format!("provider/model-{n}"),
                            name: format!("Model {n}"),
                            description: None,
                        }));
                    Ok(choices)
                })
            }),
            model_probe,
        );
        let mut config = test_config(&[("opencode", HarnessKind::OpenCode)]);
        config.subagents.straightforward = Some(mj_core::config::SubagentChoice {
            model: "provider/model-7".into(),
            effort: None,
        });
        catalog.sync(&config);
        let settled = |model: &'static str| {
            let catalog = catalog.clone();
            let config = config.clone();
            async move {
                tokio::time::timeout(Duration::from_secs(10), async {
                    while catalog.snapshot().options(&config, Some(model)).is_none() {
                        tokio::task::yield_now().await;
                    }
                })
                .await
                .expect("selected efforts are discovered");
            }
        };
        settled("provider/model-7").await;
        assert_eq!(*probed.lock().unwrap(), ["provider/model-7"]);

        // A Settings draft selecting another model probes just that one, and
        // only the models probed are published.
        let mut draft = config.clone();
        draft.subagents.subtle = Some(mj_core::config::SubagentChoice {
            model: "provider/model-42".into(),
            effort: None,
        });
        catalog.ensure(&draft);
        settled("provider/model-42").await;
        assert_eq!(
            *probed.lock().unwrap(),
            ["provider/model-7", "provider/model-42"]
        );
        let key = config.profiles["opencode"].capabilities_key("opencode");
        assert_eq!(
            catalog.snapshot().profiles[&key]
                .efforts
                .keys()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            ["opencode-model", "provider/model-42", "provider/model-7"]
        );
    }

    /// Muse catalogues change server-side without a managed-pin bump, so a
    /// retained answer hides models and efforts people expect. Every Muse
    /// read probes live; other harnesses keep serving their warm entries.
    #[tokio::test]
    async fn muse_reads_probe_live_while_other_harnesses_stay_warm() {
        let calls = calls();
        let probe: Arc<Probe> = {
            let calls = calls.clone();
            Arc::new(move |profile: String| {
                let calls = calls.clone();
                Box::pin(async move {
                    let revision = calls.fetch_add(1, Ordering::SeqCst);
                    let model = format!("{profile}-model-{revision}");
                    let mut choices = test_choices(&profile);
                    choices.model = Some(model.clone());
                    choices.models = vec![mj_core::acp::SessionConfigChoice {
                        value: model.clone(),
                        name: model,
                        description: None,
                    }];
                    Ok(choices)
                })
            })
        };
        let catalog = ProfileCatalog::with_probe(probe);
        let config = test_config(&[("muse", HarnessKind::Muse), ("codex", HarnessKind::Codex)]);
        catalog.sync_now(&config).await;

        let first = catalog.capabilities(&["muse".into()]).await.unwrap();
        let first = first[0].models[0].value.clone();
        // Wait until the supervisor published this probe, so the next read
        // cannot join the same in-flight attempt: it must start its own.
        let key = config.profiles["muse"].capabilities_key("muse");
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let published =
                    catalog
                        .snapshot()
                        .profiles
                        .get(&key)
                        .and_then(|entry| match &entry.choices {
                            CapabilityState::Ready(choices) => {
                                choices.models.first().map(|choice| choice.value.clone())
                            }
                            _ => None,
                        });
                if published.as_deref() == Some(first.as_str()) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the supervisor publishes each live Muse probe");

        let second = catalog.capabilities(&["muse".into()]).await.unwrap();
        assert_ne!(
            second[0].models[0].value, first,
            "a Muse read never serves the retained catalogue"
        );

        let codex_first = catalog.capabilities(&["codex".into()]).await.unwrap();
        let codex_second = catalog.capabilities(&["codex".into()]).await.unwrap();
        assert_eq!(
            codex_first[0].models[0].value, codex_second[0].models[0].value,
            "other harnesses keep serving their warm entries"
        );
    }

    #[tokio::test]
    async fn concurrent_readers_join_startup_hydration() {
        let calls = calls();
        let (started_tx, mut started) = tokio::sync::mpsc::unbounded_channel();
        let (release, gate) = tokio::sync::watch::channel(false);
        let catalog = ProfileCatalog::with_probe(gated_probe(calls.clone(), started_tx, gate));
        let config = test_config(&[
            ("parent", HarnessKind::Codex),
            ("helper", HarnessKind::Claude),
        ]);
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
        catalog.sync(&config);
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert_eq!(catalog.candidates().unwrap().len(), 2);
        assert!(catalog.snapshot().options(&config, None).is_none());
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
        let mut config = test_config(&[("parent", HarnessKind::Codex)]);
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
            test_config(&[("sibling", HarnessKind::Claude)])
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
        let live = test_config(&[("parent", HarnessKind::Codex)]);
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

    #[tokio::test]
    async fn expired_entries_are_probed_again_and_kept_until_a_probe_succeeds() {
        let calls = calls();
        let failing = Arc::new(AtomicBool::new(false));
        let catalog = ProfileCatalog::with_probe(flag_probe(failing.clone(), calls.clone()));
        let config = test_config(&[("parent", HarnessKind::Codex)]);
        catalog.sync_now(&config).await;
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        catalog.refresh(|_| false);
        assert_eq!(calls.load(Ordering::SeqCst), 1, "current entries stay");

        failing.store(true, Ordering::SeqCst);
        catalog.refresh(|_| true);
        while !catalog.lock().refreshing.is_empty() {
            tokio::task::yield_now().await;
        }
        // The default entry and the current model's copy are both probed.
        assert_eq!(calls.load(Ordering::SeqCst), 3);
        assert_eq!(
            catalog.capabilities(&["parent".into()]).await.unwrap()[0]
                .model
                .as_deref(),
            Some("parent-model"),
            "a failed refresh keeps serving the old choices"
        );

        failing.store(false, Ordering::SeqCst);
        catalog.refresh(|_| true);
        while !catalog.lock().refreshing.is_empty() {
            tokio::task::yield_now().await;
        }
        assert_eq!(calls.load(Ordering::SeqCst), 5);
        assert!(catalog.capabilities(&["parent".into()]).await.is_ok());
    }

    #[tokio::test(start_paused = true)]
    async fn failures_are_reported_and_only_the_owner_retries_after_backoff() {
        let calls = calls();
        let failing = Arc::new(AtomicBool::new(true));
        let catalog = ProfileCatalog::with_probe(flag_probe(failing.clone(), calls.clone()));
        let config = test_config(&[("parent", HarnessKind::Codex)]);
        catalog.sync_now(&config).await;
        for _ in 0..3 {
            assert!(catalog.capabilities(&["parent".into()]).await.is_err());
        }
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "failed UI queries cannot launch retries"
        );
        let options = catalog.snapshot().options(&config, None).unwrap();
        assert!(options.unavailable[0].contains("harness is not installed"));
        failing.store(false, Ordering::SeqCst);
        tokio::time::advance(Duration::from_secs(1)).await;
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }
        catalog.capabilities(&["parent".into()]).await.unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    // Hard-won: #1232: Windows has no local worker, so every probe failed and the owner retried forever.
    #[tokio::test(start_paused = true)]
    async fn a_discovery_no_retry_can_fix_waits_for_a_session_to_report_choices() {
        let calls = calls();
        let probe_calls = calls.clone();
        // Discovery reads the profile cache first; this flag stands for a
        // session's worker having put its choices there.
        let observed = Arc::new(AtomicBool::new(false));
        let cached = observed.clone();
        let catalog = ProfileCatalog::with_probe(Arc::new(move |profile| {
            probe_calls.fetch_add(1, Ordering::SeqCst);
            let cached = cached.load(Ordering::SeqCst);
            Box::pin(async move {
                if cached {
                    Ok(test_choices(&profile))
                } else {
                    Err(crate::controller::profile_config::DiscoveryUnsupported.into())
                }
            })
        }));
        let config = test_config(&[("parent", HarnessKind::Codex)]);
        catalog.sync_now(&config).await;
        tokio::time::advance(Duration::from_secs(600)).await;
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        let options = catalog.snapshot().options(&config, None).unwrap();
        assert!(
            options.unavailable[0].contains("runs no local Mjolnir worker"),
            "{options:?}"
        );

        observed.store(true, Ordering::SeqCst);
        catalog.reread_failed("parent");
        assert_eq!(
            catalog.fetch(&config, "parent", None).await.unwrap(),
            test_choices("parent")
        );
        let options = catalog.snapshot().options(&config, None).unwrap();
        assert!(options.unavailable.is_empty(), "{options:?}");
        assert_eq!(options.models, test_choices("parent").models);
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
        catalog.sync(&test_config(&[("parent", HarnessKind::Codex)]));
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
                .candidates()
                .unwrap_err()
                .to_string()
                .contains("not adopted")
        );
        assert!(catalog.capabilities(&["parent".into()]).await.is_err());
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn subagent_candidates_include_every_enabled_profile() {
        let catalog = ProfileCatalog::new(CancellationToken::new());
        let mut config = test_config(&[
            ("codex", HarnessKind::Codex),
            ("helper", HarnessKind::Codex),
            ("claude4", HarnessKind::Claude),
            ("disabled-parent", HarnessKind::Kimi),
        ]);
        config.profiles.get_mut("disabled-parent").unwrap().enabled = false;
        catalog.sync(&config);

        let subagent_ids = catalog
            .candidates()
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

        assert_eq!(
            subagent_ids,
            vec![
                "claude4".to_owned(),
                "codex".to_owned(),
                "helper".to_owned()
            ]
        );
        assert!(!subagent_ids.iter().any(|id| id == "disabled-parent"));
        assert_eq!(
            configured_ids,
            vec![
                "claude4".to_owned(),
                "codex".to_owned(),
                "helper".to_owned()
            ]
        );
    }
    fn legacy_upgrade_fixture(
        path: PathBuf,
        codex_offline: Arc<AtomicBool>,
    ) -> Arc<ProfileCatalog> {
        let probe: Arc<Probe> = Arc::new(move |profile| {
            let offline = codex_offline.load(Ordering::SeqCst);
            Box::pin(async move {
                if profile == "codex" && offline {
                    anyhow::bail!("codex profile is unavailable");
                }
                let models = if profile == "codex" {
                    vec!["gpt-6-terra", "gpt-6-sol"]
                } else {
                    vec!["claude-haiku-4.5"]
                };
                Ok(ProfileConfig {
                    model: Some(models[0].into()),
                    models: models
                        .into_iter()
                        .map(|value| mj_core::acp::SessionConfigChoice {
                            value: value.into(),
                            name: value.into(),
                            description: None,
                        })
                        .collect(),
                    efforts: Vec::new(),
                    observed_at: 1,
                })
            })
        });
        let mut catalog = ProfileCatalog::build(
            CancellationToken::new(),
            probe.clone(),
            Arc::new(move |profile, _| probe(profile)),
        );
        Arc::get_mut(&mut catalog).unwrap().config_path = Some(path);
        let mut config = test_config(&[
            ("codex", HarnessKind::Codex),
            ("haiku", HarnessKind::Claude),
        ]);
        config.review = mj_core::config::ReviewConfig::legacy_default();
        catalog.sync(&config);
        catalog
    }

    fn legacy_review_file(path: &Path) {
        legacy_review_file_with(
            path,
            &[
                ("codex", HarnessKind::Codex),
                ("haiku", HarnessKind::Claude),
            ],
        );
    }

    fn legacy_review_file_with(path: &Path, profiles: &[(&str, HarnessKind)]) {
        let mut config = test_config(profiles);
        for profile in config.profiles.values_mut() {
            if profile.kind == HarnessKind::OpenCode {
                profile.home.push("opencode");
            }
        }
        config.review = mj_core::config::ReviewConfig::legacy_default();
        config.review.trigger = mj_core::config::ReviewTrigger::Turn;
        config.save_to(path).unwrap();
    }

    #[tokio::test]
    async fn partial_legacy_discovery_retries_before_persisting_to_its_config_path() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("custom-config.toml");
        legacy_review_file(&path);
        let offline = Arc::new(AtomicBool::new(true));
        let catalog = legacy_upgrade_fixture(path.clone(), offline.clone());
        let error = catalog.upgrade_legacy_review(1).await.unwrap_err();
        assert!(error.to_string().contains("codex"));
        assert!(Config::load_from(&path).unwrap().review.is_legacy());
        offline.store(false, Ordering::SeqCst);
        catalog.reread_failed("codex");
        catalog.upgrade_legacy_review(2).await.unwrap();
        let review = Config::load_from(&path).unwrap().review;
        assert!(!review.is_legacy());
        assert_eq!(review.reviewers.len(), 3);
        assert_eq!(review.reviewers[0].model, "gpt-6-terra");
    }

    /// The report a refresh produces for a subscription profile.
    fn subscription_report(profile: &str, harness: HarnessKind) -> ProfileQuota {
        ProfileQuota {
            paid_credits: None,
            banked_resets: None,
            profile_id: profile.into(),
            harness,
            windows: vec![mj_client::quota::QuotaWindow {
                label: "Week".into(),
                remaining_percent: Some(80),
                duration_seconds: None,
                model_scope: None,
                used: None,
                limit: None,
                resets: None,
                resets_at_epoch_seconds: None,
            }],
            extra: None,
            error: None,
            refreshed_at_epoch_seconds: 0,
            rate_limited_until_epoch_seconds: None,
        }
    }

    /// The report a refresh produces for an OpenCode profile.
    fn not_reported(profile: &str) -> ProfileQuota {
        ProfileQuota {
            windows: Vec::new(),
            extra: Some(mj_client::quota::NOT_REPORTED_LABEL.to_owned()),
            ..subscription_report(profile, HarnessKind::OpenCode)
        }
    }

    /// A catalog over codex, haiku and an OpenCode "openrouter" profile whose
    /// catalogue holds only `latest` aliases, with quota reports `reports`.
    fn billing_catalog(path: &Path, reports: QuotaReports) -> Arc<ProfileCatalog> {
        let profiles = [
            ("codex", HarnessKind::Codex),
            ("haiku", HarnessKind::Claude),
            ("openrouter", HarnessKind::OpenCode),
        ];
        legacy_review_file_with(path, &profiles);
        let probe: Arc<Probe> = Arc::new(|profile| {
            Box::pin(async move {
                let models = match profile.as_str() {
                    "codex" => vec!["gpt-6.1-terra", "gpt-6.1-sol"],
                    "haiku" => vec!["haiku"],
                    _ => vec![
                        "openrouter/~openai/gpt-terra-latest",
                        "openrouter/~openai/gpt-sol-latest",
                        "openrouter/~anthropic/claude-haiku-latest",
                    ],
                };
                Ok(ProfileConfig {
                    model: None,
                    models: models
                        .into_iter()
                        .map(|value| mj_core::acp::SessionConfigChoice {
                            value: value.into(),
                            name: if value == "haiku" { "Haiku 4.5" } else { value }.into(),
                            description: None,
                        })
                        .collect(),
                    efforts: Vec::new(),
                    observed_at: 1,
                })
            })
        });
        let mut catalog = ProfileCatalog::build(
            CancellationToken::new(),
            probe.clone(),
            Arc::new(move |profile, _| probe(profile)),
        );
        Arc::get_mut(&mut catalog).unwrap().config_path = Some(path.to_owned());
        catalog.set_quota_reports(reports);
        let mut config = Config::load_from(path).unwrap();
        config.review = mj_core::config::ReviewConfig::legacy_default();
        catalog.sync(&config);
        catalog
    }

    fn upgraded_models(path: &Path) -> Vec<String> {
        Config::load_from(path)
            .unwrap()
            .review
            .reviewers
            .into_iter()
            .map(|choice| choice.model)
            .collect()
    }

    #[tokio::test(start_paused = true)]
    async fn legacy_upgrade_skips_models_only_opencode_profiles_offer() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("custom-config.toml");
        // The OpenCode profile's real report is `n/a`, not an API label.
        let reports: QuotaReports = Arc::new(Mutex::new(BTreeMap::from([
            (
                "codex".to_owned(),
                subscription_report("codex", HarnessKind::Codex),
            ),
            ("openrouter".to_owned(), not_reported("openrouter")),
        ])));
        let catalog = billing_catalog(&path, reports.clone());
        let upgrade = {
            let catalog = Arc::clone(&catalog);
            tokio::spawn(async move { catalog.upgrade_legacy_review(1).await })
        };
        // The upgrade waits for the Claude profile's first report, whose
        // billing is not known otherwise.
        tokio::time::advance(Duration::from_secs(5)).await;
        assert!(Config::load_from(&path).unwrap().review.is_legacy());
        reports.lock().unwrap().insert(
            "haiku".into(),
            subscription_report("haiku", HarnessKind::Claude),
        );
        upgrade.await.unwrap().unwrap();
        assert!(!Config::load_from(&path).unwrap().review.is_legacy());
        assert_eq!(
            upgraded_models(&path),
            ["gpt-6.1-terra", "gpt-6.1-sol", "haiku"]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn legacy_upgrade_treats_an_opencode_profile_without_a_report_as_usage_priced() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("custom-config.toml");
        let reports: QuotaReports = Arc::new(Mutex::new(BTreeMap::from([
            (
                "codex".to_owned(),
                subscription_report("codex", HarnessKind::Codex),
            ),
            (
                "haiku".to_owned(),
                subscription_report("haiku", HarnessKind::Claude),
            ),
        ])));
        let catalog = billing_catalog(&path, reports);
        catalog.upgrade_legacy_review(1).await.unwrap();
        assert_eq!(
            upgraded_models(&path),
            ["gpt-6.1-terra", "gpt-6.1-sol", "haiku"]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn legacy_upgrade_does_not_persist_while_a_drafted_profile_has_no_billing() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("custom-config.toml");
        // The Claude profile's quota probe never reports.
        let reports: QuotaReports = Arc::new(Mutex::new(BTreeMap::from([(
            "codex".to_owned(),
            subscription_report("codex", HarnessKind::Codex),
        )])));
        let catalog = billing_catalog(&path, reports.clone());
        let error = catalog.upgrade_legacy_review(1).await.unwrap_err();
        assert!(
            error.to_string().contains("quota reports: haiku"),
            "{error:#}"
        );
        assert!(Config::load_from(&path).unwrap().review.is_legacy());
        // A report that arrives before the next attempt lets it finish.
        reports.lock().unwrap().insert(
            "haiku".into(),
            subscription_report("haiku", HarnessKind::Claude),
        );
        catalog.upgrade_legacy_review(2).await.unwrap();
        assert_eq!(
            upgraded_models(&path),
            ["gpt-6.1-terra", "gpt-6.1-sol", "haiku"]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn stuck_profile_times_out_without_hiding_a_ready_profile() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("custom-config.toml");
        legacy_review_file(&path);
        let probe: Arc<Probe> = Arc::new(|profile| {
            Box::pin(async move {
                if profile == "codex" {
                    return std::future::pending::<Result<ProfileConfig>>().await;
                }
                Ok(ProfileConfig {
                    model: Some("claude-haiku-4.5".into()),
                    models: vec![mj_core::acp::SessionConfigChoice {
                        value: "claude-haiku-4.5".into(),
                        name: "Haiku".into(),
                        description: None,
                    }],
                    efforts: Vec::new(),
                    observed_at: 1,
                })
            })
        });
        let mut catalog = ProfileCatalog::build(
            CancellationToken::new(),
            probe.clone(),
            Arc::new(move |profile, _| probe(profile)),
        );
        Arc::get_mut(&mut catalog).unwrap().config_path = Some(path.clone());
        let mut config = test_config(&[
            ("codex", HarnessKind::Codex),
            ("haiku", HarnessKind::Claude),
        ]);
        config.review = mj_core::config::ReviewConfig::legacy_default();
        catalog.sync(&config);
        let task = tokio::spawn(async move { catalog.upgrade_legacy_review(12).await });
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }
        tokio::time::advance(Duration::from_secs(31)).await;
        task.await.unwrap().unwrap_err();
        let review = Config::load_from(&path).unwrap().review;
        assert!(review.is_legacy());
    }

    #[tokio::test]
    async fn exhausted_legacy_discovery_keeps_retrying() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("custom-config.toml");
        legacy_review_file(&path);
        let catalog = legacy_upgrade_fixture(path.clone(), Arc::new(AtomicBool::new(true)));
        catalog.upgrade_legacy_review(12).await.unwrap_err();
        let review = Config::load_from(&path).unwrap().review;
        assert!(review.is_legacy());
    }

    #[tokio::test]
    async fn failed_legacy_discovery_does_not_finalize_empty_reviewers() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        legacy_review_file(&path);
        let probe: Arc<Probe> = Arc::new(|_| Box::pin(async { anyhow::bail!("unavailable") }));
        let mut catalog = ProfileCatalog::build(
            CancellationToken::new(),
            probe.clone(),
            Arc::new(move |profile, _| probe(profile)),
        );
        Arc::get_mut(&mut catalog).unwrap().config_path = Some(path.clone());
        catalog.sync(&test_config(&[("codex", HarnessKind::Codex)]));
        assert!(catalog.upgrade_legacy_review(12).await.is_err());
        assert!(Config::load_from(&path).unwrap().review.is_legacy());
    }

    #[test]
    fn legacy_auto_upgrade_persists_once_without_rewriting_other_sections() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        std::fs::write(&path, format!(
            "# keep this comment\nversion = {}\n[review]\ntrigger = \"turn\"\n[phone]\nenabled = false\n",
            mj_core::config::CONFIG_VERSION
        )).unwrap();
        let choice = mj_core::config::SubagentChoice {
            model: "claude-haiku-4.5".into(),
            effort: None,
        };
        assert!(
            ProfileCatalog::persist_legacy_review_to(
                &path,
                vec![choice.clone()],
                false,
                &[],
                &mj_core::config::LegacyReview::default()
            )
            .unwrap()
        );
        let saved = std::fs::read_to_string(&path).unwrap();
        assert!(saved.contains("# keep this comment"));
        assert!(saved.contains("[phone]\nenabled = false"));
        let loaded = Config::load_from(&path).unwrap();
        assert!(!loaded.review.is_legacy());
        assert_eq!(loaded.review.reviewers, vec![choice]);
        assert!(
            ProfileCatalog::persist_legacy_review_to(
                &path,
                Vec::new(),
                false,
                &[],
                &mj_core::config::LegacyReview::default()
            )
            .is_err()
        );
        assert_eq!(Config::load_from(&path).unwrap().review.reviewers.len(), 1);
    }

    #[test]
    fn legacy_upgrade_retries_when_profile_definitions_change_before_save() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        legacy_review_file(&path);
        let original = Config::load_from(&path).unwrap();
        let definitions = upgrade_definitions(&original);
        let mut changed = original.clone();
        changed
            .profiles
            .get_mut("codex")
            .unwrap()
            .home
            .push("new-home");
        changed.save_to(&path).unwrap();
        let error = ProfileCatalog::persist_legacy_review_to(
            &path,
            vec![mj_core::config::SubagentChoice {
                model: "haiku".into(),
                effort: None,
            }],
            false,
            &definitions,
            original.review.legacy.as_ref().unwrap(),
        )
        .unwrap_err();
        assert!(error.to_string().contains("profile definitions changed"));
        assert!(Config::load_from(&path).unwrap().review.is_legacy());
    }

    #[test]
    fn legacy_upgrade_retries_when_a_profile_models_list_changes_before_save() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        legacy_review_file(&path);
        let original = Config::load_from(&path).unwrap();
        let definitions = upgrade_definitions(&original);
        let mut changed = original.clone();
        changed.profiles.get_mut("codex").unwrap().models = vec!["gpt-6.1-sol".into()];
        changed.save_to(&path).unwrap();
        let error = ProfileCatalog::persist_legacy_review_to(
            &path,
            vec![mj_core::config::SubagentChoice {
                model: "gpt-6.1-terra".into(),
                effort: None,
            }],
            false,
            &definitions,
            original.review.legacy.as_ref().unwrap(),
        )
        .unwrap_err();
        assert!(error.to_string().contains("profile definitions changed"));
        assert!(Config::load_from(&path).unwrap().review.is_legacy());
    }

    #[test]
    fn complete_legacy_auto_without_supported_models_turns_review_off() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        legacy_review_file(&path);
        let config = Config::load_from(&path).unwrap();
        let definitions = upgrade_definitions(&config);
        assert!(
            ProfileCatalog::persist_legacy_review_to(
                &path,
                Vec::new(),
                true,
                &definitions,
                config.review.legacy.as_ref().unwrap(),
            )
            .unwrap()
        );
        let upgraded = Config::load_from(&path).unwrap();
        assert!(!upgraded.review.is_legacy());
        assert_eq!(upgraded.review.trigger, mj_core::config::ReviewTrigger::Off);
        assert!(upgraded.review.reviewers.is_empty());
    }

    #[test]
    fn missing_review_table_upgrades_and_unrelated_save_keeps_auto() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        std::fs::write(
            &path,
            format!("version = {}\n# keep\n", mj_core::config::CONFIG_VERSION),
        )
        .unwrap();
        let config = Config::load_from(&path).unwrap();
        assert!(config.review.is_legacy());
        config.save_to(&path).unwrap();
        assert!(Config::load_from(&path).unwrap().review.is_legacy());
        assert!(
            !std::fs::read_to_string(&path)
                .unwrap()
                .contains("reviewers")
        );
        assert!(
            ProfileCatalog::persist_legacy_review_to(
                &path,
                Vec::new(),
                false,
                &[],
                &mj_core::config::LegacyReview::default()
            )
            .is_err()
        );
        assert!(Config::load_from(&path).unwrap().review.is_legacy());
        assert!(
            !std::fs::read_to_string(&path)
                .unwrap()
                .contains("reviewers = []")
        );
    }

    /// The `models` allow-list reaches every reader through the two exits of
    /// the catalogue, and an edit applies without a new probe.
    #[tokio::test]
    async fn a_models_allow_list_filters_the_snapshot_and_reads_until_the_config_changes() {
        let probes = calls();
        let counted = probes.clone();
        let catalog = ProfileCatalog::with_probe(Arc::new(move |profile| {
            counted.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move {
                let mut choices = test_choices(&profile);
                choices
                    .models
                    .extend((1..4).map(|index| mj_core::acp::SessionConfigChoice {
                        value: format!("m{index}"),
                        name: format!("m{index}"),
                        description: None,
                    }));
                Ok(choices)
            })
        }));
        let values = |choices: &ProfileConfig| {
            choices
                .models
                .iter()
                .map(|choice| choice.value.clone())
                .collect::<Vec<_>>()
        };
        let mut config = test_config(&[("p", HarnessKind::Codex)]);
        let key = config.profiles["p"].capabilities_key("p");
        let snapshot_models =
            |catalog: &Arc<ProfileCatalog>| match &catalog.snapshot().profiles[&key].choices {
                CapabilityState::Ready(choices) => values(choices),
                other => panic!("not ready: {other:?}"),
            };
        catalog.sync_now(&config).await;
        assert_eq!(snapshot_models(&catalog), ["p-model", "m1", "m2", "m3"]);

        // The current model stays offered although the list omits it.
        config.profiles.get_mut("p").unwrap().models = vec!["m2".into(), "unadvertised".into()];
        assert_eq!(
            config.profiles["p"].capabilities_key("p"),
            key,
            "the allow-list is not part of the cache key"
        );
        let probed = probes.load(Ordering::SeqCst);
        catalog.sync_now(&config).await;
        assert_eq!(snapshot_models(&catalog), ["p-model", "m2"]);
        let read = catalog.capabilities(&["p".into()]).await.unwrap();
        assert_eq!(values(&read[0]), ["p-model", "m2"]);
        let model_read = catalog
            .model_capabilities("p".into(), "m2".into())
            .await
            .unwrap();
        assert_eq!(values(&model_read), ["p-model", "m2"]);
        let unrestricted = catalog.unrestricted_capabilities("p").await.unwrap();
        assert_eq!(values(&unrestricted), ["p-model", "m1", "m2", "m3"]);
        assert_eq!(catalog.allowed_models("p"), ["m2", "unadvertised"]);
        assert_eq!(
            probes.load(Ordering::SeqCst),
            probed,
            "editing the allow-list does not rediscover"
        );

        config.profiles.get_mut("p").unwrap().models.clear();
        catalog.sync_now(&config).await;
        assert_eq!(snapshot_models(&catalog), ["p-model", "m1", "m2", "m3"]);
    }
    // Hard-won: a2afafcb3: a model probe exempted its requested hidden model.
    #[tokio::test]
    async fn a_model_probe_cannot_exempt_a_hidden_requested_model() {
        let probe: Arc<Probe> = Arc::new(|id| {
            Box::pin(async move {
                let mut choices = test_choices(&id);
                choices.models.push(mj_core::acp::SessionConfigChoice {
                    value: "hidden".into(),
                    name: "hidden".into(),
                    description: None,
                });
                Ok(choices)
            })
        });
        let default_probe = probe.clone();
        let catalog = ProfileCatalog::build(
            CancellationToken::new(),
            default_probe,
            Arc::new(move |id, model| {
                let probe = probe.clone();
                Box::pin(async move {
                    let mut choices = probe(id).await?;
                    choices.model = Some(model);
                    Ok(choices)
                })
            }),
        );
        let mut config = test_config(&[("p", HarnessKind::Codex)]);
        config.profiles.get_mut("p").unwrap().models = vec!["allowed".into()];
        catalog.sync_now(&config).await;
        let choices = catalog
            .model_capabilities("p".into(), "hidden".into())
            .await
            .unwrap();
        assert!(crate::server::api::validate_selectors(&choices, Some("hidden"), None).is_err());
        assert!(
            choices
                .models
                .iter()
                .any(|choice| choice.value == "p-model")
        );
    }

    // Hard-won: a2afafcb3: a pending read applied its starting allowlist.
    #[tokio::test]
    async fn a_pending_discovery_uses_the_current_models_setting() {
        let (started_tx, mut started) = tokio::sync::mpsc::unbounded_channel();
        let (release, gate) = tokio::sync::watch::channel(false);
        let catalog = ProfileCatalog::with_probe(Arc::new(move |id| {
            let mut gate = gate.clone();
            let started = started_tx.clone();
            Box::pin(async move {
                started.send(()).unwrap();
                if !*gate.borrow() {
                    gate.changed().await.unwrap();
                }
                let mut choices = test_choices(&id);
                choices.models.extend(["old", "new"].map(|value| {
                    mj_core::acp::SessionConfigChoice {
                        value: value.into(),
                        name: value.into(),
                        description: None,
                    }
                }));
                Ok(choices)
            })
        }));
        let mut config = test_config(&[("p", HarnessKind::Codex)]);
        config.profiles.get_mut("p").unwrap().models = vec!["old".into()];
        catalog.sync(&config);
        started.recv().await.unwrap();
        let reader = {
            let catalog = catalog.clone();
            let config = config.clone();
            tokio::spawn(async move { catalog.fetch(&config, "p", None).await })
        };
        tokio::task::yield_now().await;
        config.profiles.get_mut("p").unwrap().models = vec!["new".into()];
        catalog.sync(&config);
        release.send(true).unwrap();
        let choices = reader.await.unwrap().unwrap();
        assert_eq!(
            choices
                .models
                .iter()
                .map(|choice| choice.value.as_str())
                .collect::<Vec<_>>(),
            ["p-model", "new"]
        );
    }
}
