use super::*;
use crate::server::api::TerminalOperations;
use mj_client::daemon::{
    CreateSessionRequest, InterruptAllResult, LifecycleReceipt, RegisteredSession,
    ResumeSessionRequest, SessionLifecycleRequest, StartupPromptOutcome,
};
use mj_client::http::{
    AbandonedPromptRequest, LifecycleCompletion, MoveSourcesRequest, RestartSessionRequest,
    SessionCommandRequest, StartupPromptOutcomesRequest, StartupPromptRequest,
    SuspendPreflightRequest,
};
use mj_client::session::BoxFuture;
use mj_core::elicitation::ElicitationResponse;
use mj_core::move_workspace::RetainedMoveSource;
use mj_core::state::{MoveOutcome, MoveSessionRequest};
use mj_core::suspend_consent::{SuspendConsent, SuspendPreflight};
use std::collections::BTreeMap;

use mj_core::targets::CancellableProcessExecutor;
use std::sync::atomic::AtomicBool;

impl TerminalOperations for RuntimeState {
    fn terminal_interrupt_all(
        self: Arc<Self>,
        session_id: String,
    ) -> BoxFuture<'static, Result<InterruptAllResult>> {
        Box::pin(async move { self.interrupt_all(&session_id).await })
    }

    fn terminal_submit_session_command(
        self: Arc<Self>,
        session_id: String,
        request: SessionCommandRequest,
    ) -> BoxFuture<'static, Result<u64>> {
        Box::pin(async move {
            let SessionCommandRequest {
                command_id,
                command,
                inherited_draft,
            } = request;
            let history = if let RelayCommand::Prompt { prompt } = &command {
                let values = serde_json::to_value(prompt)?;
                let values = values
                    .as_array()
                    .context("serialized prompt content is not an array")?;
                Some(mj_core::transcript::materialized_content_text(values))
            } else {
                None
            };
            // A completed restore is ready before the background target feed
            // has necessarily installed its new actor. Match local control
            // surfaces by awaiting that bounded handoff, not losing the first
            // command immediately after Move/Resume.
            let session = self
                .session_manager
                .wait_for_session(&session_id, Duration::from_secs(5))
                .await?;
            let session_id = session.session_id().to_owned();
            // History is written before the session sees the prompt; see
            // `record_user_prompt`.
            if let Some(text) = history {
                let history_id = session_id.clone();
                let history_command = command_id.clone();
                if let Err(error) = blocking(move || {
                    mj_controller::database::record_user_prompt(
                        &history_id,
                        &history_command,
                        &text,
                    )
                })
                .await
                {
                    tracing::warn!(%error, "prompt history could not be stored");
                }
            }
            let ordinal = session.submit(command_id, command).await?;
            if let Some(expected) = inherited_draft {
                let persisted_id = session_id.clone();
                let persisted_expected = expected.clone();
                blocking(move || {
                    mj_controller::database::clear_session_draft_input_if_matches(
                        &persisted_id,
                        &persisted_expected,
                    )
                })
                .await?;
                self.owner().edit_sessions(|sessions| {
                    if let Some(record) = sessions.get_mut(&session_id)
                        && record.draft_input == expected
                    {
                        record.draft_input.clear();
                    }
                });
                self.publish_revision();
            }
            Ok(ordinal)
        })
    }

    fn terminal_queue_startup_prompt(
        self: Arc<Self>,
        session_id: String,
        request: StartupPromptRequest,
        cancellation: tokio_util::sync::CancellationToken,
    ) -> BoxFuture<'static, Result<()>> {
        Box::pin(async move {
            let StartupPromptRequest {
                command_id,
                text,
                prompt,
                encoded_draft,
                move_operation_id,
                inherited_draft,
            } = request;
            ensure!(
                !command_id.is_empty(),
                "a queued startup prompt needs a command ID"
            );
            ensure!(
                !prompt.is_empty(),
                "a queued startup prompt needs content to send"
            );
            // Validate references before persistence: raw image bytes never belong
            // in a durable command, including while the destination is unknown.
            mj_core::attachment::references(&prompt)?;
            mj_core::draft::validate_reference_draft(&encoded_draft)?;
            if let Some(draft) = &inherited_draft {
                mj_core::draft::validate_reference_draft(draft)?;
            }
            for block in &prompt {
                match block {
                    ContentBlock::Text(_) => {}
                    ContentBlock::Image(image) => {
                        ensure!(
                            mj_core::attachment::image_reference(image)?.is_some(),
                            "queued images must be installed attachments"
                        );
                    }
                    _ => bail!("startup prompts support text and installed images"),
                }
            }
            let command = RelayCommand::Prompt { prompt };
            mj_core::relay::ensure_serialized_budget(
                &command,
                mj_core::relay::RELAY_COMMAND_BYTE_BUDGET,
                "startup prompt",
            )?;
            let RelayCommand::Prompt { prompt } = command else {
                unreachable!("just constructed a prompt");
            };
            self.queue_startup_steps_with_ids(
                &session_id,
                vec![(
                    command_id,
                    StartupStep::Prompt {
                        text,
                        prompt: Some(prompt),
                        encoded_draft: Some(encoded_draft),
                        move_operation_id,
                        inherited_draft,
                    },
                )],
                None,
                &cancellation,
            )
            .await
        })
    }

    fn terminal_withdraw_startup_prompt(
        self: Arc<Self>,
        session_id: String,
        command_id: String,
    ) -> BoxFuture<'static, Result<bool>> {
        Box::pin(async move { self.withdraw_startup_prompt(&session_id, &command_id).await })
    }

    fn terminal_startup_prompt_outcomes(
        self: Arc<Self>,
        session_id: String,
        request: StartupPromptOutcomesRequest,
    ) -> BoxFuture<'static, Result<BTreeMap<String, StartupPromptOutcome>>> {
        Box::pin(async move {
            let StartupPromptOutcomesRequest { command_ids } = request;
            mj_core::runtime::spawn_blocking(move || {
                mj_controller::database::startup_prompt_outcomes(&session_id, &command_ids)
            })
            .await
            .context("startup prompt outcomes task panicked")?
        })
    }

    fn terminal_sync_session(
        self: Arc<Self>,
        session_id: String,
    ) -> BoxFuture<'static, Result<()>> {
        Box::pin(async move {
            self.session_manager
                .session(session_id)
                .await?
                .sync_now()
                .await?;
            Ok(())
        })
    }

    fn terminal_respond_elicitation(
        self: Arc<Self>,
        session_id: String,
        elicitation_id: String,
        request: ElicitationResponse,
    ) -> BoxFuture<'static, Result<()>> {
        Box::pin(async move {
            let target = self
                .review_host()
                .question_target(&session_id, &elicitation_id)
                .await;
            let (session_id, elicitation_id) = match target {
                Some(target) => target,
                None if mj_client::review::parse_reviewer_question_id(&elicitation_id)
                    .is_some() =>
                {
                    anyhow::bail!("that reviewer question is no longer active")
                }
                None => (session_id, elicitation_id),
            };
            self.session_manager
                .session(session_id)
                .await?
                .respond_elicitation(elicitation_id, request)
                .await?;
            Ok(())
        })
    }

    fn terminal_record_abandoned_prompt(
        self: Arc<Self>,
        session_id: String,
        request: AbandonedPromptRequest,
    ) -> BoxFuture<'static, Result<()>> {
        Box::pin(async move {
            let AbandonedPromptRequest { bundle_id, text } = request;
            let warning_session_id = session_id.clone();
            if let Err(error) = blocking(move || {
                mj_controller::database::record_abandoned_prompt(&session_id, &bundle_id, &text)
            })
            .await
            {
                tracing::warn!(
                    session_id = warning_session_id,
                    error = format!("{error:#}"),
                    "abandoned prompt history could not be stored"
                );
            }
            Ok(())
        })
    }

    fn terminal_start_session_lifecycle(
        self: Arc<Self>,
        request: SessionLifecycleRequest,
    ) -> BoxFuture<'static, Result<LifecycleReceipt>> {
        Box::pin(async move { self.start_session_lifecycle(request).await })
    }

    fn terminal_wait_session_lifecycle(
        self: Arc<Self>,
        request: LifecycleReceipt,
    ) -> BoxFuture<'static, Result<LifecycleCompletion>> {
        Box::pin(async move { self.wait_session_lifecycle(request).await })
    }

    fn terminal_suspend_preflight(
        self: Arc<Self>,
        session_id: String,
        request: SuspendPreflightRequest,
    ) -> BoxFuture<'static, Result<SuspendPreflight>> {
        Box::pin(async move {
            let executor = CancellableProcessExecutor::new(Arc::new(AtomicBool::new(false)));
            let _cancel = executor.cancel_on_drop();
            mj_core::runtime::spawn_blocking(move || {
                Controller::load()?.suspend_preflight(
                    &session_id,
                    request.dry_run,
                    request.without_checkpoint,
                    &executor,
                )
            })
            .await
            .context("suspend preflight task panicked")?
        })
    }

    fn terminal_suspend_session_with_consent(
        self: Arc<Self>,
        session_id: String,
        request: SuspendConsent,
    ) -> BoxFuture<'static, Result<()>> {
        Box::pin(async move { self.suspend_session_with_consent(session_id, request).await })
    }

    fn terminal_restart_session_with_ack(
        self: Arc<Self>,
        session_id: String,
        request: RestartSessionRequest,
    ) -> BoxFuture<'static, Result<()>> {
        Box::pin(async move {
            self.restart_session(session_id, request.acknowledge_container_recreation)
                .await
        })
    }

    fn terminal_start_create_session(
        self: Arc<Self>,
        request: CreateSessionRequest,
    ) -> BoxFuture<'static, Result<RegisteredSession>> {
        Box::pin(async move { self.start_create_session(request).await })
    }

    fn terminal_wait_create_session(
        self: Arc<Self>,
        session_id: String,
    ) -> BoxFuture<'static, Result<()>> {
        Box::pin(async move {
            match self.wait_create_outcome(&session_id).await {
                super::create::CreateWait::Finished => Ok(()),
                super::create::CreateWait::Failed(error) => Err(error),
            }
        })
    }

    fn terminal_resume_session(
        self: Arc<Self>,
        request: ResumeSessionRequest,
    ) -> BoxFuture<'static, Result<()>> {
        Box::pin(async move { self.resume_session(request).await })
    }

    fn terminal_move_session(
        self: Arc<Self>,
        request: MoveSessionRequest,
    ) -> BoxFuture<'static, Result<MoveOutcome>> {
        Box::pin(async move { self.move_session(request).await })
    }

    fn terminal_move_sources(
        self: Arc<Self>,
        session_id: String,
        request: MoveSourcesRequest,
    ) -> BoxFuture<'static, Result<Vec<RetainedMoveSource>>> {
        Box::pin(async move {
            let MoveSourcesRequest {
                cleanup_operation_id,
            } = request;
            let admission = if cleanup_operation_id.is_some() {
                Some(
                    self.session_gate
                        .enter_foreground(
                            &session_id,
                            "retained Move source cleanup",
                            mj_controller::recovery_gate::LifecycleOrigin::Person,
                            &AtomicBool::new(false),
                            &ProcessExecutor,
                        )
                        .await?,
                )
            } else {
                None
            };
            let owner = admission
                .as_ref()
                .map(|admission| {
                    mj_controller::worker_lifecycle::WorkerPermit::acquire(&session_id, admission)
                })
                .transpose()?;
            mj_core::runtime::spawn_blocking(move || {
                // Cleanup keeps session ownership even if the HTTP observer leaves.
                let _admission = admission;
                let cleanup = || {
                    let _guard =
                        mj_controller::controller::move_session::MoveMutationGuard::reserve(
                            &session_id,
                        )?;
                    if let Some(operation_id) = cleanup_operation_id {
                        Controller::load()?.cleanup_retained_move_source(
                            &session_id,
                            &operation_id,
                            &ProcessExecutor,
                        )?;
                    }
                    mj_controller::database::retained_move_sources(&session_id)
                };
                match owner {
                    Some(owner) => owner.scope_blocking(cleanup),
                    None => cleanup(),
                }
            })
            .await
            .context("move sources task panicked")?
        })
    }

    fn terminal_destroy_stopped_session(
        self: Arc<Self>,
        session_id: String,
    ) -> BoxFuture<'static, Result<()>> {
        Box::pin(async move {
            let started = self
                .start_destroy_stopped_session(session_id, BranchDisposition::Keep)
                .await?;
            let _admission = started.destroy_admission;
            RuntimeState::wait_lifecycle_result(started.result).await?;
            Ok(())
        })
    }

    fn terminal_force_destroy_session(
        self: Arc<Self>,
        session_id: String,
    ) -> BoxFuture<'static, Result<()>> {
        Box::pin(async move {
            self.force_destroy_session(session_id, BranchDisposition::Keep)
                .await
        })
    }

    fn terminal_cancel_lifecycle(
        self: Arc<Self>,
        session_id: String,
    ) -> BoxFuture<'static, Result<()>> {
        Box::pin(async move {
            mj_core::runtime::spawn_blocking({
                let session_id = session_id.clone();
                move || mj_controller::database::request_move_cancellation(&session_id)
            })
            .await
            .context("move cancellation task panicked")??;
            self.cancel_lifecycle_with_intent(&session_id).await?;
            Ok(())
        })
    }

    fn terminal_transcription_available(
        self: Arc<Self>,
        preferred_profile: String,
    ) -> BoxFuture<'static, Result<bool>> {
        Box::pin(async move {
            let config = self.with_config(Clone::clone);
            mj_core::runtime::spawn_blocking(move || {
                super::actions::transcription_auth(&config, &preferred_profile).is_some()
            })
            .await
            .context("dictation credentials task panicked")
        })
    }

    fn terminal_transcribe_audio(
        self: Arc<Self>,
        preferred_profile: String,
        bytes: Vec<u8>,
    ) -> BoxFuture<'static, Result<String>> {
        Box::pin(async move {
            let config = self.with_config(Clone::clone);
            let auth = mj_core::runtime::spawn_blocking(move || {
                super::actions::transcription_auth(&config, &preferred_profile)
            })
            .await
            .context("dictation credentials task panicked")?
            .context(
                "voice transcription requires a signed-in ChatGPT Codex profile on the daemon",
            )?;
            let client = anvil_client::codex_client::CodexClient::with_auth_path(auth);
            let response = client
                .transcribe(anvil_client::transcribe::TranscribeRequest::new(
                    bytes.into(),
                    "audio.wav",
                    "audio/wav",
                ))
                .await
                .context("daemon transcribe captured audio")?;
            let text = response.text.trim().to_owned();
            ensure!(!text.is_empty(), "daemon transcription returned no text");
            Ok(text)
        })
    }

    fn terminal_refresh_imported_session(self: Arc<Self>) -> BoxFuture<'static, ()> {
        Box::pin(async move { refresh_runtime_controller(&self).await })
    }
}
