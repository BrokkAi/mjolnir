use super::*;

impl RuntimeState {
    pub async fn reload_controller(&self) -> Result<()> {
        // Serialize installs so an earlier phone publication cannot overwrite
        // a later completed lifecycle with the controller snapshot it loaded.
        let _mutation = self.config_mutation.lock().await;
        let controller_loader = self.controller_loader;
        let controller = tokio::task::spawn_blocking(controller_loader)
            .await
            .context("daemon controller reload task panicked")??;
        let session_count = controller.state.sessions.len();
        *self
            .controller
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = controller;
        let revision = self.publish_revision();
        tracing::debug!(revision, session_count, "daemon controller state reloaded");
        Ok(())
    }

    /// Definitive missing-target evidence belongs to the daemon, including
    /// when no terminal is attached. Generic connection failures stay transient.
    pub(super) fn missing_target_record(
        &self,
        session_id: &str,
        view: &ManagedSessionView,
    ) -> Option<(String, String)> {
        let Some(ViewError::TargetMissing(detail)) = &view.error else {
            return None;
        };
        if view.connected {
            return None;
        }
        if self
            .lifecycle
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(session_id)
            .is_some_and(|active| active.result.borrow().is_none())
        {
            return None;
        }
        let controller = self
            .controller
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let session = controller.state.sessions.get(session_id)?;
        if !matches!(
            session.state,
            SessionState::Running | SessionState::Disconnected
        ) {
            return None;
        }
        Some((detail.clone(), session.updated_at.clone()))
    }

    pub(super) async fn persist_missing_target(
        self: &Arc<Self>,
        session_id: &str,
        detail: String,
        observed_updated_at: String,
    ) -> Result<()> {
        let changed = blocking({
            let session_id = session_id.to_owned();
            let detail = detail.clone();
            move || {
                crate::database::mark_session_target_missing_if_current(
                    &session_id,
                    &detail,
                    &chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
                    &observed_updated_at,
                )
            }
        })
        .await?;
        if changed.is_some() {
            self.reload_controller().await?;
            // A lost session has no checkpoint and no target, so its record
            // offers nothing but Destroy. Report it once and discard it
            // instead of leaving a tombstone behind.
            if changed == Some(SessionState::Lost) {
                self.discard_lost_session(session_id.to_owned()).await;
            } else {
                self.push_notice(session_id, detail);
            }
        }
        Ok(())
    }

    /// One pass of the harness readiness wait: which live sessions have run
    /// out of time to become usable.
    ///
    /// Locks are taken one at a time and nothing here touches the store, so
    /// this belongs on the daemon's background tick. The write it leads to
    /// does not; see [`Self::fail_unready_session`].
    pub(super) fn sessions_without_a_usable_harness(&self) -> Vec<UnreadySession> {
        let busy = {
            let lifecycle = self
                .lifecycle
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            lifecycle
                .iter()
                .filter(|(_, active)| active.result.borrow().is_none())
                .map(|(session_id, _)| session_id.clone())
                .collect::<std::collections::BTreeSet<_>>()
        };
        let live = {
            let controller = self
                .controller
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            controller
                .state
                .sessions
                .values()
                .filter(|record| record.state == SessionState::Running)
                .filter(|record| !busy.contains(&record.id))
                .map(|record| (record.id.clone(), record.updated_at.clone()))
                .collect::<Vec<_>>()
        };
        let now = chrono::Utc::now();
        let observations = {
            let sessions = self.sessions.lock().unwrap_or_else(PoisonError::into_inner);
            live.into_iter()
                .filter(|(session_id, _)| !self.close_is_requested(session_id))
                .map(|(session_id, updated_at)| {
                    let view = sessions.get(&session_id);
                    ReadinessObservation {
                        harness_ready: view.is_some_and(|view| {
                            view.operational.as_ref().is_some_and(
                                mj_core::relay::RelayOperationalState::native_session_is_ready,
                            )
                        }),
                        record_age: record_age(&updated_at, now),
                        updated_at,
                        detail: view
                            .and_then(|view| view.error.as_ref())
                            .map(|error| error.detail().to_owned()),
                        session_id,
                    }
                })
                .collect::<Vec<_>>()
        };
        self.harness_readiness
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .observe(std::time::Instant::now(), observations)
    }

    /// Fail one session whose harness never became usable, so it stops looking
    /// like a session a driver can wait for.
    pub(super) async fn fail_unready_session(&self, unready: UnreadySession) {
        let cause = unready.cause();
        let session_id = unready.session_id.clone();
        tracing::warn!(%session_id, waited_seconds = unready.waited.as_secs(), "{cause}");
        let applied = blocking({
            let session_id = session_id.clone();
            let cause = cause.clone();
            move || {
                let mut controller = Controller::load()?;
                controller.fail_unready_session(&session_id, &cause, &unready.observed_updated_at)
            }
        })
        .await;
        match applied {
            Ok(true) => {
                if let Err(error) = self.reload_controller().await {
                    tracing::warn!(%session_id, error = format!("{error:#}"), "could not reload state after failing an unready session");
                }
                self.push_notice(&session_id, cause);
                self.publish_revision();
            }
            Ok(false) => {}
            Err(error) => tracing::warn!(
                %session_id,
                error = format!("{error:#}"),
                "could not record that a session's harness never became usable"
            ),
        }
    }

    pub(super) async fn publish_session(
        &self,
        session_id: String,
        view: ManagedSessionView,
    ) -> Result<()> {
        let connected = view.connected;
        let has_snapshot = view.snapshot.is_some();
        tracing::debug!(
            %session_id,
            connected,
            has_snapshot,
            "daemon received a session view"
        );
        if let Some(snapshot) = view.snapshot.as_ref() {
            let controller = self
                .controller
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            if let Some(session) = controller.state.sessions.get(&session_id).cloned() {
                // An upgrade only ever runs on a quiet session, and a session
                // in a turn publishes a view every 150 ms. Skipping those here
                // keeps the config clone off the streaming path; the
                // coordinator still decides, from `quiet`, whether to act.
                let quiet =
                    view.connected && snapshot.operational.safe_to_replace(session.harness_kind);
                if quiet {
                    self.worker_upgrade_observer
                        .observe(WorkerUpgradeObservation {
                            session: session.clone(),
                            config: controller.config.clone(),
                            worker_build: snapshot.worker_build.clone(),
                            quiet,
                        });
                }
                self.recovery_observer.observe(RecoveryObservation {
                    checkpoint_safe: snapshot
                        .operational
                        .safe_for_checkpoint(session.harness_kind),
                    session,
                    config: controller.config.clone(),
                    latest_completed_turn_ordinal: snapshot.latest_completed_turn_ordinal(),
                    execution: snapshot.materialized.execution,
                });
            }
        }
        self.sessions
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(
                session_id.clone(),
                RuntimeSessionView::from_managed(session_id, view),
            );
        reach_test_hook("relay_projection_before_revision_publication").await?;
        self.publish_revision();
        Ok(())
    }

    pub(super) async fn runtime_snapshot(
        &self,
        workspace_id: &str,
        after_revision: u64,
        all_workspaces: bool,
    ) -> Result<RuntimeSnapshot> {
        let mut revisions = self.revisions.subscribe();
        if *revisions.borrow_and_update() <= after_revision {
            let _ = tokio::time::timeout(Duration::from_secs(30), revisions.changed()).await;
        }
        let revision = self.revisions.current();
        let moves = blocking(crate::database::load_move_operations).await?;
        let workspace_names = self
            .workspaces()
            .borrow()
            .clone()
            .into_iter()
            .map(|workspace| (workspace.id, workspace.name))
            .collect();
        let session_ids = if all_workspaces {
            self.controller
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .state
                .sessions
                .keys()
                .cloned()
                .collect::<BTreeSet<_>>()
        } else {
            let workspace_id = workspace_id.to_owned();
            blocking(move || crate::database::session_ids_for_workspace(&workspace_id))
                .await?
                .into_iter()
                .collect()
        };
        let native_owners = session_ids.clone();
        let native_agents = blocking(move || {
            let mut agents = Vec::new();
            for owner in native_owners {
                agents.extend(crate::database::load_native_agent_summaries(&owner)?);
            }
            Ok(agents)
        })
        .await?;
        let sessions = self
            .sessions
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .filter(|(session_id, _)| session_ids.contains(*session_id))
            .map(|(_, view)| view.clone())
            .collect();
        // Match the controller -> lifecycle lock order used by worker polling.
        // Completion reloads records before publishing its result, so holding
        // this guard prevents an absent operation paired with older records.
        let controller = self
            .controller
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let lifecycles = self
            .lifecycle
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .filter(|(session_id, active)| {
                (all_workspaces
                    || session_ids.contains(*session_id)
                    || active.resume_workspace_id.as_deref() == Some(workspace_id))
                    && active.is_visible()
            })
            .map(|(session_id, active)| RuntimeLifecycleView {
                operation_id: active.operation_id.clone(),
                cancellable: active.is_cancellable()
                    && lifecycle_cancellable(
                        active.kind,
                        durable_session_state(&controller, session_id),
                    ),
                session_id: session_id.clone(),
                kind: active.kind.into(),
                started_at_epoch_seconds: active.started_at_epoch_seconds,
                active_stages: active
                    .active_stages
                    .iter()
                    .map(|(stage, (_, started_at))| (*stage, *started_at))
                    .collect(),
                resume_destination: active.resume_destination.clone(),
                notice: active.notice.clone(),
            })
            .collect();
        let reviews = self
            .review_host
            .views()
            .into_iter()
            .filter(|review| session_ids.contains(&review.session_id))
            .collect();
        let notices = self
            .notices
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .filter(|notice| notice_reaches_workspace(notice, &session_ids))
            .cloned()
            .collect();
        let records = runtime_records_for_workspace(&controller, &session_ids);
        let subagents = runtime_subagents_for_workspace(&controller, &records);
        Ok(RuntimeSnapshot {
            native_agents,
            workspace_names,
            moves: moves
                .into_iter()
                .filter(|operation| session_ids.contains(&operation.selection.session_id))
                .collect(),
            revision,
            config: controller.config.clone(),
            records,
            sessions,
            lifecycles,
            reviews,
            notices,
            subagents,
        })
    }
}

/// Whether a notice belongs in one workspace's snapshot.
///
/// A notice usually belongs to a session, and only the workspace holding that
/// session shows it. An empty session id marks a notice the daemon owns
/// instead, such as a background container image download; there is no
/// workspace to attach it to, and every workspace wants to see it.
pub(super) fn notice_reaches_workspace(
    notice: &RuntimeNotice,
    session_ids: &BTreeSet<String>,
) -> bool {
    notice.session_id.is_empty() || session_ids.contains(&notice.session_id)
}

/// How long ago a durable record was written, when its timestamp can be read.
///
/// A record written in the future — a clock that moved backwards, a store
/// written by another host — reports no age rather than a wrapped one, so the
/// readiness wait leaves those sessions alone.
fn record_age(updated_at: &str, now: chrono::DateTime<chrono::Utc>) -> Option<Duration> {
    let written = chrono::DateTime::parse_from_rfc3339(updated_at).ok()?;
    now.signed_duration_since(written.with_timezone(&chrono::Utc))
        .to_std()
        .ok()
}
