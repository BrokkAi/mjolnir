//! Backend inputs are maintained independently of any control surface.
use super::*;
use crate::pollers::*;
use crate::quota::ProfileQuota;
use crate::server_runtime::profile_catalog::ProfileCatalog;
use crate::worker_client::CredentialSyncCoordinator;
use tokio::sync::{mpsc, watch};

#[derive(Clone)]
pub(crate) struct Services {
    pub backend: Arc<ApiBackend>,
    pub quotas: watch::Receiver<BTreeMap<String, ProfileQuota>>,
    refresh: mpsc::Sender<()>,
}
impl Services {
    pub(crate) fn refresh_quotas(&self) {
        let _ = self.refresh.try_send(());
    }
}

pub(super) struct Policy {
    pub services: Services,
    state: Arc<RuntimeState>,
    catalog: Arc<ProfileCatalog>,
    reports: Arc<std::sync::Mutex<BTreeMap<String, ProfileQuota>>>,
    rejected: Arc<std::sync::Mutex<mj_core::credentials::RejectedLogins>>,
    quotas_tx: watch::Sender<BTreeMap<String, ProfileQuota>>,
    profiles_tx: watch::Sender<QuotaRefreshBatch>,
    pub quota_rx: mpsc::Receiver<QuotaUpdate>,
    pub refresh_rx: mpsc::Receiver<()>,
    batch: QuotaRefreshBatch,
    published: BTreeMap<String, mj_core::config::HarnessProfile>,
    /// Profiles the poller is asking a provider about right now.
    probing: std::collections::BTreeSet<String>,
    /// Poll cycles the poller has finished.
    cycles: u64,
    credentials: CredentialSyncCoordinator,
    signals: CredentialSyncSignalTracker,
    notices: CredentialSyncNotices,
}
impl Policy {
    pub fn new(
        state: Arc<RuntimeState>,
        manager: &crate::session_manager::SessionManagerControl,
        stop: &CancellationToken,
    ) -> Self {
        let catalog = ProfileCatalog::new(stop.child_token());
        let reports = Arc::new(std::sync::Mutex::new(BTreeMap::new()));
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
            .with_rejected_logins(rejected.clone()),
        );
        let (quotas_tx, quotas) = watch::channel(BTreeMap::new());
        let (refresh, refresh_rx) = mpsc::channel(1);
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
        );
        let mut policy = Self {
            services: Services {
                backend,
                quotas,
                refresh,
            },
            state,
            catalog,
            reports,
            rejected,
            quotas_tx,
            profiles_tx,
            quota_rx,
            refresh_rx,
            batch: QuotaRefreshBatch::default(),
            published: BTreeMap::new(),
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
        let controller = self.state.worker_controller_projection();
        self.catalog.sync(&controller.config);
        self.credentials
            .handle()
            .set_targets(credential_sync_targets(&controller));
        if force || self.published != controller.config.profiles {
            if force {
                self.batch.generation = self.batch.generation.saturating_add(1);
                self.batch.profiles = quota_refresh_profiles(&controller);
                self.batch.refresh = true;
                self.profiles_tx.send_replace(self.batch.clone());
                // Only this request is a refresh; a later profile-set change is not.
                self.batch.refresh = false;
            }
            crate::server_runtime::republish_quota_profiles(
                &controller,
                &mut self.published,
                &mut self.batch,
                &self.profiles_tx,
            );
            let mut reports = self.reports.lock().expect("quota reports poisoned");
            reports.retain(|id, _| controller.config.enabled_profile(id).is_some());
            self.quotas_tx.send_replace(reports.clone());
            drop(reports);
            self.probing
                .retain(|id| controller.config.enabled_profile(id).is_some());
            self.publish_quotas();
        }
    }
    /// Tell every attached surface what the daemon knows about quota.
    fn publish_quotas(&self) {
        let reports = self.reports.lock().expect("quota reports poisoned").clone();
        self.state.publish_quotas(mj_client::quota::QuotaSnapshot {
            reports,
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
                if outcome.credentials_changed {
                    self.credentials
                        .handle()
                        .sync_profile_now(&outcome.report.profile_id, None);
                }
                self.probing.remove(&outcome.report.profile_id);
                let mut reports = self.reports.lock().expect("quota reports poisoned");
                reports.insert(outcome.report.profile_id.clone(), outcome.report);
                self.quotas_tx.send_replace(reports.clone());
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
                    .notice(&result, profile.map(|p| p.kind), &controller.state)
            };
            if let Some(notice) = notice {
                tracing::warn!(%notice, "credential synchronization notice");
                self.state.push_notice("", notice);
            }
        }
    }
}
