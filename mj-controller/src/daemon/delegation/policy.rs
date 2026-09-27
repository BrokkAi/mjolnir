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
        let (profiles_tx, quota_rx) = spawn_quota_refresher();
        let credentials = CredentialSyncCoordinator::spawn_guarded(state.worker_background_gate());
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
            credentials,
            signals: CredentialSyncSignalTracker::default(),
            notices: CredentialSyncNotices::default(),
        };
        policy.sync(false);
        policy
    }
    pub fn sync(&mut self, force: bool) {
        let controller = self
            .state
            .controller
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.catalog.sync(&controller.config);
        self.credentials
            .handle()
            .set_targets(credential_sync_targets(&controller));
        if force || self.published != controller.config.profiles {
            if force {
                self.batch.generation = self.batch.generation.saturating_add(1);
                self.batch.profiles = quota_refresh_profiles(&controller);
                self.profiles_tx.send_replace(self.batch.clone());
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
        }
    }
    pub fn observe(&mut self, id: &str, observation: &DelegationObservation) {
        if let Some(signal) = &observation.credential_signal {
            let controller = self
                .state
                .controller
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(session) = controller.state.sessions.get(id) {
                self.signals
                    .observe(id, &session.last_profile, signal.clone());
            }
        }
    }
    pub fn quota(&mut self, update: QuotaUpdate) {
        if let QuotaUpdate::Report(outcome) = update {
            if outcome.credentials_changed {
                self.credentials
                    .handle()
                    .sync_profile_now(&outcome.report.profile_id, None);
            }
            let mut reports = self.reports.lock().expect("quota reports poisoned");
            reports.insert(outcome.report.profile_id.clone(), outcome.report);
            self.quotas_tx.send_replace(reports.clone());
        }
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
                let controller = self
                    .state
                    .controller
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
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
