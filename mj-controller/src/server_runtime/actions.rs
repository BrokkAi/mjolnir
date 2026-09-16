use super::*;

pub(super) fn phone_action_capacity_available(active_actions: usize) -> bool {
    active_actions < MAX_CONCURRENT_PHONE_ACTIONS
}

/// Point the quota refresher at the profiles the configuration currently
/// defines, alongside the worker-poll and credential-sync targets that are
/// rebuilt from the same reload. A profile added to `config.toml` while the
/// server runs otherwise reaches the snapshot but never the refresher, and
/// reads "quota unavailable" until the next restart.
///
/// Sending a batch restarts every profile's refresh, which spawns a harness
/// process per profile, so the batch travels only when the profiles changed.
/// Reports whether it did.
pub(super) fn republish_quota_profiles(
    controller: &Controller,
    published: &mut std::collections::BTreeMap<String, HarnessProfile>,
    batch: &mut QuotaRefreshBatch,
    profiles_tx: &tokio::sync::watch::Sender<QuotaRefreshBatch>,
) -> bool {
    if *published == controller.config.profiles {
        return false;
    }
    published.clone_from(&controller.config.profiles);
    batch.generation = batch.generation.saturating_add(1);
    batch.profiles = quota_refresh_profiles(controller);
    profiles_tx.send_replace(batch.clone());
    true
}

pub(super) fn request_phone_action_cancellation(
    session_id: &str,
    action_sessions: &std::collections::BTreeMap<u64, String>,
    action_cancellations: &std::collections::BTreeMap<u64, PhoneActionControl>,
) -> bool {
    let control = action_sessions
        .iter()
        .find_map(|(action_id, active_session_id)| {
            (active_session_id == session_id)
                .then(|| action_cancellations.get(action_id))
                .flatten()
        });
    if let Some(control) = control {
        return control.request_cancel();
    }
    false
}

pub(super) fn track_started_phone_session(
    state: &mut State,
    active_actions: &mut std::collections::BTreeSet<String>,
    action_sessions: &mut std::collections::BTreeMap<u64, String>,
    action_id: u64,
    session: SessionRecord,
) -> std::result::Result<(), String> {
    let session_id = session.id.clone();
    if !active_actions.insert(session_id.clone()) {
        return Err("another operation is already running for the new session".into());
    }
    action_sessions.insert(action_id, session_id.clone());
    state.sessions.insert(session_id, session);
    Ok(())
}

/// Carry a finished action's failure into the session projection, and clear it
/// once a later action for the same session succeeds.
///
/// Nothing is waiting on the request any more, so a failure the action itself
/// did not record would reach no one but this process's stderr; the overlay
/// keeps it visible through every later durable reload, where the snapshot's
/// `has_error` takes it to the phone. Clearing on success matters just as
/// much: the overlay has no other expiry, so one transient failure would
/// otherwise badge the session as errored for the daemon's whole lifetime.
pub(super) fn record_action_result(
    pending_action_errors: &mut std::collections::BTreeMap<String, String>,
    session_id: Option<&str>,
    result: &std::result::Result<(), String>,
) {
    let Some(session_id) = session_id else {
        return;
    };
    match result {
        Err(error) => {
            pending_action_errors.insert(session_id.to_owned(), error.clone());
        }
        Ok(()) => {
            pending_action_errors.remove(session_id);
        }
    }
}

/// Retain safe notices even if provisioning removed its provisional session.
/// This bounded, daemon-lifetime history survives durable controller reloads.
pub(super) fn record_launch_failure(
    failures: &mut Vec<crate::server::ViewerLaunchFailure>,
    action_id: u64,
    workspace_id: String,
    session_id: Option<String>,
    error: Option<String>,
) {
    failures.push(crate::server::ViewerLaunchFailure {
        id: format!("{}-{action_id}", std::process::id()),
        workspace_id,
        session_id,
        error,
    });
    if failures.len() > 16 {
        failures.remove(0);
    }
}

pub(super) struct PhoneActionServices<'a> {
    pub(super) sessions: &'a SessionManagerControl,
    pub(super) daemon_runtime: &'a Arc<RuntimeState>,
}

pub(super) async fn apply_phone_action(
    controller: &mut Controller,
    services: PhoneActionServices<'_>,
    action: ControllerAction,
    _executor: &(impl CommandExecutor + Sync),
    action_id: u64,
    started: &tokio::sync::mpsc::UnboundedSender<PhoneActionStarted>,
    control: &PhoneActionControl,
) -> Result<()> {
    match action {
        ControllerAction::New {
            workspace_id,
            profile_id,
            bundle_id,
            target_id,
            title,
            project_directory,
            create_managed_worktree,
            mjolnir_subagents,
            dirty_ack: _dirty_ack,
        } => {
            let workspace_id = if workspace_id.is_empty() {
                let workspaces = crate::database::list_workspaces()?;
                match workspaces.as_slice() {
                    [workspace] => workspace.id.clone(),
                    [] => bail!("create a workspace before starting a phone session"),
                    _ => bail!("phone session creation requires a workspace_id"),
                }
            } else {
                workspace_id
            };
            // A phone that supplies no title gets the one the terminal would
            // have derived, so a session started from either surface reads the
            // same way in both.
            let title = title.unwrap_or_else(|| {
                let project = project_directory
                    .as_ref()
                    .and_then(|path| path.file_name())
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_else(|| bundle_id.clone());
                format!("{project} via {profile_id}")
            });
            let session_title_override = Some(title.clone());
            let (published, publication) = tokio::sync::oneshot::channel();
            let registered = services
                .daemon_runtime
                .start_create_session_controlled(
                    CreateSessionRequest {
                        create_managed_worktree,
                        mjolnir_subagents,
                        initial_prompt: None,
                        workspace_id,
                        profile_id,
                        bundle_id,
                        project_directory,
                        target_template_id: target_id,
                        additional_mounts: Vec::new(),
                        resource_allocation: None,
                        title,
                        session_title_override,
                    },
                    control
                        .create
                        .clone()
                        .expect("New action has a daemon create control"),
                    publication,
                )
                .await?;
            let registered_session_id = registered.session.id.clone();
            started
                .send(PhoneActionStarted {
                    action_id,
                    session: registered.session,
                    published,
                })
                .map_err(|_| anyhow::anyhow!("phone server stopped before publishing session"))?;
            services
                .daemon_runtime
                .wait_create_session(&registered_session_id)
                .await
        }
        ControllerAction::Prompt {
            session_id,
            text,
            images,
        } => {
            services
                .sessions
                .wait_for_session(&session_id, Duration::from_secs(5))
                .await?
                .submit(
                    new_command_id("phone-prompt")?,
                    RelayCommand::Prompt {
                        prompt: phone_prompt_blocks(text, images),
                    },
                )
                .await?;
            Ok(())
        }
        ControllerAction::RunShell {
            session_id,
            command,
        } => {
            services
                .sessions
                .wait_for_session(&session_id, Duration::from_secs(5))
                .await?
                .submit(
                    new_command_id("phone-shell")?,
                    RelayCommand::RunUserShell { command },
                )
                .await?;
            Ok(())
        }
        ControllerAction::CancelShell {
            session_id,
            shell_command_id,
        } => {
            services
                .sessions
                .wait_for_session(&session_id, Duration::from_secs(5))
                .await?
                .submit(
                    new_command_id("phone-cancel-shell")?,
                    RelayCommand::CancelUserShell { shell_command_id },
                )
                .await?;
            Ok(())
        }
        ControllerAction::Close { session_id } => {
            services.daemon_runtime.close_session(session_id).await
        }
        ControllerAction::ForceClose { session_id } => {
            services
                .daemon_runtime
                .force_destroy_session(session_id)
                .await
        }
        ControllerAction::Resume {
            session_id,
            workspace_id,
            profile_id,
            target_id,
            queue,
            additional_mounts,
            resource_allocation,
        } => services
            .daemon_runtime
            .resume_session(ResumeSessionRequest {
                session_id,
                workspace_id,
                profile_id,
                target_template_id: target_id,
                additional_mounts,
                resource_allocation,
                discard_queue: queue == ResumeQueueDisposition::Discard,
                repository_preflight: None,
            })
            .await
            .map(|_| ()),
        ControllerAction::Move { request } => {
            let outcome = services.daemon_runtime.move_session(request).await?;
            match outcome.outcome.as_str() {
                "completed" | "unchanged" => Ok(()),
                "cancelled" | "failed" => {
                    // The daemon keeps detailed diagnostics in its durable
                    // operation record. Only its safe recovery guidance is
                    // copied into the phone action error, where the normal
                    // failed-action path records a visible session error.
                    let recovery = outcome.recovery.unwrap_or_else(|| {
                        "Inspect the session and retry Move or Resume with previous settings."
                            .into()
                    });
                    bail!("Move {}: {recovery}", outcome.outcome)
                }
                status => bail!("Move returned an unknown outcome: {status}"),
            }
        }
        ControllerAction::Open { .. } => Ok(()),
        ControllerAction::Cancel { .. } => {
            bail!("cancel actions must be handled by the phone control loop")
        }
        ControllerAction::RemoveQueuedPrompt {
            session_id,
            queue_id,
        } => {
            services
                .sessions
                .session(&session_id)
                .await?
                .submit(
                    new_command_id("phone-remove-prompt")?,
                    RelayCommand::RemoveQueuedPrompt {
                        queued_command_id: queue_id,
                    },
                )
                .await?;
            Ok(())
        }
        ControllerAction::RespondElicitation {
            session_id,
            elicitation_id,
            response,
        } => {
            services
                .sessions
                .session(&session_id)
                .await?
                .respond_elicitation(elicitation_id, response)
                .await
        }
        ControllerAction::Rename { session_id, title } => {
            controller.rename_session(&session_id, &title)?;
            Ok(())
        }
        ControllerAction::StartReview { session_id } => {
            // The refusal is a sentence for the person holding the phone --
            // "prompts are queued", "set [review] profile in config.toml" --
            // so it travels as the error text of this action.
            services
                .daemon_runtime
                .review_host()
                .start(&session_id, true)
                .await
                .map_err(|refusal| anyhow::anyhow!("{refusal}"))?;
            Ok(())
        }
        ControllerAction::ResolveReview {
            session_id,
            resolution,
        } => {
            let resolution = crate::server::resolution_from_name(&resolution)
                .context("a review is resolved by forward, dismiss, or cancel")?;
            services
                .daemon_runtime
                .review_host()
                .resolve(&session_id, resolution)
                .await
                .map_err(|error| anyhow::anyhow!("{error}"))?;
            Ok(())
        }
        ControllerAction::CancelTurn { session_id } => {
            services
                .sessions
                .session(&session_id)
                .await?
                .submit(new_command_id("phone-cancel-turn")?, RelayCommand::Cancel)
                .await?;
            Ok(())
        }
        ControllerAction::SetConfig {
            session_id,
            key,
            value,
        } => {
            services
                .sessions
                .session(&session_id)
                .await?
                .submit(
                    new_command_id("phone-set-config")?,
                    RelayCommand::SetConfig { key, value },
                )
                .await?;
            Ok(())
        }
        ControllerAction::SetPlanMode { session_id, active } => {
            // Which call turns plan mode on is a fact about the harness, so it
            // is asked of the shared decision rather than decided here or, far
            // worse, in the browser.
            let harness_kind = controller
                .state
                .sessions
                .get(&session_id)
                .with_context(|| format!("unknown session {session_id}"))?
                .harness_kind;
            let handle = services.sessions.session(&session_id).await?;
            let operational = handle
                .view()
                .snapshot
                .map(|snapshot| snapshot.operational)
                .context("the session has not reported what it supports yet")?;
            let facts = mj_core::acp::AcpSessionFacts::from_operational(
                harness_kind,
                &operational.config,
                &operational.config_options,
                operational.modes.as_ref(),
            );
            let command = match facts.plan_control(active) {
                Ok(mj_core::acp::PlanControl::SetConfig { key, value }) => {
                    RelayCommand::SetConfig { key, value }
                }
                Ok(mj_core::acp::PlanControl::SetSessionMode { mode_id }) => {
                    RelayCommand::SetSessionMode { mode_id }
                }
                Err(reason) => bail!("{reason}"),
            };
            handle
                .submit(new_command_id("phone-plan-mode")?, command)
                .await?;
            Ok(())
        }
        // Refreshes are handled by the phone control loop, which owns the
        // pollers they nudge.
        ControllerAction::RefreshQuota { .. } | ControllerAction::RefreshCapacity { .. } => {
            bail!("refresh actions must be handled by the phone control loop")
        }
    }
}
