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
    reports: Arc<std::sync::Mutex<QuotaReportMap>>,
    rejected: Arc<std::sync::Mutex<mj_core::credentials::RejectedLogins>>,
    quotas_tx: watch::Sender<QuotaReportMap>,
    profiles_tx: watch::Sender<QuotaRefreshBatch>,
    pub quota_rx: mpsc::Receiver<QuotaUpdate>,
    pub refresh_rx: mpsc::Receiver<()>,
    batch: QuotaRefreshBatch,
    published: BTreeMap<String, mj_core::config::HarnessProfile>,
    catalog_config: Option<mj_core::config::Config>,
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
            .with_rejected_logins(rejected.clone()),
        );
        state.install_report_backend(&backend);
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
            state.clone(),
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
        if force {
            self.batch.generation = self.batch.generation.saturating_add(1);
            self.batch.profiles = quota_refresh_profiles(&controller);
            self.batch.refresh = true;
            self.profiles_tx.send_replace(self.batch.clone());
            // Only this request is a refresh; a later profile-set change is not.
            self.batch.refresh = false;
        }
        // A changed quota setting reaches the refresher even when the profiles
        // did not change.
        crate::server_runtime::republish_quota_profiles(
            &controller,
            &mut self.batch,
            &self.profiles_tx,
        );
        if force || self.published != controller.config.profiles {
            self.published.clone_from(&controller.config.profiles);
        }
        let desired = self
            .batch
            .profiles
            .iter()
            .map(|request| (request.profile_id.clone(), request.cache_identity()))
            .collect::<BTreeMap<_, _>>();
        let changed = {
            let mut reports = self.reports.lock().expect("quota reports poisoned");
            let changed = reconcile_quota_publications(&mut reports, &desired);
            if !changed.is_empty() {
                self.quotas_tx.send_replace(reports.clone());
            }
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
        let snapshot = mj_client::quota::QuotaSnapshot {
            reports: surface_quota_reports(&reports),
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
