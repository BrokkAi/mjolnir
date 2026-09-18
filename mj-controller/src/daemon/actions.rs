use super::*;

pub(super) async fn handle_action(
    action: DaemonAction,
    metadata: &DaemonMetadata,
    state: &Arc<RuntimeState>,
    cancellation: &CancellationToken,
) -> Result<DaemonReply> {
    match action {
        DaemonAction::Ping => Ok(DaemonReply::Pong),
        DaemonAction::Status => {
            state.prune_dead_clients();
            Ok(DaemonReply::Status(DaemonStatus {
                pid: metadata.pid,
                started_at: metadata.started_at.clone(),
                build_version: metadata.build_version.clone(),
                attached_clients: state.attachments().len(),
                phone_status: state.phone_status(),
            }))
        }
        DaemonAction::WebViewerAccess => {
            Ok(DaemonReply::WebViewerAccess(state.web_viewer.access()))
        }
        DaemonAction::RecoverWebViewer(action) => {
            state.web_viewer.recover(action)?;
            Ok(DaemonReply::Done)
        }
        DaemonAction::InspectWebListener => {
            let address = state.web_viewer.conflict_address()?;
            let processes = blocking(move || crate::web_viewer::inspect_listener(address)).await?;
            ensure!(
                state.web_viewer.conflict_address()? == address,
                "The viewer address changed. Inspect again."
            );
            Ok(DaemonReply::WebListeners(processes))
        }
        DaemonAction::ListWorkspaces => {
            state.prune_dead_clients();
            let workspaces = blocking(crate::database::list_workspaces).await?;
            Ok(DaemonReply::Workspaces(
                workspaces
                    .into_iter()
                    .map(|workspace| WorkspaceListing { workspace })
                    .collect(),
            ))
        }
        DaemonAction::CreateWorkspace { name } => {
            // Two setup selectors can both have observed an empty workspace
            // list. The daemon operation is create-or-get so both attach to
            // the same normalized name instead of leaking a SQLite conflict.
            let workspace =
                blocking(move || crate::database::create_or_get_workspace(&name)).await?;
            refresh_runtime_workspaces(state).await?;
            Ok(DaemonReply::Workspace(workspace))
        }
        DaemonAction::RenameWorkspace { workspace_id, name } => {
            blocking(move || crate::database::rename_workspace(&workspace_id, &name)).await?;
            refresh_runtime_workspaces(state).await?;
            Ok(DaemonReply::Done)
        }
        DaemonAction::TouchWorkspace { workspace_id } => {
            blocking(move || crate::database::touch_workspace(&workspace_id)).await?;
            refresh_runtime_workspaces(state).await?;
            Ok(DaemonReply::Done)
        }
        DaemonAction::DeleteWorkspace { workspace_id } => {
            ensure!(
                !state.workspace_has_active_resume(&workspace_id),
                "workspace has a session resume in progress"
            );
            blocking(move || crate::database::delete_workspace(&workspace_id)).await?;
            refresh_runtime_workspaces(state).await?;
            Ok(DaemonReply::Done)
        }
        DaemonAction::Attach { client_id, pid } => {
            state.attachments().insert(client_id, Attachment { pid });
            state.ever_attached.store(true, Ordering::Release);
            Ok(DaemonReply::Done)
        }
        DaemonAction::Detach { client_id } => {
            state.attachments().remove(&client_id);
            Ok(DaemonReply::Done)
        }
        DaemonAction::PersistReadReceipt {
            client_id,
            workspace_id,
            session_id,
            through,
        } => {
            let frontier = blocking(move || {
                crate::database::persist_read_receipt(
                    &client_id,
                    &workspace_id,
                    &session_id,
                    through,
                )
            })
            .await?;
            Ok(DaemonReply::Ordinal(frontier))
        }
        DaemonAction::PersistDetachedSessionState {
            client_id,
            workspace_id,
            session_id,
            through,
            owner_pid,
            draft,
        } => {
            blocking(move || {
                let receipt = crate::database::persist_read_receipt(
                    &client_id,
                    &workspace_id,
                    &session_id,
                    through,
                )
                .map(|_| ());
                // Draft durability is independent of receipt validity. A
                // stale or malformed receipt must never discard typed text.
                let saved_draft = crate::database::save_detached_session_draft(
                    &workspace_id,
                    &session_id,
                    &client_id,
                    owner_pid,
                    draft,
                )
                .map(|_| ());
                receipt.and(saved_draft)
            })
            .await?;
            Ok(DaemonReply::Done)
        }
        DaemonAction::SaveActiveReview { session_id, review } => {
            blocking(move || crate::database::save_active_review(&session_id, &review)).await?;
            Ok(DaemonReply::Done)
        }
        DaemonAction::ClearActiveReview { session_id } => {
            blocking(move || crate::database::clear_active_review(&session_id)).await?;
            Ok(DaemonReply::Done)
        }
        DaemonAction::RememberReviewerSelection {
            workspace_id,
            selection,
        } => {
            blocking(move || {
                crate::database::remember_reviewer_selection(&workspace_id, &selection)
            })
            .await?;
            Ok(DaemonReply::Done)
        }
        DaemonAction::SaveWorkspacePaneSizes {
            workspace_id,
            sizes,
        } => {
            blocking(move || crate::database::save_workspace_pane_sizes(&workspace_id, sizes))
                .await?;
            Ok(DaemonReply::Done)
        }
        DaemonAction::SaveWorkspaceLayout {
            workspace_id,
            layout,
        } => {
            blocking(move || crate::database::save_workspace_layout(&workspace_id, layout)).await?;
            Ok(DaemonReply::Done)
        }
        DaemonAction::PersistImportedSession { session } => {
            blocking(move || crate::import::persist_imported_session_locally(&session)).await?;
            refresh_runtime_controller(state).await;
            Ok(DaemonReply::Done)
        }
        DaemonAction::SetSessionTitle { session_id, title } => {
            let title =
                blocking(move || Controller::load()?.rename_session(&session_id, &title)).await?;
            refresh_runtime_controller(state).await;
            Ok(DaemonReply::Text(title))
        }
        DaemonAction::SetSessionContainerSettings {
            session_id,
            cpus,
            memory,
            mounts,
            mount_history,
        } => {
            ensure!(
                !crate::controller::move_session::move_owns_session(&session_id),
                "session is moving; change container settings after Move finishes"
            );
            blocking(move || {
                Controller::load()?.update_session_container_settings(
                    &session_id,
                    cpus,
                    memory,
                    mounts,
                    mount_history,
                )
            })
            .await?;
            refresh_runtime_controller(state).await;
            Ok(DaemonReply::Done)
        }
        DaemonAction::SetSessionAcpTitle { session_id, title } => {
            blocking(move || crate::database::set_session_acp_title(&session_id, title.as_deref()))
                .await?;
            refresh_runtime_controller(state).await;
            Ok(DaemonReply::Done)
        }
        DaemonAction::MarkSessionTargetMissing {
            session_id,
            detail,
            updated_at,
        } => {
            let changed = blocking(move || {
                crate::database::mark_session_target_missing(&session_id, &detail, &updated_at)
            })
            .await?;
            refresh_runtime_controller(state).await;
            Ok(DaemonReply::OptionalSessionState(changed))
        }
        DaemonAction::CheckpointSession { session_id } => Ok(DaemonReply::Checkpoint(
            state.checkpoint_session_now(&session_id).await?,
        )),
        DaemonAction::WikiSearch { query, limit } => Ok(DaemonReply::WikiRows(
            state.wiki_search(query, limit).await?,
        )),
        DaemonAction::WikiBrief { wiki_id, max_chars } => {
            let markdown = state
                .wiki_brief(wiki_id.clone(), max_chars)
                .await?
                .with_context(|| format!("no indexed session {wiki_id}"))?;
            Ok(DaemonReply::Text(markdown))
        }
        DaemonAction::WikiHits {
            wiki_id,
            query,
            context_messages,
            per_message_chars,
        } => Ok(DaemonReply::WikiHits(
            state
                .wiki_hits(wiki_id, query, context_messages, per_message_chars)
                .await?,
        )),
        DaemonAction::WikiRestore(request) => {
            let wiki_id = request.wiki_id.clone();
            let registered = state
                .restore_wiki_session(request, cancellation)
                .await?
                .with_context(|| format!("no indexed session {wiki_id}"))?;
            Ok(DaemonReply::RegisteredSession(Box::new(registered)))
        }
        DaemonAction::ScanRecovery { all_instances } => {
            let scan = blocking(move || {
                Ok(Controller::load()?.scan_orphan_workers(&ProcessExecutor, all_instances))
            })
            .await?;
            Ok(DaemonReply::RecoveryScan(scan))
        }
        DaemonAction::AdoptRecovery {
            session_id,
            target_id,
            profile,
            bundle,
            all_instances,
        } => {
            ensure_no_active_lifecycle(state)?;
            let mut controller = blocking(Controller::load).await?;
            controller
                .adopt_orphan_worker(
                    &session_id,
                    &target_id,
                    profile.as_deref(),
                    bundle.as_deref(),
                    all_instances,
                    &ProcessExecutor,
                )
                .await?;
            refresh_runtime_controller(state).await;
            Ok(DaemonReply::Done)
        }
        DaemonAction::DestroyRecovery {
            session_id,
            target_id,
            confirmation,
            all_instances,
        } => {
            ensure_no_active_lifecycle(state)?;
            blocking(move || {
                Controller::load()?.destroy_orphan_worker(
                    &session_id,
                    &target_id,
                    &confirmation,
                    all_instances,
                    &ProcessExecutor,
                )
            })
            .await?;
            Ok(DaemonReply::Done)
        }
        DaemonAction::Snapshot { workspace_id } => {
            let snapshot = blocking(move || workspace_snapshot(&workspace_id)).await?;
            Ok(DaemonReply::Snapshot(snapshot))
        }
        DaemonAction::RuntimeSnapshot {
            workspace_id,
            after_revision,
            all_workspaces,
        } => Ok(DaemonReply::RuntimeSnapshot(Box::new(
            state
                .runtime_snapshot(&workspace_id, after_revision, all_workspaces)
                .await?,
        ))),
        DaemonAction::RenameProfile { old_id, new_id } => {
            let _config_mutation = state.config_mutation.lock().await;
            ensure_no_active_lifecycle(state)?;
            let controller = blocking(move || {
                let mut controller = Controller::load()?;
                controller.rename_profile_id(&old_id, &new_id)?;
                Ok(controller)
            })
            .await?;
            install_renamed_controller(state, controller);
            Ok(DaemonReply::Done)
        }
        DaemonAction::RenameTarget { old_id, new_id } => {
            let _config_mutation = state.config_mutation.lock().await;
            ensure_no_active_lifecycle(state)?;
            let controller = blocking(move || {
                let mut controller = Controller::load()?;
                controller.rename_target_id(&old_id, &new_id)?;
                Ok(controller)
            })
            .await?;
            install_renamed_controller(state, controller);
            Ok(DaemonReply::Done)
        }
        DaemonAction::SubmitSessionCommand {
            inherited_draft,
            session_id,
            command_id,
            command,
        } => {
            let history = if let RelayCommand::Prompt { prompt } = &command {
                let values = serde_json::to_value(prompt)?;
                let values = values
                    .as_array()
                    .context("serialized prompt content is not an array")?;
                let text = mj_core::transcript::materialized_content_text(values);
                let bundle_id = state
                    .controller
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .state
                    .sessions
                    .get(&session_id)
                    .with_context(|| format!("unknown session {session_id}"))?
                    .bundle_id
                    .clone();
                Some((bundle_id, text))
            } else {
                None
            };
            // A completed restore is ready before the background target feed
            // has necessarily installed its new actor. Match local control
            // surfaces by awaiting that bounded handoff, not losing the first
            // command immediately after Move/Resume.
            let session = state
                .session_manager
                .wait_for_session(&session_id, Duration::from_secs(5))
                .await?;
            let session_id = session.session_id().to_owned();
            let ordinal = session.submit(command_id, command).await?;
            if let Some(expected) = inherited_draft {
                let persisted_id = session_id.clone();
                let persisted_expected = expected.clone();
                blocking(move || {
                    crate::database::clear_session_draft_input_if_matches(
                        &persisted_id,
                        &persisted_expected,
                    )
                })
                .await?;
                if let Some(record) = state
                    .controller
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .state
                    .sessions
                    .get_mut(&session_id)
                    && record.draft_input == expected
                {
                    record.draft_input.clear();
                }
                state.publish_revision();
            }
            if let Some((bundle_id, text)) = history
                && let Err(error) = blocking(move || {
                    crate::database::record_prompt(&session_id, &bundle_id, ordinal, None, &text)
                })
                .await
            {
                tracing::warn!(%error, "prompt was accepted but its history could not be stored");
            }
            Ok(DaemonReply::Ordinal(ordinal))
        }
        DaemonAction::QueueStartupPrompt {
            session_id,
            text,
            inherited_draft,
        } => {
            ensure!(
                !text.trim().is_empty(),
                "a queued startup prompt needs text to send"
            );
            state.queue_startup_step(
                &session_id,
                StartupStep::Prompt {
                    text,
                    inherited_draft,
                },
                cancellation,
            )?;
            Ok(DaemonReply::Done)
        }
        DaemonAction::ReviewerAction {
            session_id,
            role,
            action,
        } => {
            let session = state.session_manager.session(session_id).await?;
            Ok(DaemonReply::Reviewer(Box::new(
                session.reviewer_as(role, action).await?,
            )))
        }
        DaemonAction::StartTurnReview { session_id } => {
            state
                .review_host()
                .start(&session_id, true)
                .await
                .map_err(|refusal| anyhow!("{refusal}"))?;
            state.publish_revision();
            Ok(DaemonReply::Done)
        }
        DaemonAction::ResolveTurnReview {
            session_id,
            resolution,
        } => {
            state
                .review_host()
                .resolve(&session_id, resolution)
                .await
                .map_err(|error| anyhow!("{error}"))?;
            state.publish_revision();
            Ok(DaemonReply::Done)
        }
        DaemonAction::SyncSession { session_id } => {
            state
                .session_manager
                .session(session_id)
                .await?
                .sync_now()
                .await?;
            Ok(DaemonReply::Done)
        }
        DaemonAction::RespondElicitation {
            session_id,
            elicitation_id,
            response,
        } => {
            state
                .session_manager
                .session(session_id)
                .await?
                .respond_elicitation(elicitation_id, response)
                .await?;
            Ok(DaemonReply::Done)
        }
        DaemonAction::StopBackgroundTask {
            session_id,
            background_task_id,
        } => {
            state
                .session_manager
                .session(session_id)
                .await?
                .stop_background_task(background_task_id)
                .await?;
            Ok(DaemonReply::Done)
        }
        DaemonAction::CloseSession { session_id } => {
            state.close_session(session_id).await?;
            Ok(DaemonReply::Done)
        }
        DaemonAction::StartCreateSession(request) => Ok(DaemonReply::RegisteredSession(Box::new(
            state.start_create_session(request).await?,
        ))),
        DaemonAction::WaitCreateSession { session_id } => {
            state.wait_create_session(&session_id).await?;
            Ok(DaemonReply::Done)
        }
        DaemonAction::ResumeSession(request) => {
            state.resume_session(request).await?;
            Ok(DaemonReply::Done)
        }
        DaemonAction::PrepareMoveSession(selection) => Ok(DaemonReply::MovePreparation(Box::new(
            state.prepare_move_session(selection).await?,
        ))),
        DaemonAction::MoveSession(request) => {
            Ok(DaemonReply::MoveOutcome(state.move_session(request).await?))
        }
        DaemonAction::ForceStopSession { session_id } => {
            state.force_stop_session(session_id).await?;
            Ok(DaemonReply::Done)
        }
        DaemonAction::DestroyStoppedSession {
            session_id,
            delete_branch,
        } => {
            state
                .destroy_stopped_session(session_id, branch_disposition(delete_branch))
                .await?;
            Ok(DaemonReply::Done)
        }
        DaemonAction::ForceDestroySession {
            session_id,
            delete_branch,
        } => {
            state
                .force_destroy_session(session_id, branch_disposition(delete_branch))
                .await?;
            Ok(DaemonReply::Done)
        }
        DaemonAction::ForceDeleteWorkspace { workspace_id } => {
            state.force_delete_workspace(workspace_id).await?;
            Ok(DaemonReply::Done)
        }
        DaemonAction::CancelLifecycle { session_id } => {
            blocking({
                let session_id = session_id.clone();
                move || crate::database::request_move_cancellation(&session_id)
            })
            .await?;
            state.cancel_lifecycle(&session_id)?;
            Ok(DaemonReply::Done)
        }
        DaemonAction::RecoverDraft { draft_id } => {
            blocking(move || crate::database::recover_detached_draft(&draft_id)).await?;
            Ok(DaemonReply::Done)
        }
        DaemonAction::Stop => {
            cancellation.cancel();
            Ok(DaemonReply::Done)
        }
    }
}

/// Destroy requests carry a flag, not a controller enum, so the wire protocol
/// stays independent of the controller's types.
fn branch_disposition(delete_branch: bool) -> BranchDisposition {
    if delete_branch {
        BranchDisposition::Delete
    } else {
        BranchDisposition::Keep
    }
}
