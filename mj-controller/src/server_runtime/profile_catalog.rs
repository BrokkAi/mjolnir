//! One daemon-owned cache, warmed independently of settings and tool calls.
//! Entries belong to configured profile identities, never a shared generation.
//! Every reader joins the owner's attempt; only its supervisor retries failures.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::{
    Arc, Mutex, MutexGuard, PoisonError,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, SystemTime};

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
use sha2::{Digest, Sha256};

use crate::quota::ProfileQuota;
use tokio_util::sync::CancellationToken;

#[cfg(test)]
pub(crate) type Probe = dyn Fn(String) -> BoxFuture<'static, Result<ProfileConfig>> + Send + Sync;
#[cfg(test)]
type ModelProbe = dyn Fn(String, String) -> BoxFuture<'static, Result<ProfileConfig>> + Send + Sync;
type Attempt = Shared<BoxFuture<'static, Result<ProbeOutcome, AttemptError>>>;
type CacheKey = (String, Option<String>);
type NoticeCallback = Arc<dyn Fn(String) + Send + Sync>;
pub(crate) type QuotaReports = Arc<Mutex<BTreeMap<String, ProfileQuota>>>;

/// How long the legacy upgrade waits for every offering profile's first
/// quota report before treating profiles without one as subscriptions.
const QUOTA_REPORT_WAIT: Duration = Duration::from_secs(30);

/// How often ready entries are checked against the choice lifetime. An
/// entry is probed again at most this long after it expires.
const REFRESH_CHECK_INTERVAL: Duration = Duration::from_secs(5 * 60);

/// How often the owner checks every distinct profile home for catalog-input
/// edits. A pass stats the three inputs per home and reads a file only when
/// its stat changed or the file may have been written inside one mtime tick
/// of the last pass, so the steady state does no file I/O beyond the stats.
const HOME_WATCH_INTERVAL: Duration = Duration::from_secs(30);

/// How long after a failed re-probe the home watch tries the same edit again.
/// Matches the expiry refresh cadence: a failure that persists, such as a
/// missing login, is retried rarely rather than every pass.
const HOME_EDIT_RETRY_WAIT: Duration = Duration::from_secs(5 * 60);

/// A file written more recently than this is re-hashed on every pass, even
/// when its stat matches the stored one: filesystems with one-second mtime
/// granularity can hide a same-length rewrite inside an unchanged (mtime,
/// size) pair, and the recency read catches it on the next pass.
const RECENT_EDIT_WINDOW: Duration = Duration::from_secs(2 * 30);

/// The files in a profile home that decide what a session advertises: the
/// harness's own configuration, which names the model provider and the
/// default model, and the model catalog Mjolnir merges over a provider's
/// list. Staging copies `home/<name>` verbatim, so the owner watches exactly
/// what a probe will stage. Reads never touch these from a read path; the
/// home watch and the probes read them, from background tasks.
const HOME_CATALOG_INPUTS: [&str; 3] = ["config.toml", "models.json", "settings.json"];

/// One watched input's cheap identity. `None` is an absent file.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct FileStat {
    mtime: Option<SystemTime>,
    size: u64,
}

/// One catalog input's observed state: its stat and the hash of its
/// contents, taken when the stat last changed or the file may have been
/// rewritten inside one mtime tick. Both are `None` for an absent file.
#[derive(Clone, PartialEq, Debug)]
struct WatchedInput {
    stat: Option<FileStat>,
    hash: Option<[u8; 32]>,
}

/// The observed catalog-input state of one home. The digest compares content,
/// not metadata: rewriting a file with identical bytes only refreshes the
/// stat, so it never registers as an edit.
#[derive(Clone)]
struct HomeFiles {
    inputs: [WatchedInput; 3],
    digest: [u8; 32],
}

/// What one probe answers, and the catalog-input digest of the profile home
/// as the probe started — what its staged copy was cut from. Recorded when
/// the probe succeeds, so the home watch compares what the catalog serves
/// against what it was built from, never against when the probe finished.
#[derive(Clone)]
struct ProbeOutcome {
    choices: ProfileConfig,
    home_digest: Option<[u8; 32]>,
}

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
    /// The catalog-input state of each watched home, a pure polling
    /// optimization: content hashes are reused while a file's stat holds,
    /// so a pass that no edit touched reads no file.
    home_stats: BTreeMap<PathBuf, HomeFiles>,
    /// The catalog-input digest each served entry was probed from, recorded
    /// when that entry's probe succeeds. The home watch compares every
    /// served entry of an identity against the current files, so one entry's
    /// failed re-probe cannot hide behind a sibling's success: the failed
    /// entry keeps its older digest and stays stale.
    home_settled: BTreeMap<CacheKey, [u8; 32]>,
    /// When the watch may launch the next re-probe for an identity whose
    /// edit re-probe failed. Bounded like the expiry refresh: a persistent
    /// failure is retried rarely, not every pass.
    home_edit_retry_after: BTreeMap<String, tokio::time::Instant>,
}

impl Inner {
    /// Whether every non-Failed entry of `identity` carries a settled
    /// digest, all identical: the identity's served answers all describe
    /// one home state, so nothing is left retrying. A still-failing entry
    /// keeps its older digest and leaves the identity pending.
    fn converged_at_one_digest(&self, identity: &str) -> bool {
        let mut digests = self
            .entries
            .iter()
            .filter(|((entry_identity, _), entry)| {
                entry_identity == identity && !matches!(entry, Entry::Failed { .. })
            })
            .map(|(key, _)| self.home_settled.get(key));
        match digests.next() {
            Some(first) => first.is_some_and(|first| digests.all(|digest| digest == Some(first))),
            None => true,
        }
    }

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
    /// How long the home watch waits before re-probing an identity whose
    /// edit re-probe failed. Tests shorten this to observe the retry.
    #[cfg(test)]
    home_edit_retry_wait: Duration,
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
            #[cfg(test)]
            home_edit_retry_wait: HOME_EDIT_RETRY_WAIT,
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
            self.start_watching_homes();
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
            // With no reviewers to choose, clearing the pending request is
            // what turns review off.
            config.review.reviewers = reviewers;
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
                            let digest = inner.home_settled.get(&(identity.clone(), None)).copied();
                            started.extend(self.claim_models(
                                &mut inner,
                                &definition,
                                &identity,
                                &choices,
                                digest,
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
                    let digest = inner.home_settled.get(&key).copied();
                    started.extend(self.claim_models(
                        &mut inner,
                        &definition,
                        &identity,
                        &choices,
                        digest,
                    ));
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
    /// `home_digest` is what the probe that produced `choices` staged from;
    /// the copied current-model entry serves exactly that, so it settles at
    /// the same digest.
    fn claim_models(
        &self,
        inner: &mut Inner,
        definition: &Definition,
        identity: &str,
        choices: &ProfileConfig,
        home_digest: Option<[u8; 32]>,
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
                    .insert(model_key.clone(), Entry::Ready(choices.clone()));
                if let Some(digest) = home_digest {
                    inner.home_settled.insert(model_key, digest);
                }
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
                catalog
                    .refresh(|_, choices| !crate::controller::profile_config::is_current(choices));
            }
        });
    }

    /// Watch every distinct profile home for catalog-input edits until
    /// shutdown. The watch is the only code that stats or reads home files,
    /// and it runs here, never on a read path.
    fn start_watching_homes(self: &Arc<Self>) {
        let catalog = Arc::downgrade(self);
        let cancellation = self.cancellation.clone();
        tokio::spawn(async move {
            // Startup discovery already reads the homes; the first pass
            // only baselines them.
            let mut tick = tokio::time::interval_at(
                tokio::time::Instant::now() + HOME_WATCH_INTERVAL,
                HOME_WATCH_INTERVAL,
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
                catalog.watch_homes();
            }
        });
    }

    /// One pass over every distinct profile home. For each profile, compare
    /// the digest its served answer was probed from — recorded when its last
    /// probe succeeded — against the digest of the home's files now. A
    /// mismatch means the home was edited after that probe read it (or no
    /// probe has succeeded yet), so re-probe exactly that profile through
    /// the keep-serving-the-old-answer refresh path. An edit therefore
    /// stays pending until a probe that read the edited home published:
    /// one that read it earlier records the old digest, and one that fails
    /// records nothing.
    fn watch_homes(self: &Arc<Self>) {
        let (identities_by_home, stats, settled, served) = {
            let inner = self.lock();
            let mut identities_by_home: BTreeMap<PathBuf, BTreeSet<String>> = BTreeMap::new();
            for (identity, definition) in &inner.definitions {
                if definition.cancellation.is_cancelled() {
                    continue;
                }
                let Some(profile) = definition.config.enabled_profile(&definition.id) else {
                    continue;
                };
                let home = mj_core::path_input::expand_local(&profile.home)
                    .unwrap_or_else(|_| profile.home.clone());
                identities_by_home
                    .entry(home)
                    .or_default()
                    .insert(identity.clone());
            }
            (
                identities_by_home,
                inner.home_stats.clone(),
                inner.home_settled.clone(),
                inner
                    .entries
                    .iter()
                    .filter(|(_, entry)| !matches!(entry, Entry::Failed { .. }))
                    .map(|(key, _)| key.clone())
                    .collect::<BTreeSet<_>>(),
            )
        };
        let mut polled = BTreeMap::new();
        for home in identities_by_home.keys() {
            polled.insert(home.clone(), poll_home_files(home, stats.get(home)));
        }
        let mut stale = BTreeSet::new();
        let mut converged = BTreeSet::new();
        let mut updates: Vec<(PathBuf, HomeFiles)> = Vec::new();
        for (home, current) in polled {
            let Some(mut current) = current else {
                // A hard read error keeps the previous stats, so the next
                // pass compares against the same knowledge.
                continue;
            };
            // A probe reads the whole home; the poll may reuse a hash taken
            // before a timestamp-preserving rewrite (cp -p) hid an edit from
            // the stat. When the poll saw no change yet a settled digest
            // disagrees, the poll is behind: re-hash in full so the identity
            // converges instead of churning a re-probe every pass.
            let poll_blind = stats
                .get(&home)
                .is_some_and(|previous| previous.digest == current.digest)
                && identities_by_home[&home].iter().any(|identity| {
                    served.iter().any(|key| {
                        key.0 == *identity
                            && settled
                                .get(key)
                                .is_some_and(|digest| *digest != current.digest)
                    })
                });
            if poll_blind && let Some(fresh) = hash_home_inputs(&home) {
                current = HomeFiles {
                    digest: home_digest(&fresh),
                    inputs: fresh,
                };
            }
            // The stat cache is only a polling optimization; store whatever
            // the pass observed.
            updates.push((home.clone(), current.clone()));
            // Every served answer must have been read from the home as it
            // now is. A Failed entry serves no home-derived answer — its own
            // supervisor retries it with backoff, and its success records a
            // digest that joins the check — so it neither settles nor blocks
            // convergence.
            for identity in &identities_by_home[&home] {
                let every_entry_settled = served
                    .iter()
                    .filter(|key| key.0 == *identity)
                    .all(|key| settled.get(key) == Some(&current.digest));
                if every_entry_settled {
                    converged.insert(identity.clone());
                } else {
                    stale.insert(identity.clone());
                }
            }
        }
        // A failed re-probe records nothing, so the identity stays stale and
        // the next pass tries again; bound how often the re-probe repeats,
        // matching the expiry refresh cadence for persistent failures.
        // Launches and failed re-probes arm the window; nothing else touches
        // it, so concurrent probes cannot disagree about it. An identity
        // observed converged this pass has nothing left to retry: clear its
        // window before deciding what launches, so a genuinely new edit
        // re-probes on the next pass instead of waiting the window out.
        let watched: BTreeSet<&str> = identities_by_home
            .values()
            .flat_map(|identities| identities.iter().map(String::as_str))
            .collect();
        let now = tokio::time::Instant::now();
        let retry_wait = self.home_edit_retry_wait();
        let due: Vec<String> = {
            let mut inner = self.lock();
            inner.home_edit_retry_after.retain(|identity, _| {
                watched.contains(identity.as_str()) && !converged.contains(identity)
            });
            stale
                .iter()
                .filter(|identity| {
                    !matches!(
                        inner.home_edit_retry_after.get(*identity),
                        Some(after) if *after > now
                    )
                })
                .cloned()
                .collect()
        };
        let launched = if due.is_empty() {
            Vec::new()
        } else {
            let due: BTreeSet<&str> = due.iter().map(String::as_str).collect();
            self.refresh(|key, _| due.contains(key.0.as_str()))
        };
        let launched_identities: BTreeSet<String> = launched.into_iter().map(|key| key.0).collect();
        let mut inner = self.lock();
        for (home, files) in updates {
            inner.home_stats.insert(home, files);
        }
        // Homes no profile uses anymore stop being watched, and identities
        // no definition serves stop being settled or throttled.
        inner
            .home_stats
            .retain(|home, _| identities_by_home.contains_key(home));
        inner
            .home_settled
            .retain(|key, _| watched.contains(key.0.as_str()));
        for identity in launched_identities {
            inner
                .home_edit_retry_after
                .insert(identity, now + retry_wait);
        }
    }

    /// How long after a failed edit re-probe the home watch tries again.
    fn home_edit_retry_wait(&self) -> Duration {
        #[cfg(test)]
        {
            self.home_edit_retry_wait
        }
        #[cfg(not(test))]
        {
            HOME_EDIT_RETRY_WAIT
        }
    }

    /// Probe again every ready entry that `expired` selects. Readers keep
    /// the entry until a probe succeeds; a failed probe leaves it in place
    /// and records nothing. Returns the keys whose probes launched.
    fn refresh(
        self: &Arc<Self>,
        expired: impl Fn(&CacheKey, &ProfileConfig) -> bool,
    ) -> Vec<CacheKey> {
        let mut started = Vec::new();
        {
            let mut inner = self.lock();
            let keys = inner
                .entries
                .iter()
                .filter(|(key, entry)| {
                    matches!(entry, Entry::Ready(choices) if expired(key, choices))
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
                let attempt = self.attempt_refresh(&definition, key.1.clone());
                inner.refreshing.insert(key.clone(), attempt.clone());
                started.push((key, attempt, definition.cancellation));
            }
        }
        let launched = started
            .iter()
            .map(|(key, _, _)| key.clone())
            .collect::<Vec<_>>();
        for (key, attempt, cancellation) in started {
            tokio::spawn(self.clone().drive_refresh(key, attempt, cancellation));
        }
        launched
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
            let Some(Ok(outcome)) = result else {
                // The re-probe failed: the entry keeps its pre-edit copy and
                // the identity stays stale. Re-arm the retry window so the
                // next pass waits it out instead of re-probing the identity
                // every poll. The window is cleared by the home watch when
                // it observes every served entry settled, never by a probe.
                inner.home_edit_retry_after.insert(
                    key.0.clone(),
                    tokio::time::Instant::now() + self.home_edit_retry_wait(),
                );
                return;
            };
            let Some(Entry::Ready(previous)) = inner.entries.get(&key).cloned() else {
                return;
            };
            let digest = outcome.home_digest;
            if let Some(digest) = digest {
                inner.home_settled.insert(key.clone(), digest);
            }
            // Discharge the failed-reprobe window only when this success
            // leaves the identity at one digest: a still-failing sibling
            // keeps its older digest and the window's bound, whatever order
            // the probes complete in. The home watch also clears the window
            // for identities it observes converged.
            if inner.converged_at_one_digest(&key.0) {
                inner.home_edit_retry_after.remove(&key.0);
            }
            let choices = outcome.choices;
            inner
                .entries
                .insert(key.clone(), Entry::Ready(choices.clone()));
            let mut started = Vec::new();
            if key.1.is_none() {
                let definition = inner.definitions[&key.0].clone();
                started = self.claim_models(&mut inner, &definition, &key.0, &choices, digest);
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
        self.build_attempt(definition, model, false)
    }

    /// A re-probe the owner launches after detecting a change the cache
    /// cannot see, such as an edited home: the harness must answer from the
    /// current files, so the process-global cache is bypassed. Readers that
    /// join a pending first probe keep the plain attempt above.
    fn attempt_refresh(&self, definition: &Definition, model: Option<String>) -> Attempt {
        self.build_attempt(definition, model, true)
    }

    fn build_attempt(
        &self,
        definition: &Definition,
        model: Option<String>,
        refresh: bool,
    ) -> Attempt {
        let probe: BoxFuture<'static, Result<crate::controller::profile_config::Discovery>> = {
            #[cfg(test)]
            if let Some((probe, model_probe)) = &self.probe {
                match model {
                    None => probe(definition.id.clone()),
                    Some(model) => model_probe(definition.id.clone(), model),
                }
                .map_ok(|choices| crate::controller::profile_config::Discovery {
                    choices,
                    probed: true,
                })
                .boxed()
            } else {
                crate::controller::profile_config::discover_for(
                    (*definition.config).clone(),
                    definition.id.clone(),
                    model,
                    definition.cancellation.clone(),
                    refresh,
                )
                .boxed()
            }
            #[cfg(not(test))]
            {
                crate::controller::profile_config::discover_for(
                    (*definition.config).clone(),
                    definition.id.clone(),
                    model,
                    definition.cancellation.clone(),
                    refresh,
                )
                .boxed()
            }
        };
        let home = definition_home(definition);
        async move {
            // Read the catalog inputs before the probe stages them. The home
            // watch later compares what a probe read, not when it finished,
            // so an edit that lands mid-probe is re-detected, and a failed
            // probe records nothing and stays pending.
            let digest_at_start = home
                .as_deref()
                .and_then(hash_home_inputs)
                .map(|inputs| home_digest(&inputs));
            let discovery = probe.await?;
            // A cache-served answer predates the home edit the digest just
            // captured: it settles nothing, so the watch keeps the entry
            // pending for a real re-probe instead of masking the edit.
            let home_digest = if discovery.probed {
                digest_at_start
            } else {
                None
            };
            Ok(ProbeOutcome {
                choices: discovery.choices,
                home_digest,
            })
        }
        .map_err(|error: anyhow::Error| AttemptError::new(&error))
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
                        Ok(outcome) => {
                            if let Some(digest) = outcome.home_digest {
                                inner.home_settled.insert(key.clone(), digest);
                            }
                            // See drive_refresh: discharge the window only at
                            // one digest, so a failing sibling keeps the bound.
                            if inner.converged_at_one_digest(&key.0) {
                                inner.home_edit_retry_after.remove(&key.0);
                            }
                            inner
                                .entries
                                .insert(key.clone(), Entry::Ready(outcome.choices.clone()));
                            if key.1.is_none() {
                                let definition = inner.definitions[&key.0].clone();
                                started.extend(self.claim_models(
                                    &mut inner,
                                    &definition,
                                    &key.0,
                                    &outcome.choices,
                                    outcome.home_digest,
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
        // read uses the default entry it already depended on. Reading it must
        // not start another live probe and replace concurrent readers.
        let default = if specific {
            let cached = self.lock().entries.get(&(identity.clone(), None)).cloned();
            match cached {
                Some(Entry::Ready(choices)) => choices.model,
                Some(Entry::Pending(attempt)) => {
                    attempt
                        .await
                        .map_err(|error| anyhow::anyhow!("{}", error.message))?
                        .choices
                        .model
                }
                Some(Entry::Failed { error, .. }) => bail!("{error}"),
                None => self.fetch_unrestricted(config, id, None).await?.model,
            }
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
        // A live read has its own fresh-probe admission in fetch_live. Re-
        // ensuring an adopted definition would retire that owner and discard
        // the default entry that model-specific reads depend on.
        let identity = profile.capabilities_key(id);
        if !live || !self.lock().definitions.contains_key(&identity) {
            self.ensure(config);
        }
        let key = (identity, model);
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
            .map(|outcome| outcome.choices)
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

/// The expanded home directory a definition's probe stages from, when the
/// profile is still enabled in the definition's configuration.
fn definition_home(definition: &Definition) -> Option<PathBuf> {
    let profile = definition.config.enabled_profile(&definition.id)?;
    Some(mj_core::path_input::expand_local(&profile.home).unwrap_or_else(|_| profile.home.clone()))
}

/// The catalog-input state of `home`, read in full. `poll_home_files` calls
/// this only for inputs whose stat changed or may have been rewritten inside
/// one mtime tick; probes call it at launch to record what they staged from.
fn hash_home_inputs(home: &Path) -> Option<[WatchedInput; 3]> {
    let mut inputs = Vec::with_capacity(HOME_CATALOG_INPUTS.len());
    for name in HOME_CATALOG_INPUTS {
        let path = home.join(name);
        let stat = match std::fs::metadata(&path) {
            Ok(metadata) => Some(FileStat {
                mtime: metadata.modified().ok(),
                size: metadata.len(),
            }),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(_) => return None,
        };
        inputs.push(WatchedInput {
            hash: hash_home_input(&path, stat.is_some()),
            stat,
        });
    }
    inputs.try_into().ok()
}

/// The current catalog-input state of `home`. `previous` is the state the
/// last watch pass saw: an input whose stat is unchanged keeps its content
/// hash, so a pass that no edit touched reads no file. An input whose mtime
/// is recent is re-hashed even when its stat matches: filesystems with
/// one-second mtime granularity can hide a same-length rewrite inside an
/// unchanged (mtime, size) pair. A hard I/O error (anything but a missing
/// input) returns `None`; the home keeps its previous state and is retried
/// on the next pass rather than being treated as edited.
fn poll_home_files(home: &Path, previous: Option<&HomeFiles>) -> Option<HomeFiles> {
    let mut inputs = Vec::with_capacity(HOME_CATALOG_INPUTS.len());
    let mut content_changed = previous.is_none();
    for (index, name) in HOME_CATALOG_INPUTS.iter().enumerate() {
        let path = home.join(name);
        let stat = match std::fs::metadata(&path) {
            Ok(metadata) => Some(FileStat {
                mtime: metadata.modified().ok(),
                size: metadata.len(),
            }),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => {
                tracing::debug!(home = %home.display(), input = %name, %error, "home catalog watch skipped this pass");
                return None;
            }
        };
        let previous_input = previous.map(|files| &files.inputs[index]);
        let recently_written = stat.is_some_and(|stat| {
            stat.mtime.is_some_and(|mtime| {
                SystemTime::now()
                    .duration_since(mtime)
                    .is_ok_and(|age| age < RECENT_EDIT_WINDOW)
            })
        });
        let input = if !recently_written && previous_input.is_some_and(|input| input.stat == stat) {
            previous_input.expect("checked above").clone()
        } else {
            WatchedInput {
                hash: hash_home_input(&path, stat.is_some()),
                stat,
            }
        };
        content_changed |= previous_input.map(|input| input.hash) != Some(input.hash);
        inputs.push(input);
    }
    let inputs: [WatchedInput; 3] = inputs.try_into().ok()?;
    let digest = if content_changed {
        home_digest(&inputs)
    } else {
        previous?.digest
    };
    Some(HomeFiles { inputs, digest })
}

/// The content hash of one catalog input; `None` when the input is absent.
/// A file that cannot be read races the directory scan and is observed as
/// absent for this pass; a spurious re-probe from that is safe, and the next
/// pass sees the file again.
fn hash_home_input(path: &Path, present: bool) -> Option<[u8; 32]> {
    if !present {
        return None;
    }
    let bytes = std::fs::read(path).ok()?;
    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    Some(hasher.finalize().into())
}

/// The identity of a home's catalog inputs: every input's name and either an
/// absent marker or its content hash.
fn home_digest(inputs: &[WatchedInput; 3]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    for (name, input) in HOME_CATALOG_INPUTS.iter().zip(inputs.iter()) {
        hasher.update(name.as_bytes());
        match input.hash {
            Some(hash) => {
                hasher.update([1]);
                hasher.update(hash);
            }
            None => hasher.update([0]),
        }
    }
    hasher.finalize().into()
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
                        ..mj_core::config::HarnessProfile::new(
                            *kind,
                            std::path::PathBuf::from("/home/agent").join(id),
                        )
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

    /// A configuration whose profile homes are real directories under
    /// `root`, so the home watch has files to observe, together with the
    /// id-to-home map the fixture probe answers from.
    fn homes_config(
        root: &Path,
        profiles: &[(&str, HarnessKind)],
    ) -> (Config, BTreeMap<String, PathBuf>) {
        let mut config = Config::default();
        let mut homes = BTreeMap::new();
        for (id, kind) in profiles {
            let home = root.join(id);
            homes.insert((*id).to_owned(), home.clone());
            config.profiles.insert(
                (*id).to_owned(),
                HarnessProfile {
                    ..mj_core::config::HarnessProfile::new(*kind, home)
                },
            );
        }
        (config, homes)
    }

    fn write_home_model(root: &Path, profile: &str, model: &str) {
        std::fs::create_dir_all(root.join(profile)).unwrap();
        std::fs::write(
            root.join(profile).join("config.toml"),
            format!("model = \"{model}\"\n"),
        )
        .unwrap();
    }

    /// The choices the profile home's `config.toml` advertises: the `model =`
    /// line, read the way the real probe reads the staged copy.
    fn read_home_choices(
        homes: &BTreeMap<String, PathBuf>,
        profile: &str,
    ) -> Result<ProfileConfig> {
        let home = homes.get(profile).expect("fixture home");
        let text = std::fs::read_to_string(home.join("config.toml"))
            .with_context(|| format!("read the {profile} home config.toml"))?;
        let model = text
            .trim()
            .strip_prefix("model = \"")
            .and_then(|rest| rest.strip_suffix('"'))
            .context("the home config.toml names a model")?
            .to_owned();
        let mut choices = test_choices(profile);
        choices.model = Some(model.clone());
        choices.models = vec![mj_core::acp::SessionConfigChoice {
            value: model.clone(),
            name: model,
            description: None,
        }];
        Ok(choices)
    }

    /// A probe for model-specific entries that fails while the flag is set,
    /// so a test can make one entry's re-probe fail while its sibling's
    /// succeeds.
    fn flaky_model_probe(
        homes: BTreeMap<String, PathBuf>,
        calls: Arc<AtomicUsize>,
        failing: Arc<AtomicBool>,
    ) -> Arc<ModelProbe> {
        Arc::new(move |profile: String, _model: String| {
            calls.fetch_add(1, Ordering::SeqCst);
            let homes = homes.clone();
            let failing = failing.clone();
            Box::pin(async move {
                if failing.load(Ordering::SeqCst) {
                    anyhow::bail!("model report failed")
                }
                read_home_choices(&homes, &profile)
            })
        })
    }

    /// A probe that answers from the profile home's `config.toml`.
    fn home_model_probe(homes: BTreeMap<String, PathBuf>, calls: Arc<AtomicUsize>) -> Arc<Probe> {
        Arc::new(move |profile: String| {
            calls.fetch_add(1, Ordering::SeqCst);
            let homes = homes.clone();
            Box::pin(async move { read_home_choices(&homes, &profile) })
        })
    }

    /// A home-reading probe that fails while the flag is set, so a test can
    /// drive a failed edit re-probe and its retry.
    fn flaky_home_model_probe(
        homes: BTreeMap<String, PathBuf>,
        calls: Arc<AtomicUsize>,
        failing: Arc<AtomicBool>,
    ) -> Arc<Probe> {
        Arc::new(move |profile: String| {
            calls.fetch_add(1, Ordering::SeqCst);
            let homes = homes.clone();
            let failing = failing.clone();
            Box::pin(async move {
                if failing.load(Ordering::SeqCst) {
                    anyhow::bail!("provider is offline")
                }
                read_home_choices(&homes, &profile)
            })
        })
    }

    /// A home-reading probe that can be held after it has read the home, so
    /// a test keeps a re-probe in flight — as an expiry refresh can be when
    /// a home edit lands — and then releases it. The call count increments
    /// after the read, so reaching a count proves what the in-flight probes
    /// answered from.
    fn held_home_model_probe(
        homes: BTreeMap<String, PathBuf>,
        calls: Arc<AtomicUsize>,
        gate: tokio::sync::watch::Receiver<bool>,
    ) -> Arc<Probe> {
        Arc::new(move |profile: String| {
            let homes = homes.clone();
            let mut gate = gate.clone();
            let calls = calls.clone();
            Box::pin(async move {
                let choices = read_home_choices(&homes, &profile)?;
                calls.fetch_add(1, Ordering::SeqCst);
                if !*gate.borrow_and_update() {
                    let _ = gate.changed().await;
                }
                Ok(choices)
            })
        })
    }

    async fn current_model(catalog: &Arc<ProfileCatalog>, profile: &str) -> Option<String> {
        catalog
            .capabilities(&[profile.to_owned()])
            .await
            .ok()
            .and_then(|mut choices| choices.pop())
            .and_then(|choices| choices.model)
    }

    /// Wait until the catalog serves `model` as the profile's default, so
    /// the spawned refresh supervisors have run.
    async fn converge_on(catalog: &Arc<ProfileCatalog>, profile: &str, model: &str) {
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if current_model(catalog, profile).await.as_deref() == Some(model) {
                    return;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("{profile} did not converge on {model}"));
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
        catalog.refresh(|_, _| false);
        assert_eq!(calls.load(Ordering::SeqCst), 1, "current entries stay");

        failing.store(true, Ordering::SeqCst);
        catalog.refresh(|_, _| true);
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
        catalog.refresh(|_, _| true);
        while !catalog.lock().refreshing.is_empty() {
            tokio::task::yield_now().await;
        }
        assert_eq!(calls.load(Ordering::SeqCst), 5);
        assert!(catalog.capabilities(&["parent".into()]).await.is_ok());
    }

    #[test]
    fn home_watch_digest_tracks_content_not_metadata() {
        let directory = tempfile::tempdir().unwrap();
        let home = directory.path();
        let absent = poll_home_files(home, None).unwrap();
        let unchanged = poll_home_files(home, Some(&absent)).unwrap();
        assert_eq!(absent.digest, unchanged.digest, "no edit, no change");

        std::fs::write(home.join("config.toml"), "model = \"one\"\n").unwrap();
        let written = poll_home_files(home, Some(&absent)).unwrap();
        assert_ne!(
            absent.digest, written.digest,
            "a new catalog input is an edit"
        );

        std::fs::write(home.join("config.toml"), "model = \"one\"\n").unwrap();
        assert_eq!(
            written.digest,
            poll_home_files(home, Some(&written)).unwrap().digest,
            "rewriting identical content is metadata, not an edit"
        );

        std::fs::write(home.join("config.toml"), "model = \"two\"\n").unwrap();
        assert_ne!(
            written.digest,
            poll_home_files(home, Some(&written)).unwrap().digest,
            "changed content is an edit"
        );

        std::fs::remove_file(home.join("config.toml")).unwrap();
        assert_ne!(
            written.digest,
            poll_home_files(home, Some(&written)).unwrap().digest,
            "a deleted input is an edit"
        );
    }

    #[test]
    fn home_watch_rehash_catches_a_same_size_rewrite_with_an_unchanged_stat() {
        let directory = tempfile::tempdir().unwrap();
        let home = directory.path();
        std::fs::write(home.join("config.toml"), "model = \"one\"\n").unwrap();
        let first = poll_home_files(home, None).unwrap();
        let stored_mtime = first.inputs[0].stat.unwrap().mtime.unwrap();

        // A rewrite that keeps the byte length and restores the mtime is
        // invisible to the stat; only the recency re-hash catches it, the
        // way a same-second edit lands on a one-second-granularity filesystem.
        std::fs::write(home.join("config.toml"), "model = \"two\"\n").unwrap();
        std::fs::File::open(home.join("config.toml"))
            .unwrap()
            .set_modified(stored_mtime)
            .unwrap();
        let rewritten = poll_home_files(home, Some(&first)).unwrap();
        assert_eq!(first.inputs[0].stat, rewritten.inputs[0].stat);
        assert_ne!(
            first.digest, rewritten.digest,
            "a same-size rewrite with an unchanged stat is still an edit"
        );
    }

    // Hard-won: #1337 review: a timestamp-preserving rewrite hid from the
    // poll while a probe recorded the fresh digest, so the identity looked
    // stale every pass and re-probed forever.
    #[tokio::test]
    async fn a_stat_hidden_rewrite_converges_once_a_probe_reads_it() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        let old = SystemTime::now() - Duration::from_secs(120);
        write_home_model(root, "solo", "aaa");
        std::fs::File::open(root.join("solo").join("config.toml"))
            .unwrap()
            .set_modified(old)
            .unwrap();
        let calls = calls();
        let (config, homes) = homes_config(root, &[("solo", HarnessKind::Codex)]);
        let catalog = ProfileCatalog::with_probe(home_model_probe(homes, calls.clone()));
        catalog.sync_now(&config).await;
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        catalog.watch_homes();

        // A timestamp-preserving rewrite of the same length is invisible to
        // the stat poll and stays undetected while no probe reads the home.
        write_home_model(root, "solo", "bbb");
        std::fs::File::open(root.join("solo").join("config.toml"))
            .unwrap()
            .set_modified(old)
            .unwrap();
        catalog.watch_homes();
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "a stat-hidden rewrite stays undetected until something reads the home"
        );

        // A probe reads the whole home — the guaranteed one-hour expiry
        // refresh is one — and records the fresh digest. The poll, still
        // blind, must catch up from the probe's read instead of churning a
        // re-probe every pass.
        catalog.refresh(|_, _| true);
        while !catalog.lock().refreshing.is_empty() {
            tokio::task::yield_now().await;
        }
        assert_eq!(
            current_model(&catalog, "solo").await.as_deref(),
            Some("bbb")
        );
        for _ in 0..3 {
            catalog.watch_homes();
            while !catalog.lock().refreshing.is_empty() {
                tokio::task::yield_now().await;
            }
        }
        assert_eq!(
            calls.load(Ordering::SeqCst),
            3,
            "the poll reconciles with the probe's read and stays quiet"
        );
    }

    // Hard-won: #1337: editing a profile's harness home never reached the catalog.
    #[tokio::test]
    async fn an_edited_home_republishes_within_one_watch_pass_and_reprimes_only_that_home() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        write_home_model(root, "alpha", "alpha-first");
        write_home_model(root, "beta", "beta-first");
        let calls = calls();
        let (config, homes) = homes_config(
            root,
            &[("alpha", HarnessKind::Codex), ("beta", HarnessKind::Codex)],
        );
        let catalog = ProfileCatalog::with_probe(home_model_probe(homes, calls.clone()));
        catalog.sync_now(&config).await;
        assert_eq!(calls.load(Ordering::SeqCst), 2);

        // The first pass only baselines the homes; nothing is re-probed.
        catalog.watch_homes();
        assert_eq!(
            calls.load(Ordering::SeqCst),
            2,
            "baselining launches no probe"
        );

        // Rewriting identical content (new mtime, same bytes) is not an edit.
        write_home_model(root, "alpha", "alpha-first");
        catalog.watch_homes();
        assert_eq!(
            calls.load(Ordering::SeqCst),
            2,
            "a metadata-only rewrite re-probes nothing"
        );

        write_home_model(root, "alpha", "alpha-second");
        catalog.watch_homes();
        assert_eq!(
            calls.load(Ordering::SeqCst),
            4,
            "the edited home's default and current-model entries re-probe; beta stays warm"
        );
        converge_on(&catalog, "alpha", "alpha-second").await;
        assert_eq!(
            current_model(&catalog, "beta").await.as_deref(),
            Some("beta-first")
        );
        assert_eq!(calls.load(Ordering::SeqCst), 4);
    }

    #[tokio::test]
    async fn a_home_shared_by_two_profiles_reprimes_each_of_them_and_no_others() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        write_home_model(root, "one", "shared-first");
        write_home_model(root, "other", "other-first");
        let calls = calls();
        let (mut config, mut homes) = homes_config(
            root,
            &[
                ("one", HarnessKind::Codex),
                ("two", HarnessKind::Codex),
                ("other", HarnessKind::Codex),
            ],
        );
        // Two profiles install the same home; editing it must re-probe both.
        config.profiles.get_mut("two").unwrap().home = root.join("one");
        homes.insert("two".to_owned(), root.join("one"));
        let catalog = ProfileCatalog::with_probe(home_model_probe(homes, calls.clone()));
        catalog.sync_now(&config).await;
        assert_eq!(calls.load(Ordering::SeqCst), 3);

        catalog.watch_homes();
        assert_eq!(
            calls.load(Ordering::SeqCst),
            3,
            "baselining launches no probe"
        );
        write_home_model(root, "one", "shared-second");
        catalog.watch_homes();
        assert_eq!(
            calls.load(Ordering::SeqCst),
            7,
            "both profiles of the edited home re-probe, each entry they serve"
        );
        converge_on(&catalog, "one", "shared-second").await;
        converge_on(&catalog, "two", "shared-second").await;
        assert_eq!(
            current_model(&catalog, "other").await.as_deref(),
            Some("other-first")
        );
        assert_eq!(calls.load(Ordering::SeqCst), 7);
    }

    // Hard-won: #1337: a home edit detected while another re-probe was in
    // flight settled on the pre-edit answer that probe published.
    #[tokio::test]
    async fn a_change_detected_during_an_in_flight_reprobe_is_not_lost() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        write_home_model(root, "solo", "solo-first");
        let calls = calls();
        let (config, homes) = homes_config(root, &[("solo", HarnessKind::Codex)]);
        let (release, gate) = tokio::sync::watch::channel(true);
        let catalog = ProfileCatalog::with_probe(held_home_model_probe(homes, calls.clone(), gate));
        catalog.sync_now(&config).await;
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        catalog.watch_homes();

        // An expiry refresh starts and reads the home before the edit; held
        // in flight, it will publish the pre-edit answer when released.
        release.send(false).unwrap();
        catalog.refresh(|_, _| true);
        while calls.load(Ordering::SeqCst) < 3 {
            tokio::task::yield_now().await;
        }
        write_home_model(root, "solo", "solo-second");
        catalog.watch_homes();
        assert_eq!(
            calls.load(Ordering::SeqCst),
            3,
            "entries already re-probing are joined, not doubled"
        );

        // The held probes publish the pre-edit answer.
        release.send(true).unwrap();
        while !catalog.lock().refreshing.is_empty() {
            tokio::task::yield_now().await;
        }
        assert_eq!(
            current_model(&catalog, "solo").await.as_deref(),
            Some("solo-first"),
            "the in-flight probe published what it read"
        );
        assert_eq!(calls.load(Ordering::SeqCst), 3);

        // The completing probes record the digest they read, which predates
        // the edit, so the edit stays pending: the next pass re-detects it
        // and re-probes from the current home. This pass's probes are
        // spawned, so convergence proves they ran.
        catalog.watch_homes();
        converge_on(&catalog, "solo", "solo-second").await;
        assert_eq!(calls.load(Ordering::SeqCst), 5);
    }

    // Hard-won: #1337: a failed edit re-probe recorded nothing, so the edit
    // looked settled until the one-hour expiry refresh.
    #[tokio::test]
    async fn a_failed_home_edit_reprobe_is_retried_until_it_succeeds() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        write_home_model(root, "solo", "solo-first");
        let calls = calls();
        let failing = Arc::new(AtomicBool::new(false));
        let (config, homes) = homes_config(root, &[("solo", HarnessKind::Codex)]);
        let flaky = flaky_home_model_probe(homes, calls.clone(), failing.clone());
        let mut catalog = ProfileCatalog::build(
            CancellationToken::new(),
            flaky.clone(),
            Arc::new(move |id, _| flaky(id)),
        );
        Arc::get_mut(&mut catalog).unwrap().home_edit_retry_wait = Duration::from_millis(50);
        catalog.sync_now(&config).await;
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        catalog.watch_homes();

        failing.store(true, Ordering::SeqCst);
        write_home_model(root, "solo", "solo-second");
        catalog.watch_homes();
        assert_eq!(
            calls.load(Ordering::SeqCst),
            3,
            "the edit re-probes every served entry at once"
        );
        // Let the launched probes run, and confirm they ran while the flag
        // was set: they fail, the old choices stay, and nothing settles.
        while !catalog.lock().refreshing.is_empty() {
            tokio::task::yield_now().await;
        }
        assert_eq!(
            current_model(&catalog, "solo").await.as_deref(),
            Some("solo-first"),
            "a failed re-probe keeps serving the old choices"
        );

        // The failure recorded nothing, so the edit stays pending; only the
        // retry cadence keeps a persistent failure from probing every pass.
        catalog.watch_homes();
        assert_eq!(
            calls.load(Ordering::SeqCst),
            3,
            "the failed re-probe waits for its retry window"
        );

        failing.store(false, Ordering::SeqCst);
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                catalog.watch_homes();
                if current_model(&catalog, "solo").await.as_deref() == Some("solo-second") {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the failed re-probe is retried until the edit converges");
        assert_eq!(calls.load(Ordering::SeqCst), 5);
    }

    // Hard-won: #1337 review: a follow-up edit right after a successful
    // re-probe stalled behind the failed-reprobe retry window.
    #[tokio::test]
    async fn a_follow_up_edit_reprimes_without_waiting_for_the_retry_window() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        write_home_model(root, "solo", "solo-first");
        let calls = calls();
        let (config, homes) = homes_config(root, &[("solo", HarnessKind::Codex)]);
        let mut catalog = ProfileCatalog::with_probe(home_model_probe(homes, calls.clone()));
        Arc::get_mut(&mut catalog).unwrap().home_edit_retry_wait = Duration::from_secs(60);
        catalog.sync_now(&config).await;
        catalog.watch_homes();

        // The first edit re-probes and succeeds, all before the retry window
        // that arming recorded could matter.
        write_home_model(root, "solo", "solo-second");
        catalog.watch_homes();
        assert_eq!(calls.load(Ordering::SeqCst), 3);
        while !catalog.lock().refreshing.is_empty() {
            tokio::task::yield_now().await;
        }
        assert_eq!(
            current_model(&catalog, "solo").await.as_deref(),
            Some("solo-second")
        );

        // A follow-up edit is a new change: it must re-probe on the next
        // pass, not wait out the previous launch's retry window. Every served
        // entry re-probes: the default, the previous model's, and the
        // current model's.
        write_home_model(root, "solo", "solo-third");
        catalog.watch_homes();
        assert_eq!(
            calls.load(Ordering::SeqCst),
            6,
            "a new edit re-probes on the next pass even inside the retry window"
        );
        converge_on(&catalog, "solo", "solo-third").await;
    }

    // Hard-won: #1337 review: an identity-level settled digest let one
    // entry's failed re-probe hide behind a sibling's success.
    #[tokio::test]
    async fn a_failed_entry_reprimes_even_when_its_sibling_reprobe_succeeded() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        write_home_model(root, "solo", "solo-first");
        let calls = calls();
        let failing = Arc::new(AtomicBool::new(false));
        let (config, homes) = homes_config(root, &[("solo", HarnessKind::Codex)]);
        let mut catalog = ProfileCatalog::build(
            CancellationToken::new(),
            home_model_probe(homes.clone(), calls.clone()),
            flaky_model_probe(homes, calls.clone(), failing.clone()),
        );
        Arc::get_mut(&mut catalog).unwrap().home_edit_retry_wait = Duration::from_millis(50);
        catalog.sync_now(&config).await;
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        catalog.watch_homes();

        // The edit re-probes every served entry; the default entry's probe
        // succeeds and the model entry's fails.
        failing.store(true, Ordering::SeqCst);
        write_home_model(root, "solo", "solo-second");
        catalog.watch_homes();
        assert_eq!(calls.load(Ordering::SeqCst), 3);
        while !catalog.lock().refreshing.is_empty() {
            tokio::task::yield_now().await;
        }
        assert_eq!(
            current_model(&catalog, "solo").await.as_deref(),
            Some("solo-second"),
            "the default entry's re-probe published the edit"
        );

        // The failed model entry keeps serving its pre-edit copy; its
        // sibling's success must not settle it. The failure re-arms the
        // retry window, so the next pass waits it out instead of re-probing
        // the whole identity every poll.
        catalog.watch_homes();
        assert_eq!(
            calls.load(Ordering::SeqCst),
            3,
            "the failed re-probe re-arms the retry window"
        );
        tokio::time::sleep(Duration::from_millis(60)).await;
        catalog.watch_homes();
        assert_eq!(
            calls.load(Ordering::SeqCst),
            6,
            "a failed entry re-probes after the window even when its sibling's re-probe succeeded"
        );
        // That round fails too (the flag is still set), re-arming the
        // window; let the failures land before allowing success.
        while !catalog.lock().refreshing.is_empty() {
            tokio::task::yield_now().await;
        }

        failing.store(false, Ordering::SeqCst);
        for _ in 0..4 {
            catalog.watch_homes();
            while !catalog.lock().refreshing.is_empty() {
                tokio::task::yield_now().await;
            }
            tokio::time::sleep(Duration::from_millis(60)).await;
        }
        assert_eq!(
            current_model(&catalog, "solo").await.as_deref(),
            Some("solo-second")
        );
        assert_eq!(calls.load(Ordering::SeqCst), 9);
    }

    // Hard-won: #1337 review: a permanently failed entry kept its identity
    // stale, re-probing the healthy entries every pass.
    #[tokio::test]
    async fn a_failed_entry_does_not_keep_repriming_its_healthy_siblings() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        write_home_model(root, "solo", "solo-first");
        let calls = calls();
        let (mut config, homes) = homes_config(root, &[("solo", HarnessKind::Codex)]);
        config.subagents.straightforward = Some(mj_core::config::SubagentChoice {
            model: "extra".into(),
            effort: None,
        });
        let probe_homes = homes.clone();
        let model_homes = homes;
        let probe_calls = calls.clone();
        let model_calls = calls.clone();
        let catalog = ProfileCatalog::build(
            CancellationToken::new(),
            Arc::new(move |profile| {
                probe_calls.fetch_add(1, Ordering::SeqCst);
                let homes = probe_homes.clone();
                Box::pin(async move {
                    let mut choices = read_home_choices(&homes, &profile)?;
                    choices.models.push(mj_core::acp::SessionConfigChoice {
                        value: "extra".into(),
                        name: "extra".into(),
                        description: None,
                    });
                    Ok(choices)
                })
            }),
            Arc::new(move |profile, model| {
                model_calls.fetch_add(1, Ordering::SeqCst);
                let homes = model_homes.clone();
                Box::pin(async move {
                    // The delisted model's probe fails for good; other models
                    // answer from the home like the default probe does.
                    if model == "extra" {
                        return Err(crate::controller::profile_config::DiscoveryUnsupported.into());
                    }
                    let mut choices = read_home_choices(&homes, &profile)?;
                    choices.model = Some(model.clone());
                    Ok(choices)
                })
            }),
        );
        catalog.sync_now(&config).await;
        // sync_now claimed the default and the current model, and the
        // selected model's probe failed permanently.
        assert_eq!(calls.load(Ordering::SeqCst), 2);

        // The failed entry neither settles nor blocks convergence: passes
        // stay quiet, with no re-probes of the healthy entries.
        catalog.watch_homes();
        catalog.watch_homes();
        assert_eq!(
            calls.load(Ordering::SeqCst),
            2,
            "a failed entry does not keep its identity stale"
        );

        // A home edit still re-probes the healthy entries, and convergence
        // returns once they record the new digest.
        write_home_model(root, "solo", "solo-second");
        catalog.watch_homes();
        assert_eq!(calls.load(Ordering::SeqCst), 4);
        converge_on(&catalog, "solo", "solo-second").await;
        catalog.watch_homes();
        assert_eq!(calls.load(Ordering::SeqCst), 4);
    }

    // Hard-won: #1337: an edit made while the daemon was stopped stayed stale after start.
    #[tokio::test]
    async fn a_new_catalog_generation_discovers_the_current_home() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        write_home_model(root, "solo", "solo-first");
        let calls = calls();
        let (config, homes) = homes_config(root, &[("solo", HarnessKind::Codex)]);
        let first = ProfileCatalog::with_probe(home_model_probe(homes.clone(), calls.clone()));
        first.sync_now(&config).await;
        assert_eq!(
            current_model(&first, "solo").await.as_deref(),
            Some("solo-first")
        );

        // Capability entries and the process-global probe cache live only as
        // long as the daemon process, so a daemon that stopped and started
        // again discovers fresh. A new catalog over the same configuration
        // is what such a start looks like from inside one test process.
        write_home_model(root, "solo", "solo-second");
        let second = ProfileCatalog::with_probe(home_model_probe(homes, calls.clone()));
        second.sync_now(&config).await;
        assert_eq!(
            calls.load(Ordering::SeqCst),
            2,
            "a fresh generation probes instead of serving the previous generation's answer"
        );
        assert_eq!(
            current_model(&second, "solo").await.as_deref(),
            Some("solo-second")
        );
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
        assert_eq!(
            upgraded.review.trigger(),
            mj_core::config::ReviewTrigger::Off
        );
        assert!(upgraded.review.reviewers.is_empty());
    }

    #[test]
    fn pending_automatic_review_upgrades_and_unrelated_save_keeps_it() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        // A configuration that asked for review without naming a model. One
        // with no `[review]` table never asked, so it stays off and is not
        // given reviewers.
        std::fs::write(
            &path,
            format!(
                "version = {}\n# keep\n[review]\ntrigger = \"push\"\n",
                mj_core::config::CONFIG_VERSION
            ),
        )
        .unwrap();
        let config = Config::load_from(&path).unwrap();
        assert!(config.review.is_legacy());
        std::fs::write(
            directory.path().join("none.toml"),
            format!("version = {}\n", mj_core::config::CONFIG_VERSION),
        )
        .unwrap();
        assert!(
            !Config::load_from(&directory.path().join("none.toml"))
                .unwrap()
                .review
                .is_legacy()
        );
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
    // Hard-won: dfbb210a5: restricted Muse reads restarted default discovery.
    #[tokio::test]
    async fn restricted_live_model_read_reuses_cached_default() {
        let probes = calls();
        let counted = probes.clone();
        let catalog = ProfileCatalog::build(
            CancellationToken::new(),
            counting_probe(probes.clone()),
            Arc::new(move |id, model| {
                counted.fetch_add(1, Ordering::SeqCst);
                Box::pin(async move {
                    let mut choices = test_choices(&id);
                    choices.model = Some(model);
                    Ok(choices)
                })
            }),
        );
        let mut config = test_config(&[("p", HarnessKind::Muse)]);
        config.profiles.get_mut("p").unwrap().models = vec!["allowed".into()];
        catalog.sync_now(&config).await;
        probes.store(0, Ordering::SeqCst);
        let choices = catalog
            .model_capabilities("p".into(), "allowed".into())
            .await
            .unwrap();
        assert_eq!(probes.load(Ordering::SeqCst), 1);
        assert!(
            choices
                .models
                .iter()
                .any(|choice| choice.value == "p-model")
        );
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
