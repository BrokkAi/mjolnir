//! Backend inputs are maintained independently of any control surface.
use super::*;
use crate::pollers::*;
use crate::quota::{ProfileQuota, QuotaReportMap, QuotaReportPublication};
use crate::server_runtime::profile_catalog::ProfileCatalog;
use crate::worker_client::CredentialSyncCoordinator;
use std::time::SystemTime;
use tokio::sync::{mpsc, watch};

#[derive(Clone)]
pub(crate) struct Services {
    pub backend: Arc<ApiBackend>,
    pub quotas: watch::Receiver<QuotaReportMap>,
    refresh: mpsc::UnboundedSender<crate::daemon::QuotaRefreshIntent>,
}
impl Services {
    pub(crate) fn refresh_quotas(&self) {
        let _ = self.refresh.send(crate::daemon::QuotaRefreshIntent::Force);
    }
}

pub(super) struct Policy {
    pub services: Services,
    state: Arc<RuntimeState>,
    pub quota_source_reader: super::QuotaSourceReader,
    catalog: Arc<ProfileCatalog>,
    reports: Arc<std::sync::Mutex<QuotaReportMap>>,
    rejected: Arc<std::sync::Mutex<mj_core::credentials::RejectedLogins>>,
    quotas_tx: watch::Sender<QuotaReportMap>,
    profiles_tx: watch::Sender<QuotaRefreshBatch>,
    pub quota_rx: mpsc::Receiver<QuotaUpdate>,
    pub refresh_rx: mpsc::UnboundedReceiver<crate::daemon::QuotaRefreshIntent>,
    batch: QuotaRefreshBatch,
    quota_sources: QuotaSourceScan,
    catalog_config: Option<mj_core::config::Config>,
    /// Profiles the poller is asking a provider about right now.
    probing: std::collections::BTreeSet<String>,
    /// Poll cycles the poller has finished.
    cycles: u64,
    credentials: CredentialSyncCoordinator,
    signals: CredentialSyncSignalTracker,
    notices: CredentialSyncNotices,
}

#[derive(Default)]
enum QuotaSourceScan {
    #[default]
    Idle,
    Queued(crate::daemon::QuotaRefreshIntent),
    Running {
        task: Option<tokio::task::Id>,
        config: Box<mj_core::config::Config>,
        intent: crate::daemon::QuotaRefreshIntent,
        successor: Option<crate::daemon::QuotaRefreshIntent>,
    },
}

impl QuotaSourceScan {
    fn request(&mut self, intent: crate::daemon::QuotaRefreshIntent) {
        match self {
            Self::Idle => *self = Self::Queued(intent),
            Self::Queued(pending) => *pending = coalesce_intent(*pending, intent),
            Self::Running {
                intent: running,
                successor,
                ..
            } => {
                let pending = successor.take().unwrap_or(*running);
                *successor = Some(coalesce_intent(pending, intent));
            }
        }
    }

    fn start(
        &mut self,
        config: mj_core::config::Config,
    ) -> Option<crate::daemon::QuotaRefreshIntent> {
        let Self::Queued(intent) = self else {
            return None;
        };
        let intent = *intent;
        *self = Self::Running {
            task: None,
            config: Box::new(config),
            intent,
            successor: None,
        };
        Some(intent)
    }

    fn task_started(&mut self, task: tokio::task::Id) {
        if let Self::Running { task: slot, .. } = self {
            *slot = Some(task);
        }
    }

    fn running(&self, task: tokio::task::Id) -> bool {
        matches!(self, Self::Running { task: Some(current), .. } if *current == task)
    }

    fn finish(
        &mut self,
        task: tokio::task::Id,
        current_config: &mj_core::config::Config,
    ) -> Option<(crate::daemon::QuotaRefreshIntent, bool)> {
        let Self::Running {
            task: Some(current_task),
            config,
            intent,
            successor,
        } = self
        else {
            return None;
        };
        if *current_task != task {
            return None;
        }
        let running_intent = *intent;
        let successor = *successor;
        let stale = config.as_ref() != current_config;
        let rerun = stale || successor.is_some();
        let next_intent = if rerun {
            let followup = successor.unwrap_or(running_intent);
            if stale {
                coalesce_intent(followup, running_intent)
            } else {
                followup
            }
        } else {
            running_intent
        };
        *self = if rerun {
            Self::Queued(next_intent)
        } else {
            Self::Idle
        };
        Some((next_intent, rerun))
    }
}

fn coalesce_intent(
    left: crate::daemon::QuotaRefreshIntent,
    right: crate::daemon::QuotaRefreshIntent,
) -> crate::daemon::QuotaRefreshIntent {
    if left == crate::daemon::QuotaRefreshIntent::Force
        || right == crate::daemon::QuotaRefreshIntent::Force
    {
        crate::daemon::QuotaRefreshIntent::Force
    } else {
        crate::daemon::QuotaRefreshIntent::ReconcileSources
    }
}
impl Policy {
    pub fn new(
        state: Arc<RuntimeState>,
        manager: &crate::session_manager::SessionManagerControl,
        stop: &CancellationToken,
        quota_source_reader: super::QuotaSourceReader,
    ) -> Self {
        let catalog = ProfileCatalog::new_with_config_path(
            stop.child_token(),
            mj_core::config::config_path(),
        );
        let weak_state = Arc::downgrade(&state);
        catalog.set_publisher(Arc::new(move || {
            if let Some(state) = weak_state.upgrade() {
                state.publish_revision();
            }
        }));
        let weak_state = Arc::downgrade(&state);
        catalog.set_notice(Arc::new(move |message| {
            if let Some(state) = weak_state.upgrade() {
                state.push_notice("", message);
            }
        }));
        assert!(
            state.profile_catalog.set(catalog.clone()).is_ok(),
            "daemon catalogue installed once"
        );
        let reports = Arc::new(std::sync::Mutex::new(QuotaReportMap::new()));
        let (quotas_tx, quotas) = watch::channel(QuotaReportMap::new());
        let (refresh, refresh_rx) = mpsc::unbounded_channel();
        catalog.set_quota_reports(reports.clone());
        let rejected = Arc::new(std::sync::Mutex::new(
            mj_core::credentials::RejectedLogins::default(),
        ));
        let runtime = state.clone();
        let backend = Arc::new(
            ApiBackend::new(
                manager.client(),
                Arc::new(move |id| runtime.session_state(id)),
                state.clone(),
            )
            .with_profile_catalog(catalog.clone())
            .with_quota_reports(reports.clone())
            .with_quota_publications(quotas.clone())
            .with_quota_refresh(refresh.clone())
            .with_rejected_logins(rejected.clone()),
        );
        state.install_report_backend(&backend);
        // A surface's Refresh reaches this poller through the daemon.
        state.attach_quota_refresh(refresh.clone());
        // The stored report is a rebuildable cache: a failed read only costs a probe.
        let (profiles_tx, quota_rx) = spawn_quota_refresher(Arc::new(|request| {
            crate::database::load_quota_cache(&request.cache_identity())
                .inspect_err(
                    |error| tracing::warn!(%error, "could not read the stored quota report"),
                )
                .ok()
                .flatten()
        }));
        let credentials = CredentialSyncCoordinator::spawn_guarded(
            manager.clone(),
            state.worker_background_gate(),
            state.clone(),
        );
        let mut policy = Self {
            services: Services {
                backend,
                quotas,
                refresh,
            },
            state,
            quota_source_reader,
            catalog,
            reports,
            rejected,
            quotas_tx,
            profiles_tx,
            quota_rx,
            refresh_rx,
            batch: QuotaRefreshBatch::default(),
            quota_sources: QuotaSourceScan::default(),
            catalog_config: None,
            probing: Default::default(),
            cycles: 0,
            credentials,
            signals: CredentialSyncSignalTracker::default(),
            notices: CredentialSyncNotices::default(),
        };
        policy.sync(false);
        policy
    }
    pub fn sync(&mut self, force: bool) {
        let controller = {
            let owner = self.state.owner();
            if !force && self.catalog_config.as_ref() == Some(&owner.controller().config) {
                return;
            }
            crate::controller::Controller {
                config: owner.controller().config.clone(),
                state: Default::default(),
            }
        };
        self.catalog.sync(&controller.config);
        self.catalog_config = Some(controller.config.clone());
        self.quota_sources.request(if force {
            crate::daemon::QuotaRefreshIntent::Force
        } else {
            crate::daemon::QuotaRefreshIntent::ReconcileSources
        });
    }

    /// Consume all queued requests together; Force dominates source-only
    /// reconciliation while retaining the original manual-refresh behavior.
    pub fn refresh(&mut self, first: crate::daemon::QuotaRefreshIntent) {
        match coalesce_refresh_intents(first, &mut self.refresh_rx) {
            crate::daemon::QuotaRefreshIntent::Force => self.sync(true),
            crate::daemon::QuotaRefreshIntent::ReconcileSources => self
                .quota_sources
                .request(crate::daemon::QuotaRefreshIntent::ReconcileSources),
        }
    }

    pub fn start_quota_source_scan(&mut self) -> Option<mj_core::config::Config> {
        let config = self.state.owner().controller().config.clone();
        self.quota_sources.start(config.clone())?;
        Some(config)
    }

    pub fn quota_source_task_started(&mut self, task: tokio::task::Id) {
        self.quota_sources.task_started(task);
    }

    pub fn is_quota_source_task(&self, task: tokio::task::Id) -> bool {
        self.quota_sources.running(task)
    }

    pub fn quota_sources_finished(
        &mut self,
        task: tokio::task::Id,
        result: Result<Vec<crate::quota::QuotaRefreshRequest>>,
    ) {
        if let Err(error) = &result {
            tracing::warn!(%error, "quota source scan task failed");
        }
        let current_config = self.state.owner().controller().config.clone();
        let Some((intent, obsolete)) = self.quota_sources.finish(task, &current_config) else {
            return;
        };
        if obsolete {
            tracing::debug!(
                "discarding obsolete quota source scan and reconciling current sources"
            );
            return;
        }
        if let Ok(requests) = result {
            self.apply_source_scan(intent, requests);
        }
    }

    fn apply_source_scan(
        &mut self,
        intent: crate::daemon::QuotaRefreshIntent,
        requests: Vec<crate::quota::QuotaRefreshRequest>,
    ) {
        if intent == crate::daemon::QuotaRefreshIntent::Force {
            self.batch.generation = self.batch.generation.saturating_add(1);
            self.batch.profiles = requests;
            self.batch.refresh = true;
            self.profiles_tx.send_replace(self.batch.clone());
            self.batch.refresh = false;
        } else {
            replace_source_requests(&mut self.batch, &self.profiles_tx, requests.clone());
        }
        self.reconcile_publications(&self.batch.profiles.clone());
    }

    fn reconcile_publications(&mut self, requests: &[crate::quota::QuotaRefreshRequest]) {
        let desired = requests
            .iter()
            .map(|request| (request.profile_id.clone(), request.cache_identity()))
            .collect::<BTreeMap<_, _>>();
        let changed = {
            let mut reports = self.reports.lock().expect("quota reports poisoned");
            let changed = reconcile_quota_publications(&mut reports, &desired);
            // ReconcileSources must wake readers even when the source reverted
            // to an identity whose report is still reusable.
            self.quotas_tx.send_replace(reports.clone());
            changed
        };
        if !changed.is_empty() {
            let mut health = crate::profile_health::shared()
                .lock()
                .expect("profile health poisoned");
            for id in changed {
                health.forget_quota(&id);
            }
        }
        self.probing.retain(|id| desired.contains_key(id));
        self.publish_quotas();
    }

    /// Tell every attached surface what the daemon knows about quota.
    fn publish_quotas(&self) {
        let reports = self.reports.lock().expect("quota reports poisoned").clone();
        self.state.publish_quotas(mj_client::quota::QuotaSnapshot {
            reports: surface_quota_reports(&reports),
            probing: self.probing.clone(),
            cycles: self.cycles,
        });
    }
    pub fn observe(&mut self, id: &str, observation: &DelegationObservation) {
        if let Some(signal) = &observation.credential_signal
            && let Some(session) = self.state.session_record(id)
        {
            self.signals
                .observe(id, &session.last_profile, signal.clone());
        }
    }
    pub fn quota(&mut self, update: QuotaUpdate) {
        match update {
            QuotaUpdate::Refreshing { profile_ids } => self.probing.extend(profile_ids),
            QuotaUpdate::Report(outcome) => {
                let profile_id = outcome.report.profile_id.clone();
                let accepted = {
                    let mut reports = self.reports.lock().expect("quota reports poisoned");
                    let accepted = publish_quota_outcome(&mut reports, &outcome);
                    if accepted {
                        self.quotas_tx.send_replace(reports.clone());
                    }
                    accepted
                };
                if !accepted {
                    tracing::debug!(
                        profile_id,
                        "discarding quota result for an obsolete profile identity"
                    );
                    return;
                }
                if outcome.credentials_changed {
                    self.credentials
                        .handle()
                        .sync_profile_now(&profile_id, None);
                }
                self.probing.remove(&profile_id);
                // A usage-priced profile has no allowance to run out of.
                let windows = outcome.report.error.is_none().then(|| {
                    if outcome.report.is_usage_priced() {
                        Vec::new()
                    } else {
                        outcome.report.windows.clone()
                    }
                });
                if let Some(windows) = windows {
                    crate::profile_health::shared()
                        .lock()
                        .expect("profile health poisoned")
                        .observe_quota(&profile_id, &windows, SystemTime::now());
                }
            }
            QuotaUpdate::Finished { .. } => {
                self.probing.clear();
                self.cycles += 1;
            }
        }
        self.publish_quotas();
    }
    pub fn tick(&mut self) {
        schedule_due_credential_syncs(
            &mut self.signals,
            &self.credentials.handle(),
            Instant::now(),
        );
        while let Some(result) = self.credentials.try_result() {
            log_credential_sync_actions(&result);
            let notice = {
                let owner = self.state.owner();
                let controller = owner.controller();
                let profile = controller.config.profiles.get(&result.profile_id);
                if let Some(profile) = profile {
                    self.rejected
                        .lock()
                        .expect("rejected logins poisoned")
                        .observe(&result, profile);
                }
                self.notices
                    .leveled_notice(&result, profile.map(|p| p.kind), &controller.state)
            };
            if let Some(notice) = notice {
                if notice.routine {
                    tracing::info!(notice = %notice.text, "credential synchronization notice");
                } else {
                    tracing::warn!(notice = %notice.text, "credential synchronization notice");
                }
                self.state.push_notice("", notice.text);
            }
        }
    }
}

fn coalesce_refresh_intents(
    first: crate::daemon::QuotaRefreshIntent,
    pending: &mut mpsc::UnboundedReceiver<crate::daemon::QuotaRefreshIntent>,
) -> crate::daemon::QuotaRefreshIntent {
    let mut force = first == crate::daemon::QuotaRefreshIntent::Force;
    while let Ok(intent) = pending.try_recv() {
        force |= intent == crate::daemon::QuotaRefreshIntent::Force;
    }
    if force {
        crate::daemon::QuotaRefreshIntent::Force
    } else {
        crate::daemon::QuotaRefreshIntent::ReconcileSources
    }
}

fn replace_source_requests(
    batch: &mut QuotaRefreshBatch,
    profiles: &watch::Sender<QuotaRefreshBatch>,
    requests: Vec<crate::quota::QuotaRefreshRequest>,
) -> bool {
    if requests == batch.profiles {
        return false;
    }
    batch.generation = batch.generation.saturating_add(1);
    batch.profiles = requests;
    batch.refresh = false;
    profiles.send_replace(batch.clone());
    true
}

fn publish_quota_outcome(
    reports: &mut QuotaReportMap,
    outcome: &crate::quota::QuotaRefreshOutcome,
) -> bool {
    let Some(publication) = reports.get_mut(&outcome.report.profile_id) else {
        return false;
    };
    if publication.cache_identity != outcome.cache_identity {
        return false;
    }
    publication.report = Some(outcome.report.clone());
    true
}

fn reconcile_quota_publications(
    reports: &mut QuotaReportMap,
    desired: &BTreeMap<String, String>,
) -> Vec<String> {
    let mut changed = reports
        .keys()
        .filter(|id| !desired.contains_key(*id))
        .cloned()
        .collect::<Vec<_>>();
    reports.retain(|id, _| desired.contains_key(id));
    for (id, identity) in desired {
        if reports
            .get(id)
            .is_none_or(|old| old.cache_identity != *identity)
        {
            reports.insert(
                id.clone(),
                QuotaReportPublication {
                    cache_identity: identity.clone(),
                    report: None,
                },
            );
            changed.push(id.clone());
        }
    }
    changed
}

fn surface_quota_reports(reports: &QuotaReportMap) -> BTreeMap<String, ProfileQuota> {
    reports
        .iter()
        .filter_map(|(id, publication)| {
            publication
                .report
                .as_ref()
                .map(|report| (id.clone(), crate::quota::surface_quota_report(report)))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    // Hard-won: #1487: an older native-source scan could publish over edited profile inputs.
    #[tokio::test]
    async fn obsolete_quota_source_scan_is_discarded_and_force_carries_forward() {
        let old = mj_core::config::Config::default();
        let mut current = old.clone();
        current.profiles.insert(
            "edited".into(),
            mj_core::config::HarnessProfile::new(
                mj_core::config::HarnessKind::Codex,
                std::path::PathBuf::from("quota-source-edited"),
            ),
        );

        let mut scan = QuotaSourceScan::default();
        scan.request(crate::daemon::QuotaRefreshIntent::ReconcileSources);
        assert_eq!(
            scan.start(old.clone()),
            Some(crate::daemon::QuotaRefreshIntent::ReconcileSources)
        );
        let task = tokio::spawn(async {});
        let task_id = task.id();
        task.await.unwrap();
        scan.task_started(task_id);
        scan.request(crate::daemon::QuotaRefreshIntent::Force);

        let (intent, obsolete) = scan.finish(task_id, &current).unwrap();
        assert!(
            obsolete,
            "the old scan must not be applied to edited config"
        );
        assert_eq!(intent, crate::daemon::QuotaRefreshIntent::Force);
        assert_eq!(
            scan.start(current),
            Some(crate::daemon::QuotaRefreshIntent::Force)
        );
    }

    #[tokio::test]
    async fn queued_force_dominates_source_reconciliation() {
        let (sender, mut receiver) = mpsc::unbounded_channel();
        sender
            .send(crate::daemon::QuotaRefreshIntent::ReconcileSources)
            .unwrap();
        sender
            .send(crate::daemon::QuotaRefreshIntent::Force)
            .unwrap();
        sender
            .send(crate::daemon::QuotaRefreshIntent::ReconcileSources)
            .unwrap();

        assert_eq!(
            coalesce_refresh_intents(
                crate::daemon::QuotaRefreshIntent::ReconcileSources,
                &mut receiver,
            ),
            crate::daemon::QuotaRefreshIntent::Force
        );
        assert!(receiver.try_recv().is_err());

        let config = mj_core::config::Config::default();
        let mut scan = QuotaSourceScan::default();
        scan.request(crate::daemon::QuotaRefreshIntent::Force);
        assert_eq!(
            scan.start(config.clone()),
            Some(crate::daemon::QuotaRefreshIntent::Force)
        );
        let task = tokio::spawn(async {});
        let task_id = task.id();
        task.await.unwrap();
        scan.task_started(task_id);
        scan.request(crate::daemon::QuotaRefreshIntent::ReconcileSources);
        let (intent, obsolete) = scan.finish(task_id, &config).unwrap();
        assert!(obsolete);
        assert_eq!(intent, crate::daemon::QuotaRefreshIntent::Force);
        assert_eq!(
            scan.start(config),
            Some(crate::daemon::QuotaRefreshIntent::Force)
        );
    }

    // Hard-won: #1487: native source edits were missed when Mjolnir config was unchanged.
    #[test]
    fn native_codex_provider_edit_republishes_only_changed_source_requests() {
        let home = tempfile::tempdir().unwrap();
        let native_config = home.path().join("config.toml");
        let write_provider = |env_key: &str| {
            std::fs::write(
                &native_config,
                format!(
                    "model_provider = \"custom\"\n\
                     [model_providers.custom]\n\
                     base_url = \"https://api.invalid/v1\"\n\
                     env_key = \"{env_key}\"\n\
                     wire_api = \"responses\"\n"
                ),
            )
            .unwrap();
        };
        write_provider("ZAI_API_KEY");
        let codex = mj_core::config::HarnessProfile {
            environment: [
                ("ZAI_API_KEY".into(), "first-provider-key".into()),
                ("ZAI_API_KEY_TWO".into(), "second-provider-key".into()),
            ]
            .into_iter()
            .collect(),
            ..mj_core::config::HarnessProfile::new(
                mj_core::config::HarnessKind::Codex,
                home.path().to_path_buf(),
            )
        };
        let other_home = tempfile::tempdir().unwrap();
        let claude = mj_core::config::HarnessProfile::new(
            mj_core::config::HarnessKind::Claude,
            other_home.path().to_path_buf(),
        );
        let mut controller = crate::controller::Controller {
            config: mj_core::config::Config {
                profiles: BTreeMap::from([("codex".into(), codex), ("claude".into(), claude)]),
                ..Default::default()
            },
            state: Default::default(),
        };
        controller.config.quota.runtime = mj_core::config::QuotaRuntimeChoice::Auto;
        let before = quota_refresh_profiles(&controller);
        let (profiles_tx, profiles_rx) = watch::channel(QuotaRefreshBatch::default());
        let mut batch = QuotaRefreshBatch {
            profiles: before.clone(),
            ..Default::default()
        };

        write_provider("ZAI_API_KEY_TWO");
        let after = quota_refresh_profiles(&controller);
        assert!(replace_source_requests(
            &mut batch,
            &profiles_tx,
            after.clone()
        ));

        let published = profiles_rx.borrow().clone();
        assert!(!published.refresh, "source reconciliation is not Force");
        assert_ne!(
            before
                .iter()
                .find(|request| request.profile_id == "codex")
                .unwrap()
                .cache_identity(),
            after
                .iter()
                .find(|request| request.profile_id == "codex")
                .unwrap()
                .cache_identity(),
        );
        assert_eq!(
            before
                .iter()
                .find(|request| request.profile_id == "claude")
                .unwrap(),
            after
                .iter()
                .find(|request| request.profile_id == "claude")
                .unwrap(),
        );

        let first_generation = published.generation;
        assert!(!replace_source_requests(
            &mut batch,
            &profiles_tx,
            after.clone()
        ));
        assert_eq!(profiles_rx.borrow().generation, first_generation);

        let added_home = tempfile::tempdir().unwrap();
        controller.config.profiles.insert(
            "added".into(),
            mj_core::config::HarnessProfile::new(
                mj_core::config::HarnessKind::Claude,
                added_home.path().to_path_buf(),
            ),
        );
        let grown = quota_refresh_profiles(&controller);
        assert!(replace_source_requests(
            &mut batch,
            &profiles_tx,
            grown.clone()
        ));
        assert_eq!(
            profiles_rx
                .borrow()
                .profiles
                .iter()
                .map(|request| request.profile_id.as_str())
                .collect::<Vec<_>>(),
            ["added", "claude", "codex"]
        );

        controller.config.quota.runtime = mj_core::config::QuotaRuntimeChoice::Host;
        let runtime_changed = quota_refresh_profiles(&controller);
        assert!(replace_source_requests(
            &mut batch,
            &profiles_tx,
            runtime_changed.clone()
        ));
        let runtime_batch = profiles_rx.borrow().clone();
        assert!(!runtime_batch.refresh);
        assert_eq!(
            runtime_batch.profiles[0].runtime.choice,
            mj_core::config::QuotaRuntimeChoice::Host
        );
        let runtime_generation = runtime_batch.generation;
        assert!(!replace_source_requests(
            &mut batch,
            &profiles_tx,
            runtime_changed
        ));
        assert_eq!(profiles_rx.borrow().generation, runtime_generation);
    }

    // Hard-won: #1487: an old quota result repopulated an edited profile's slot.
    #[test]
    fn quota_result_from_old_profile_identity_cannot_repopulate_edited_slot() {
        let mut reports = BTreeMap::from([(
            "codex".to_owned(),
            QuotaReportPublication {
                cache_identity: "old-profile".into(),
                report: Some(ProfileQuota {
                    profile_id: "codex".into(),
                    harness: mj_core::config::HarnessKind::Codex,
                    windows: Vec::new(),
                    paid_credits: None,
                    banked_resets: None,
                    extra: None,
                    error: None,
                    refreshed_at_epoch_seconds: 0,
                    rate_limited_until_epoch_seconds: None,
                }),
            },
        )]);
        let outcome = crate::quota::QuotaRefreshOutcome {
            cache_identity: "old-profile".into(),
            report: ProfileQuota {
                profile_id: "codex".into(),
                harness: mj_core::config::HarnessKind::Codex,
                windows: Vec::new(),
                paid_credits: None,
                banked_resets: None,
                extra: None,
                error: Some("old request failed".into()),
                refreshed_at_epoch_seconds: 1,
                rate_limited_until_epoch_seconds: None,
            },
            credentials_changed: false,
        };
        reconcile_quota_publications(
            &mut reports,
            &BTreeMap::from([("codex".into(), "edited-profile".into())]),
        );
        assert!(!publish_quota_outcome(&mut reports, &outcome));
        assert!(reports["codex"].report.is_none());
    }

    #[test]
    fn published_quota_snapshot_keeps_claude_fable_and_hides_other_scopes() {
        let report = ProfileQuota {
            paid_credits: None,
            banked_resets: None,
            profile_id: "claude".into(),
            harness: mj_core::config::HarnessKind::Claude,
            windows: vec![
                crate::quota::QuotaWindow {
                    label: "Week".into(),
                    remaining_percent: Some(70),
                    duration_seconds: Some(604_800),
                    model_scope: None,
                    used: None,
                    limit: None,
                    resets: None,
                    resets_at_epoch_seconds: Some(2_000),
                },
                crate::quota::QuotaWindow {
                    label: "Week (Fable)".into(),
                    remaining_percent: Some(17),
                    duration_seconds: Some(604_800),
                    model_scope: Some("fable".into()),
                    used: None,
                    limit: None,
                    resets: None,
                    resets_at_epoch_seconds: Some(1_500),
                },
                crate::quota::QuotaWindow {
                    label: "Week (Other)".into(),
                    remaining_percent: Some(8),
                    duration_seconds: Some(604_800),
                    model_scope: Some("other-model".into()),
                    used: None,
                    limit: None,
                    resets: None,
                    resets_at_epoch_seconds: None,
                },
            ],
            extra: None,
            error: None,
            refreshed_at_epoch_seconds: 1_000,
            rate_limited_until_epoch_seconds: None,
        };
        let codex = ProfileQuota {
            profile_id: "codex".into(),
            harness: mj_core::config::HarnessKind::Codex,
            windows: vec![crate::quota::QuotaWindow {
                label: "Week (Scoped)".into(),
                remaining_percent: Some(9),
                duration_seconds: Some(604_800),
                model_scope: Some("fable".into()),
                used: None,
                limit: None,
                resets: None,
                resets_at_epoch_seconds: None,
            }],
            ..report.clone()
        };
        let reports = BTreeMap::from([
            ("claude".to_owned(), report.clone()),
            ("codex".to_owned(), codex.clone()),
        ]);
        let publications = reports
            .iter()
            .map(|(id, report)| {
                (
                    id.clone(),
                    QuotaReportPublication {
                        cache_identity: "test-identity".into(),
                        report: Some(report.clone()),
                    },
                )
            })
            .collect::<QuotaReportMap>();
        let snapshot = mj_client::quota::QuotaSnapshot {
            reports: surface_quota_reports(&publications),
            ..Default::default()
        };
        let serialized = serde_json::to_vec(&snapshot).expect("serialize published snapshot");
        let snapshot: mj_client::quota::QuotaSnapshot =
            serde_json::from_slice(&serialized).expect("read published snapshot");

        assert_eq!(reports["claude"].windows.len(), 3);
        assert_eq!(reports["codex"].windows.len(), 1);
        assert_eq!(snapshot.reports["claude"].windows.len(), 2);
        assert_eq!(snapshot.reports["claude"].windows[0].label, "Week");
        assert_eq!(snapshot.reports["claude"].windows[1].label, "Week (Fable)");
        assert_eq!(
            snapshot.reports["claude"].windows[1].remaining_percent,
            Some(17)
        );
        assert_eq!(snapshot.reports["codex"].windows.len(), 0);
    }
}
