use super::*;
use crate::server::api::DashboardBackend;
use mj_client::daemon::{ResumeCandidates, SessionRecordWithVerdict, SessionTextSearchPage};
use mj_client::http::{ContainerSettingsRequest, ContainerSettingsResponse};
use mj_client::runtime_feed::{RuntimeCursor, SessionTailReply};
use mj_client::session::BoxFuture;
use mj_core::attention::NotificationDismissal;
use mj_core::state::{MaterializedSession, MaterializedSessionSummary, TranscriptItem};
use mj_core::workspace::{ConversationLayout, PaneSize, PaneSizes};

impl DashboardBackend for RuntimeState {
    fn select_provider_profile(
        &self,
        request: mj_client::http::ProviderProfileSelectionRequest,
    ) -> BoxFuture<'_, Result<String>> {
        Box::pin(async move {
            let config = self.owner().controller().config.clone();
            let policy = request
                .target_template_id
                .as_ref()
                .map(|id| {
                    config
                        .targets
                        .get(id)
                        .with_context(|| format!("target {id} is no longer configured"))
                })
                .transpose()?
                .map_or(mj_core::config::ExecutionPolicy::Unconstrained, |target| {
                    target.execution_policy()
                });
            if let Some(id) = &request.profile_id
                && let Some(profile) = config.enabled_profile(id)
                && mj_controller::profile_scheduler::guardian_eligible(profile.kind, policy)
            {
                return Ok(id.clone());
            }
            let backend: Arc<dyn crate::server::api::SubagentBackend> = self
                .quota_report_backend()
                .context("session selection backend is unavailable")?;
            let selected = crate::server::api::resolve_provider_session_selection(
                &backend,
                &config,
                &request.provider,
                policy,
                None,
            )
            .await
            .map_err(|failure| anyhow::anyhow!(failure.message))?;
            // Browsing the remaining pages must not hold quota headroom.
            Ok(selected.profile_id)
        })
    }

    fn dashboard_startup(
        &self,
        client_id: String,
        workspace_id: String,
    ) -> BoxFuture<'_, Result<DashboardStartup>> {
        Box::pin(async move {
            let stored = blocking(move || dashboard_reads::dashboard_stored(&workspace_id)).await?;
            let frame = self
                .runtime_changes_for_subagent_parents(None, false, stored.subagent_parents)
                .await?;
            let mj_client::runtime_feed::RuntimeFrame::Snapshot {
                cursor,
                mut projection,
            } = frame
            else {
                bail!("dashboard bootstrap did not produce a runtime snapshot");
            };
            let records = std::mem::take(&mut projection.records);
            projection.records =
                blocking(move || dashboard_reads::client_read_frontiers(records, &client_id))
                    .await?;
            let queued_prompts = stored
                .queued_prompts
                .into_iter()
                .filter(|(id, _)| projection.records.contains_key(id))
                .collect();
            Ok(DashboardStartup {
                runtime: mj_client::runtime_feed::RuntimeBootstrap {
                    cursor,
                    projection: *projection,
                },
                metadata: stored.metadata,
                pane_sizes: stored.layouts.pane_sizes,
                conversation_layouts: stored.layouts.conversation_layouts,
                queued_prompts,
            })
        })
    }

    fn workspace_layouts(
        &self,
        workspace_ids: Vec<String>,
    ) -> BoxFuture<'_, Result<WorkspaceLayouts>> {
        Box::pin(async move {
            blocking(move || dashboard_reads::workspace_layouts(workspace_ids)).await
        })
    }

    fn stored_session_metadata(&self) -> BoxFuture<'_, Result<StoredSessionMetadata>> {
        Box::pin(async move { blocking(dashboard_reads::stored_session_metadata).await })
    }

    fn stored_projection_tail(
        &self,
        session_id: String,
        items: usize,
    ) -> BoxFuture<'_, Result<Option<MaterializedSession>>> {
        Box::pin(async move {
            let tail = blocking(move || {
                mj_controller::database::load_materialized_projection_tail(&session_id, items)
            })
            .await?;
            Ok(tail.map(|(session, _)| session))
        })
    }

    fn stored_transcript_item(
        &self,
        session_id: String,
        stable_id: String,
    ) -> BoxFuture<'_, Result<Option<TranscriptItem>>> {
        Box::pin(async move {
            let item = blocking(move || {
                mj_controller::database::load_materialized_transcript_item(&session_id, &stable_id)
            })
            .await?;
            Ok(item)
        })
    }

    fn stored_agent_message_through(
        &self,
        session_id: String,
        through: u64,
    ) -> BoxFuture<'_, Result<Option<TranscriptItem>>> {
        Box::pin(async move {
            let item = blocking(move || {
                mj_controller::database::load_materialized_agent_item_through(&session_id, through)
            })
            .await?;
            Ok(item)
        })
    }

    fn stored_session_summary(
        &self,
        session_id: String,
    ) -> BoxFuture<'_, Result<Option<MaterializedSessionSummary>>> {
        Box::pin(async move {
            let summary = blocking(move || {
                mj_controller::database::load_materialized_session_summary(&session_id)
            })
            .await?;
            Ok(summary)
        })
    }

    fn session_tail(
        &self,
        session_id: String,
        cursor: RuntimeCursor,
    ) -> BoxFuture<'_, Result<SessionTailReply>> {
        Box::pin(async move { Ok(RuntimeState::session_tail(self, &session_id, &cursor)) })
    }

    fn resume_candidates(&self) -> BoxFuture<'_, Result<ResumeCandidates>> {
        Box::pin(async move { Ok(RuntimeState::resume_candidates(self)) })
    }

    fn session_record_with_verdict(
        &self,
        session_id: String,
    ) -> BoxFuture<'_, Result<Option<SessionRecordWithVerdict>>> {
        Box::pin(async move {
            let capture = self.runtime_publication()?;
            Ok(capture
                .records
                .get(&session_id)
                .map(|record| SessionRecordWithVerdict {
                    record: record.clone(),
                    verdict: capture.verdicts[&session_id],
                }))
        })
    }

    fn go_startup_session(
        &self,
        workspace_id: String,
        last_session_id: Option<String>,
    ) -> BoxFuture<'_, Result<Option<SessionRecord>>> {
        Box::pin(async move {
            Ok(RuntimeState::go_startup_session(
                self,
                &workspace_id,
                last_session_id.as_deref(),
            ))
        })
    }

    fn session_text_search(&self, q: String) -> BoxFuture<'_, Result<SessionTextSearchPage>> {
        Box::pin(async move { RuntimeState::session_text_search(self, q).await })
    }

    fn dismiss_notification(
        &self,
        session_id: String,
        target: NotificationDismissal,
    ) -> BoxFuture<'_, Result<u64>> {
        Box::pin(async move { RuntimeState::dismiss_notification(self, session_id, target).await })
    }

    fn record_operator_activity(&self) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move {
            RuntimeState::record_operator_activity(self).await;
            Ok(())
        })
    }

    fn set_operator_away(&self, away: bool) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move { RuntimeState::set_operator_away(self, away).await })
    }

    fn attach(&self, client_id: String) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move {
            self.attachments().insert(
                client_id,
                Attachment {
                    renewed: std::time::Instant::now(),
                },
            );
            self.ever_attached.store(true, Ordering::Release);
            Ok(())
        })
    }

    fn detach(&self, client_id: String) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move {
            self.attachments().remove(&client_id);
            Ok(())
        })
    }

    fn save_sidebar_size(&self, size: PaneSize) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move {
            // Hold the configuration owner through persistence and publication:
            // an acknowledged save must be visible to the next dashboard.
            let mut inactivity = self.config_mutation.lock().await;
            let config = blocking(move || {
                Config::update(|config| {
                    config.sidebar_size = size;
                    Ok(())
                })
                .map(|(config, ())| config)
            })
            .await?;
            self.configure_inactivity(&mut inactivity, config.advanced.auto_afk_minutes);
            self.review_config
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clone_from(&config.review);
            self.owner().install_config(config);
            self.publish_revision();
            Ok(())
        })
    }

    fn save_workspace_pane_sizes(
        &self,
        workspace_id: String,
        sizes: PaneSizes,
    ) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move {
            blocking(move || {
                mj_controller::database::save_workspace_pane_sizes(&workspace_id, sizes)
            })
            .await?;
            Ok(())
        })
    }

    fn save_workspace_layout(
        &self,
        workspace_id: String,
        layout: ConversationLayout,
    ) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move {
            blocking(move || mj_controller::database::save_workspace_layout(&workspace_id, layout))
                .await?;
            Ok(())
        })
    }

    fn set_session_title(
        &self,
        session_id: String,
        title: String,
    ) -> BoxFuture<'_, Result<String>> {
        Box::pin(async move {
            let title =
                blocking(move || Controller::load()?.rename_session(&session_id, &title)).await?;
            refresh_runtime_controller(self).await;
            Ok(title)
        })
    }

    fn set_session_notifications_ignored(
        &self,
        session_id: String,
        ignored: bool,
    ) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move {
            blocking(move || {
                Controller::load()?.set_session_notifications_ignored(&session_id, ignored)
            })
            .await?;
            refresh_runtime_controller(self).await;
            Ok(())
        })
    }

    fn set_session_workspace(
        &self,
        session_id: String,
        workspace_id: String,
    ) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move {
            if mj_controller::controller::move_session::move_owns_session(&session_id) {
                return Err(Refusal::precondition(
                    "session is moving; change its workspace after Move finishes",
                )
                .into());
            }
            blocking(move || Controller::load()?.set_session_workspace(&session_id, &workspace_id))
                .await?;
            refresh_runtime_controller(self).await;
            Ok(())
        })
    }

    fn set_session_container_settings(
        self: Arc<Self>,
        session_id: String,
        settings: ContainerSettingsRequest,
    ) -> BoxFuture<'static, Result<ContainerSettingsResponse>> {
        Box::pin(async move {
            let preview = RuntimeState::set_container_settings(
                &self,
                session_id,
                mj_core::state::ContainerSettingsChange {
                    resource_allocation: settings.resource_allocation,
                    mounts: settings.mounts,
                    mount_history: Some(settings.mount_history),
                },
            )
            .await?;
            Ok(ContainerSettingsResponse { preview })
        })
    }
}
