use super::*;
use mj_controller::controller::BranchDisposition;

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
        return control.request_cancel("person cancel from phone action");
    }
    false
}

pub(super) fn track_started_phone_session(
    active_actions: &mut std::collections::BTreeSet<String>,
    action_sessions: &mut std::collections::BTreeMap<u64, String>,
    action_id: u64,
    session_id: &str,
) -> std::result::Result<(), String> {
    if !active_actions.insert(session_id.to_owned()) {
        return Err("another operation is already running for the new session".into());
    }
    action_sessions.insert(action_id, session_id.to_owned());
    Ok(())
}

pub(super) struct PhoneActionServices<'a> {
    pub(super) sessions: &'a SessionManagerControl,
    pub(super) daemon_runtime: &'a Arc<RuntimeState>,
}

/// The creation title and the rename a New action starts with. A phone that
/// supplies no title gets the title the terminal would have derived, so a
/// session started from either surface reads the same way in both. Only a
/// title the person typed is a rename: the derived one must not hide the name
/// the harness gives the session.
fn new_session_titles(
    title: Option<String>,
    project_directory: Option<&std::path::Path>,
    bundle_id: &str,
    profile_id: &str,
) -> (String, Option<String>) {
    let typed = title.filter(|title| !title.trim().is_empty());
    let title = typed.clone().unwrap_or_else(|| {
        mj_core::state::default_session_title(project_directory, bundle_id, profile_id)
    });
    (title, typed)
}

/// Record a prompt a person is sending through the web viewer or the HTTP
/// API, before it is submitted; see `mj_controller::database::record_user_prompt`.
/// A failure to record does not stop the prompt, so it is logged.
pub(super) async fn record_user_prompt(session_id: String, command_id: String, text: String) {
    let recorded = mj_core::runtime::spawn_blocking({
        let session_id = session_id.clone();
        move || mj_controller::database::record_user_prompt(&session_id, &command_id, &text)
    })
    .await;
    let error = match recorded {
        Ok(Ok(())) => return,
        Ok(Err(error)) => format!("{error:#}"),
        Err(error) => format!("the history task failed: {error}"),
    };
    tracing::warn!(%error, %session_id, "prompt history could not be stored");
}

pub(super) fn prepare_phone_prompt(
    text: String,
    images: Vec<crate::server::ViewerPromptImage>,
    files: Vec<mj_core::attachment::FileAttachmentRef>,
) -> Result<(RelayCommand, String)> {
    let prompt = phone_prompt_blocks(text.clone(), images, files);
    let history = if text.is_empty() {
        let blocks = serde_json::to_value(&prompt)?;
        mj_core::transcript::materialized_content_text(
            blocks
                .as_array()
                .context("serialized prompt content is not an array")?,
        )
    } else {
        text
    };
    Ok((RelayCommand::Prompt { prompt }, history))
}

pub(super) async fn apply_phone_action(
    controller: &mut Controller,
    services: PhoneActionServices<'_>,
    action: ControllerAction,
    action_id: u64,
    started: &tokio::sync::mpsc::UnboundedSender<PhoneActionStarted>,
    control: &PhoneActionControl,
) -> Result<()> {
    match action {
        ControllerAction::PushSubscribe { subscription } => {
            mj_controller::web_push::blocking(move || {
                mj_controller::database::push_subscribe(subscription)
            })
            .await?;
            mj_controller::web_push::wake();
            Ok(())
        }
        ControllerAction::PushRenew {
            old_endpoint,
            subscription,
        } => {
            mj_controller::web_push::blocking(move || {
                mj_controller::database::push_renew(old_endpoint, subscription)
            })
            .await?;
            mj_controller::web_push::wake();
            Ok(())
        }
        ControllerAction::PushUnsubscribe { endpoint } => {
            mj_controller::web_push::blocking(move || {
                mj_controller::database::push_unsubscribe(&endpoint)
            })
            .await?;
            mj_controller::web_push::wake();
            Ok(())
        }
        ControllerAction::PushTest { endpoint } => {
            mj_controller::web_push::blocking(move || mj_controller::database::push_test(endpoint))
                .await?;
            mj_controller::web_push::wake();
            Ok(())
        }
        ControllerAction::RecordOperatorActivity => {
            services.daemon_runtime.record_operator_activity().await;
            Ok(())
        }
        ControllerAction::SetOperatorAway { away } => {
            services.daemon_runtime.set_operator_away(away).await
        }
        ControllerAction::New {
            remember_launch_choices,
            remember_container_size,
            workspace_id,
            profile_id,
            bundle_id,
            target_id,
            resource_allocation,
            additional_mounts,
            title,
            project_directory,
            create_managed_worktree,
            at,
            branch,
            base,
            no_later_history,
            subagents,
            oracle,
            oracle_choice,
            review,
            no_project_memory,
            no_github_events,
            no_github_credentials,
            notifications_ignored,
            dirty_ack: _dirty_ack,
        } => {
            let workspace_id = if workspace_id.is_empty() {
                let workspaces = mj_controller::database::list_workspaces()?;
                match workspaces.as_slice() {
                    [workspace] => workspace.id.clone(),
                    // Both are preconditions the caller can fix, so each says
                    // so in its own words rather than becoming a generic 500.
                    [] => {
                        return Err(anyhow::Error::new(Refusal::precondition(
                            "this instance has no workspace yet; create one before starting a session",
                        )));
                    }
                    _ => {
                        return Err(anyhow::Error::new(Refusal::precondition(
                            "this instance holds more than one workspace; name the one to start the session in",
                        )));
                    }
                }
            } else {
                workspace_id
            };
            let (title, session_title_override) =
                new_session_titles(title, project_directory.as_deref(), &bundle_id, &profile_id);
            let (published, publication) = tokio::sync::oneshot::channel();
            let (registered, review_notice, capacity_warning) = services
                .daemon_runtime
                .start_create_session_controlled(
                    CreateSessionRequest {
                        remember_launch_choices,
                        remember_container_size,
                        create_managed_worktree,
                        at,
                        branch,
                        base,
                        no_later_history,
                        subagents,
                        oracle,
                        oracle_choice,
                        review,
                        no_project_memory,
                        no_github_events,
                        no_github_credentials,
                        notifications_ignored,
                        initial_prompt: None,
                        workspace_id,
                        profile_id,
                        provider: None,
                        bundle_id,
                        project_directory,
                        target_template_id: target_id,
                        additional_mounts,
                        resource_allocation: resource_allocation.map(|allocation| *allocation),
                        title,
                        session_title_override,
                    },
                    control
                        .lifecycle
                        .clone()
                        .expect("New action has a daemon create control"),
                    publication,
                )
                .await?;
            // The next API call can be a wait that needs the registered
            // record. Advance the history's view before acknowledging the ID,
            // while this action is still off the phone control loop.
            services.daemon_runtime.browser_views(None)?;
            let registered_session_id = registered.session.id.clone();
            started
                .send(PhoneActionStarted {
                    action_id,
                    session_id: registered_session_id.clone(),
                    review_notice,
                    capacity_warning,
                    published,
                })
                .map_err(|_| anyhow::anyhow!("phone server stopped before publishing session"))?;
            // The phone already has the published session. If the daemon stops
            // under this wait the next daemon finishes the create and updates
            // the record the phone shows, so that is no result, not a failure.
            let lifecycle = control
                .lifecycle
                .as_ref()
                .expect("New action has a daemon create control");
            tokio::select! {
                outcome = services.daemon_runtime.wait_create_outcome(&registered_session_id) => {
                    match outcome {
                        crate::daemon::create::CreateWait::Finished => Ok(()),
                        crate::daemon::create::CreateWait::Failed(error) => Err(error),
                    }
                }
                () = lifecycle.shutdown_stopped() => Ok(()),
            }
        }
        ControllerAction::Prompt {
            session_id,
            text,
            images,
            files,
            command_id,
        } => {
            let session = services
                .sessions
                .wait_for_session(&session_id, Duration::from_secs(5))
                .await?;
            let command_id = command_id.map_or_else(|| new_command_id("phone-prompt"), Ok)?;
            let (command, history) = prepare_phone_prompt(text, images, files)?;
            record_user_prompt(session_id, command_id.clone(), history).await;
            session
                .submit(command_id, command)
                .await
                .map_err(prompt_submission_error)?;
            Ok(())
        }
        ControllerAction::RunShell {
            session_id,
            command,
            command_id,
        } => {
            services
                .sessions
                .wait_for_session(&session_id, Duration::from_secs(5))
                .await?
                .submit(
                    command_id.map_or_else(|| new_command_id("phone-shell"), Ok)?,
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
        ControllerAction::Suspend {
            session_id,
            discard_untracked,
            abort_operations,
            without_checkpoint,
            confirm_no_checkpoint,
        } => {
            services
                .daemon_runtime
                .suspend_session_with_consent(
                    session_id,
                    mj_core::suspend_consent::SuspendConsent {
                        discard_untracked,
                        abort_operations,
                        without_checkpoint,
                        confirm_no_checkpoint,
                    },
                )
                .await
        }
        ControllerAction::Destroy { session_id } => {
            services
                .daemon_runtime
                .force_destroy_session(session_id, BranchDisposition::Keep)
                .await
        }
        ControllerAction::Resume { .. } => {
            unreachable!("Resume is admitted by the phone control loop")
        }
        ControllerAction::Move { request } => {
            let outcome = services.daemon_runtime.move_session(*request).await?;
            finish_phone_lifecycle(mj_client::http::LifecycleCompletion::MoveOutcome(outcome))
        }
        ControllerAction::Open { .. } => Ok(()),
        ControllerAction::CancelContainerUpgrade { session_id } => {
            services
                .daemon_runtime
                .cancel_container_upgrade(&session_id)
                .await
        }
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
            let target = services
                .daemon_runtime
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
            // so it travels as a refusal: the phone shows that sentence and
            // the API answers 409 with it, not a 500 behind a log reference.
            services
                .daemon_runtime
                .review_host()
                .start(&session_id)
                .await
                .map_err(|refusal| {
                    anyhow::Error::new(mj_core::refusal::Refusal::precondition(refusal.to_string()))
                })?;
            Ok(())
        }
        ControllerAction::DismissNotification { session_id, target } => {
            services
                .daemon_runtime
                .dismiss_notification(session_id, target)
                .await?;
            Ok(())
        }
        ControllerAction::SetSessionNotificationsIgnored {
            session_id,
            ignored,
        } => controller.set_session_notifications_ignored(&session_id, ignored),
        ControllerAction::TurnControl {
            session_id,
            command,
        } => {
            anyhow::ensure!(
                crate::server::is_turn_control_command(&command),
                "unsupported turn-control command"
            );
            services
                .sessions
                .session(&session_id)
                .await?
                .submit(new_command_id("phone-turn-control")?, command)
                .await?;
            Ok(())
        }
        ControllerAction::InterruptTurn { session_id } => {
            services.daemon_runtime.interrupt_turn(&session_id).await
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
            let control = facts
                .plan_control(active)
                .map_err(|reason| anyhow::anyhow!("{reason}"))?;
            handle
                .client()
                .apply_plan_control(new_command_id("phone-plan-mode")?, control)
                .await?;
            Ok(())
        }
        ControllerAction::ChangeWorkspace {
            session_id,
            workspace_id,
        } => {
            controller.set_session_workspace(&session_id, &workspace_id)?;
            Ok(())
        }
        ControllerAction::SetContainerSettings { .. } => {
            bail!("container settings requests use the settings confirmation handler")
        }
        ControllerAction::UpgradeContainer { request } => {
            services.daemon_runtime.upgrade_container(request).await
        }
        ControllerAction::Restart {
            session_id,
            acknowledge_container_recreation,
        } => {
            let _record = controller
                .state
                .sessions
                .get(&session_id)
                .cloned()
                .with_context(|| format!("unknown session {session_id}"))?;
            services
                .daemon_runtime
                .restart_session_controlled(
                    session_id,
                    acknowledge_container_recreation,
                    control.lifecycle.clone(),
                )
                .await
                .map(|_| ())
        }
        ControllerAction::InterruptAll { session_id } => {
            let result = services.daemon_runtime.interrupt_all(&session_id).await?;
            let failed = result.failures.len();
            // Report the number without exposing worker diagnostics. The
            // successfully interrupted members are already settling.
            if failed != 0 {
                return Err(mj_core::refusal::Refusal::precondition(format!(
                    "Could not interrupt {failed} of the tree's sessions. Open the remaining sessions to use their turn or goal controls, answer their questions, or stop them."
                )).into());
            }
            Ok(())
        }
        // Refreshes are handled by the phone control loop, which owns the
        // pollers they nudge.
        ControllerAction::CheckAvailability
        | ControllerAction::RefreshQuota { .. }
        | ControllerAction::RefreshCapacity { .. } => {
            bail!("refresh actions must be handled by the phone control loop")
        }
    }
}

pub(super) fn finish_phone_lifecycle(reply: mj_client::http::LifecycleCompletion) -> Result<()> {
    match reply {
        mj_client::http::LifecycleCompletion::Done => Ok(()),
        mj_client::http::LifecycleCompletion::MoveOutcome(outcome) => {
            match outcome.outcome.as_str() {
                "completed" | "unchanged" | "interrupted" => Ok(()),
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
        _ => bail!("unexpected lifecycle outcome"),
    }
}

#[cfg(test)]
mod tests {
    use super::new_session_titles;
    use std::path::Path;

    #[test]
    fn a_new_action_without_a_title_is_not_a_rename() {
        for title in [None, Some(String::new()), Some("  \n".to_owned())] {
            let (created_with, rename) =
                new_session_titles(title, Some(Path::new("/work/project")), "bundle", "claude");
            assert_eq!(created_with, "project via claude");
            assert_eq!(rename, None);
        }
    }

    #[test]
    fn a_new_action_keeps_the_title_a_person_supplied_as_a_rename() {
        let (created_with, rename) =
            new_session_titles(Some("My name".into()), None, "bundle", "claude");
        assert_eq!(created_with, "My name");
        assert_eq!(rename.as_deref(), Some("My name"));
    }
}
