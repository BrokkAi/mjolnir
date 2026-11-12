//! What the dashboard does with the work its key handling asks for.
//!
//! Every arm here either updates UI state directly or starts a background job,
//! except `SetupLogin`, which hands the terminal to `mj login` and waits for it.

use mj_client::daemon::{DaemonAction as MachineAction, DaemonReply as MachineReply};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result, bail};
use mj_core::config::is_bare_project_target;
use mj_core::state::{MoveSelection, MoveSessionRequest};

use mj_controller::controller::ResumeRepositorySourceReceipt;
use mj_controller::targets::CancellableProcessExecutor;
use mj_tui::WebViewerAccess;
use mj_tui::{DashboardAction, SessionOperationKind};

use crate::daemon;
use crate::dashboard::io::{
    ConfigRenameRequest, ContainerSettingsRequest, DashboardIoUpdate, LifecycleOperationRequest,
    ResumeRepositoryPreflightApply, config_only_controller, report, spawn_async_job,
    spawn_cancellable_io, spawn_cancellable_io_with_token, spawn_clipboard_read,
    spawn_config_rename, spawn_create_bundle, spawn_dashboard_change_workspace,
    spawn_dashboard_container_settings, spawn_dashboard_create_session, spawn_dashboard_rename,
    spawn_lifecycle_operation, spawn_remove_bundle, spawn_workspace_close,
    spawn_workspace_close_cancel, spawn_workspace_create, spawn_workspace_draft_recovery,
    spawn_workspace_management_load, spawn_workspace_rename,
};
use crate::dashboard::{DashboardContext, QUOTA_REFRESH_NOTICE, resume_progress_notice};
use crate::import::{DashboardImportSafety, PendingDashboardImport};
use crate::pollers::LifecycleSuccess;
use crate::short_id;

struct MbxInstallCompletion {
    install_result: Result<String, String>,
    preview_result: Result<Option<mj_core::state::BuildCachePreview>, String>,
    install_mbx_available: bool,
}

/// The chat-scoped toggles the host's keys can run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ChatToggle {
    TranscriptRendering,
    Dictation,
}

/// Applies a toggle to the conversation on screen, or says there is none.
///
/// The keys that run these live in the dashboard's command registry, so they
/// answer from every surface, including a pane that has never opened a chat.
pub(crate) fn apply_chat_toggle(
    dashboard: &mut mj_tui::DashboardState,
    chat: Option<&mut mj_chat::chat::ActiveChat>,
    toggle: ChatToggle,
) {
    let Some(chat) = chat else {
        dashboard.set_notice("No conversation is open.");
        return;
    };
    match toggle {
        ChatToggle::TranscriptRendering => chat.toggle_transcript_rendering(),
        ChatToggle::Dictation => {
            if let Some(reason) = chat.toggle_dictation() {
                dashboard.set_notice(reason);
            }
        }
    }
}

/// What the notice bar says after the session menu's Copy session ID. The
/// clipboard gets the full ID; the notice names the short form the rest of
/// the dashboard uses.
pub(crate) fn copied_session_id_notice(session_id: &str) -> String {
    format!("Copied session ID {}", short_id(session_id))
}

fn start_suspension(
    context: &mut DashboardContext,
    session_id: String,
    acknowledge_unpublished_work: bool,
    ignore_nested_repositories: bool,
) {
    context.dashboard.set_notice(format!(
        "Suspending {}…",
        context.session_notice_name(&session_id)
    ));
    let request = context.begin_lifecycle_operation(&session_id, SessionOperationKind::Suspending);
    mark_active_chat_retiring(context.chats.get_mut(&session_id), &session_id);
    spawn_lifecycle_operation(
        request,
        context.critical_operations.clone(),
        move |_cancelled, admission| {
            mj_core::runtime::block_on(async {
                let mut client = daemon::connect_or_start().await?;
                let receipt = admission
                    .submit(
                        &mut client,
                        daemon::SessionLifecycleRequest::Suspend {
                            session_id,
                            acknowledge_unpublished_work,
                            ignore_nested_repositories,
                        },
                    )
                    .await?;
                client.wait_session_lifecycle(receipt).await?;
                Ok::<_, anyhow::Error>(())
            })??;
            Ok(LifecycleSuccess::Closed)
        },
    );
}

/// Carries out one dashboard action.
pub(crate) async fn apply_dashboard_action(
    context: &mut DashboardContext,
    action: DashboardAction,
) -> Result<()> {
    context.cancel_stale_path_input();
    // A fast-start launch bypasses the new-session key, so it meets the Git
    // identity check here.
    let action = match action {
        launch @ DashboardAction::GoLaunch { .. } => {
            context.dashboard.gate_launch_on_git_identity(launch)
        }
        action => action,
    };
    match action {
        DashboardAction::SetOperatorAway { away } => {
            let revision = context.dashboard.operator_presence_revision();
            crate::dashboard::io::spawn_critical_async(
                context.critical_operations.clone(),
                "changing operator presence",
                context.dashboard_io_tx.clone(),
                std::time::Duration::from_secs(30),
                async move {
                    let mut client = daemon::connect_or_start().await?;
                    match client
                        .request(daemon::DaemonAction::SetOperatorAway { away })
                        .await?
                    {
                        daemon::DaemonReply::Done => Ok(()),
                        reply => bail!("unexpected operator-presence reply {reply:?}"),
                    }
                },
                move |result| DashboardIoUpdate::OperatorAway {
                    away,
                    revision,
                    result,
                },
            );
        }
        DashboardAction::None => {}
        DashboardAction::CheckGitIdentity => {
            // The daemon reads Git in the environment its sessions inherit.
            let updates = context.dashboard_io_tx.clone();
            tokio::spawn(async move {
                let result = async { daemon::connect_or_start().await?.git_identity().await }
                    .await
                    .map_err(|e: anyhow::Error| format!("{e:#}"));
                report(
                    "reading the Git identity",
                    &updates,
                    DashboardIoUpdate::GitIdentityChecked(result),
                );
            });
        }
        DashboardAction::SaveGitIdentity { name, email } => {
            super::io::spawn_critical_async(
                context.critical_operations.clone(),
                "saving the Git identity",
                context.dashboard_io_tx.clone(),
                super::io::SAVE_ACK_TIMEOUT,
                async move {
                    daemon::connect_or_start()
                        .await?
                        .set_git_identity(name, email)
                        .await
                },
                DashboardIoUpdate::GitIdentitySaved,
            );
        }
        DashboardAction::CopyNativeSessionId { native_session_id } => {
            context.copy_text(&native_session_id, "Copied session ID")?;
        }
        DashboardAction::CopySessionId { session_id } => {
            context.copy_text(&session_id, &copied_session_id_notice(&session_id))?;
        }
        DashboardAction::ToggleTranscriptRendering => {
            let (dashboard, chat) = context.dashboard_and_visible_chat();
            apply_chat_toggle(dashboard, chat, ChatToggle::TranscriptRendering);
        }
        DashboardAction::ToggleDictation => {
            let (dashboard, chat) = context.dashboard_and_visible_chat();
            apply_chat_toggle(dashboard, chat, ChatToggle::Dictation);
        }
        DashboardAction::GoPrepareProject { target_id: _ } => {
            if let Some(go) = context.dashboard.go_mode() {
                spawn_create_bundle(
                    vec![go.directory.to_string_lossy().into_owned()],
                    context.dashboard_io_tx.clone(),
                    context.critical_operations.clone(),
                );
            }
        }
        DashboardAction::GoLaunch { recipe } => {
            if let Some(go) = context.dashboard.go_mode().cloned() {
                context.cancel_startup_session();
                let workspace_id = context
                    .dashboard
                    .active_workspace_id()
                    .unwrap_or_default()
                    .to_owned();
                let retry = DashboardAction::GoLaunch {
                    recipe: recipe.clone(),
                };
                context.dashboard.set_notice("Preparing session…");
                let config = context.controller.config.clone();
                super::io::spawn_critical_io(
                    context.critical_operations.clone(),
                    "preparing fast-start session",
                    context.dashboard_io_tx.clone(),
                    move || crate::go::resolve_recipe(&config, &go.directory, recipe),
                    move |result| DashboardIoUpdate::GoPrepared {
                        workspace_id,
                        retry: Box::new(retry),
                        result: Box::new(result),
                    },
                );
            }
        }
        DashboardAction::SaveSpinnerStyle { style } => {
            super::io::spawn_spinner_style_save(
                style,
                context.dashboard_io_tx.clone(),
                context.critical_operations.clone(),
            );
        }
        DashboardAction::QuitDetach => {
            context.request_shutdown();
        }
        DashboardAction::SelectWorkspace { workspace_id } => {
            // An explicit choice outranks opening a workspace created earlier.
            context.pending_workspace_selection = None;
            if context.dashboard.active_workspace_id() != Some(workspace_id.as_str()) {
                context.select_workspace(Some(workspace_id));
            }
        }
        DashboardAction::LoadNativeAgentHistory {
            owner,
            child,
            before,
        } => {
            let updates = context.dashboard_io_tx.clone();
            tokio::spawn(async move {
                let result = async {
                    daemon::connect_or_start()
                        .await?
                        .native_agent_history(owner.clone(), child.clone(), before)
                        .await
                }
                .await
                .map_err(|error: anyhow::Error| format!("{error:#}"));
                if let Err(error) = updates.send(DashboardIoUpdate::NativeAgentHistory {
                    owner,
                    child,
                    result,
                }) {
                    tracing::warn!(%error, "native agent history receiver closed");
                }
            });
        }
        DashboardAction::StopNativeAgent { owner, child } => {
            let updates = context.dashboard_io_tx.clone();
            tokio::spawn(async move {
                let result = async {
                    let mut client = daemon::connect_existing().await?;
                    client
                        .stop_background_task(owner.clone(), format!("native-agent:{child}"))
                        .await
                }
                .await
                .map_err(|error: anyhow::Error| format!("{error:#}"));
                if let Err(error) = updates.send(DashboardIoUpdate::NativeAgentStopped {
                    owner,
                    child,
                    result,
                }) {
                    tracing::warn!(%error, "native agent stop result receiver closed");
                }
            });
        }
        DashboardAction::OpenSubagents { parent_id } => context.open_subagents(parent_id),
        DashboardAction::DismissNotification { session_id, target } => {
            let result_session = session_id.clone();
            let result_target = target.clone();
            spawn_async_job(
                Some(context.critical_operations.clone()),
                "notification dismissal",
                context.dashboard_io_tx.clone(),
                std::time::Duration::from_secs(30),
                async move {
                    let mut client = daemon::connect_existing().await?;
                    match client
                        .request(mj_client::daemon::DaemonAction::DismissNotification {
                            session_id,
                            target,
                        })
                        .await?
                    {
                        mj_client::daemon::DaemonReply::Ordinal(ordinal) => Ok(ordinal),
                        reply => anyhow::bail!("unexpected dismissal reply: {reply:?}"),
                    }
                },
                move |result| DashboardIoUpdate::NotificationDismissed {
                    session_id: result_session,
                    target: result_target,
                    result,
                },
            );
        }
        DashboardAction::InterruptAll { targets } => {
            let updates = context.dashboard_io_tx.clone();
            tokio::spawn(async move {
                let failures = interrupt_all(&targets).await;
                if let Err(error) =
                    updates.send(DashboardIoUpdate::InterruptAllFinished { targets, failures })
                {
                    tracing::warn!(%error, "interrupt all result receiver closed");
                }
            });
        }
        DashboardAction::InterruptTurn { session_id } => {
            // The chat is what knows the turn and sends the interrupt, so the
            // command reaches only a conversation this terminal has open.
            let notice = match context
                .chats
                .get_mut(&session_id)
                .map(|chat| chat.interrupt_turn())
            {
                Some(true) => None,
                Some(false) => Some("No turn to interrupt."),
                None => Some("Open the conversation to interrupt its turn."),
            };
            if let Some(notice) = notice {
                context.dashboard.set_notice(notice);
            }
        }
        DashboardAction::ExitSubagentWorkspace => {
            let parent_id = context.dashboard.subagent_parent_id().map(str::to_owned);
            context.dashboard.close_subagent_workspace();
            if let Some(parent_id) = parent_id {
                context.open_chat_session(&parent_id);
            }
        }
        DashboardAction::LoadWorkspaceManagement { generation } => {
            spawn_workspace_management_load(generation, context.dashboard_io_tx.clone());
        }
        DashboardAction::CreateWorkspace { generation, name } => {
            spawn_workspace_create(
                generation,
                name,
                context.dashboard_io_tx.clone(),
                context.critical_operations.clone(),
            );
        }
        DashboardAction::RenameWorkspace {
            generation,
            workspace_id,
            name,
        } => {
            spawn_workspace_rename(
                generation,
                workspace_id,
                name,
                context.dashboard_io_tx.clone(),
                context.critical_operations.clone(),
            );
        }
        DashboardAction::CancelWorkspaceClose { workspace_id } => {
            spawn_workspace_close_cancel(workspace_id, context.dashboard_io_tx.clone());
        }
        DashboardAction::CloseWorkspace {
            generation,
            workspace_id,
        } => {
            spawn_workspace_close(generation, workspace_id, context.dashboard_io_tx.clone());
        }
        DashboardAction::RecoverWorkspaceDraft {
            generation,
            draft_id,
        } => {
            spawn_workspace_draft_recovery(
                generation,
                draft_id,
                context.dashboard_io_tx.clone(),
                context.critical_operations.clone(),
            );
        }
        DashboardAction::OpenConfig => context.dashboard.begin_setup(),
        DashboardAction::DiscoverSetup {
            generation,
            scope,
            machines,
        } => {
            super::io::spawn_setup_discovery(
                generation,
                scope,
                machines,
                context.dashboard_io_tx.clone(),
                context.critical_operations.clone(),
            );
        }
        DashboardAction::CheckAvailability { generation } => {
            super::io::spawn_availability_check(
                generation,
                context.dashboard_io_tx.clone(),
                context.critical_operations.clone(),
            );
        }
        DashboardAction::ResolveContainerPath { session_id, source } => {
            let input_context = context.dashboard.path_input_context();
            let target_id = context
                .controller
                .state
                .sessions
                .get(&session_id)
                .context("Session no longer exists")?
                .target_template_id
                .clone();
            let (cancelled, _) = spawn_cancellable_io_with_token(
                context.critical_operations.clone(),
                "resolving attached directory",
                context.dashboard_io_tx.clone(),
                move |cancelled| match super::io::machine_request(
                    MachineAction::ResolveContainerPath {
                        target_id,
                        source: source.into(),
                    },
                    cancelled,
                )? {
                    MachineReply::ResolvedPath(path) => Ok(path),
                    reply => anyhow::bail!("unexpected path reply: {reply:?}"),
                },
                move |result| DashboardIoUpdate::ContainerPathResolved {
                    context: input_context,
                    result,
                },
            );
            context.track_path_input(cancelled);
        }
        DashboardAction::ResolveSetupPath {
            generation,
            draft,
            path,
            value,
            machine,
        } => {
            let requested = value.clone();
            let (cancelled, _) = spawn_cancellable_io_with_token(
                context.critical_operations.clone(),
                "resolving setup path",
                context.dashboard_io_tx.clone(),
                move |cancelled| match super::io::machine_request(
                    MachineAction::ResolveSetupPath {
                        machine: *machine,
                        source: requested.into(),
                    },
                    cancelled,
                )? {
                    MachineReply::ResolvedPath(path) => Ok(path),
                    reply => anyhow::bail!("unexpected setup path reply: {reply:?}"),
                },
                move |result| DashboardIoUpdate::SetupPathResolved {
                    generation,
                    draft,
                    path,
                    value,
                    result,
                },
            );
            context.track_path_input(cancelled);
        }
        DashboardAction::PreviewBuildCache {
            generation,
            key,
            machine,
        } => {
            // A stale answer is dropped by its key, so the job is not tied
            // to the path-input job that a later edit would cancel.
            spawn_cancellable_io_with_token(
                context.critical_operations.clone(),
                "previewing the build cache",
                context.dashboard_io_tx.clone(),
                move |cancelled| {
                    // Several host commands, one of which may be an SSH
                    // round trip each.
                    let executor = CancellableProcessExecutor::new(cancelled)
                        .with_deadline(std::time::Duration::from_secs(60));
                    let result =
                        mj_controller::controller::preview_build_cache(&machine, &executor)?;
                    let install_mbx_available = result.as_ref().is_some_and(|preview| {
                        mj_controller::controller::mbx_install_kind(preview).is_some()
                    });
                    Ok((result, install_mbx_available))
                },
                move |result| match result {
                    Ok((result, install_mbx_available)) => DashboardIoUpdate::BuildCachePreviewed {
                        generation,
                        key,
                        result: Ok(result),
                        install_mbx_available,
                    },
                    Err(error) => DashboardIoUpdate::BuildCachePreviewed {
                        generation,
                        key,
                        result: Err(error),
                        install_mbx_available: false,
                    },
                },
            );
        }
        DashboardAction::InstallMbx {
            generation,
            key,
            machine_id,
            machine,
        } => {
            if !context
                .dashboard
                .mbx_install_started(generation, &key, &machine_id)
            {
                return Ok(());
            }
            let operation_label = format!("installing mbx on machine {machine_id}");
            spawn_cancellable_io_with_token(
                context.critical_operations.clone(),
                operation_label,
                context.dashboard_io_tx.clone(),
                move |cancelled| {
                    let install_executor = CancellableProcessExecutor::new(cancelled.clone())
                        .with_deadline(std::time::Duration::from_secs(240));
                    let install_result = mj_controller::controller::install_mbx(
                        &machine,
                        &install_executor,
                    )
                    .map(|result| {
                        let verb = match result.kind {
                            mj_controller::controller::MbxInstallKind::Install => "Installed",
                            mj_controller::controller::MbxInstallKind::Upgrade => "Upgraded",
                        };
                        let profile = if result.profile_changed {
                            format!("updated {}", result.profile_file)
                        } else {
                            format!(
                                "left {} unchanged because its marked PATH block already exists",
                                result.profile_file
                            )
                        };
                        let mut message = format!(
                            "{verb} mbx at {} (version {}). {profile}. New login shells will pick up the PATH change.",
                            result.program.display(),
                            result.version
                        );
                        if let Some(warning) = result.profile_warning {
                            message.push(' ');
                            message.push_str(&warning);
                        }
                        if let Some(path_line) = result.manual_path_line {
                            message.push(' ');
                            message.push_str(&path_line);
                        }
                        message
                    })
                    .map_err(|error| format!("{error:#}"));
                    let preview_executor = CancellableProcessExecutor::new(cancelled)
                        .with_deadline(std::time::Duration::from_secs(60));
                    let preview_result =
                        mj_controller::controller::preview_build_cache(&machine, &preview_executor)
                            .map_err(|error| format!("{error:#}"));
                    let install_mbx_available = preview_result
                        .as_ref()
                        .ok()
                        .and_then(Option::as_ref)
                        .is_some_and(|preview| {
                            mj_controller::controller::mbx_install_kind(preview).is_some()
                        });
                    Ok(MbxInstallCompletion {
                        install_result,
                        preview_result,
                        install_mbx_available,
                    })
                },
                move |result| {
                    let completion = result.unwrap_or_else(|error| MbxInstallCompletion {
                        install_result: Err(error),
                        preview_result: Err(
                            "preview refresh was skipped because the background task failed".into(),
                        ),
                        install_mbx_available: false,
                    });
                    DashboardIoUpdate::MbxInstalled {
                        generation,
                        key,
                        machine_id,
                        result: completion.install_result,
                        preview: completion.preview_result,
                        install_mbx_available: completion.install_mbx_available,
                    }
                },
            );
        }
        DashboardAction::PreviewArchiveSpace {
            generation,
            older_than_days,
        } => {
            // Walks the sessions directory, so it belongs in the blocking
            // pool; a stale answer is dropped by its days value.
            super::io::spawn_io(
                "estimating archive space",
                context.dashboard_io_tx.clone(),
                move || {
                    mj_core::runtime::block_on(async {
                        daemon::connect_or_start()
                            .await?
                            .archive_space_preview(older_than_days)
                            .await
                    })?
                },
                move |result| DashboardIoUpdate::ArchiveSpacePreviewed {
                    generation,
                    older_than_days,
                    result,
                },
            );
        }
        DashboardAction::SaveSetup {
            generation,
            original,
            updated,
        } => {
            super::io::spawn_setup_save(
                generation,
                original,
                updated,
                None,
                context.dashboard_io_tx.clone(),
                context.critical_operations.clone(),
            );
        }
        DashboardAction::SaveSetupAndLogin {
            generation,
            original,
            updated,
            profile_id,
        } => {
            super::io::spawn_setup_save(
                generation,
                original,
                updated,
                Some(profile_id),
                context.dashboard_io_tx.clone(),
                context.critical_operations.clone(),
            );
        }
        DashboardAction::OpenManagedSetup => {
            context.dashboard.begin_managed_setup();
        }
        DashboardAction::SetupLogin {
            generation,
            profile_id,
        } => {
            let result = run_setup_login(context, &profile_id).await;
            context.loop_stalls.forget_pass();
            context
                .dashboard
                .setup_login_finished(generation, &profile_id, result);
        }
        // One key refreshes both panes, so it runs both requests rather than
        // leaving the user to focus each pane in turn.
        DashboardAction::RefreshAll => {
            context.request_quota_refresh();
            context.request_capacity_refresh();
            context.dashboard.set_notice(QUOTA_REFRESH_NOTICE);
        }
        action @ (DashboardAction::LoadWebAccess
        | DashboardAction::RecoverWebViewer(_)
        | DashboardAction::InspectWebListener) => {
            spawn_web_request(context, action);
        }
        DashboardAction::RestartDaemon => {
            spawn_daemon_restart(context);
        }
        DashboardAction::WarmProfileCapabilities { key, config } => {
            super::io::spawn_background_async(
                "starting profile capability hydration",
                context.dashboard_io_tx.clone(),
                std::time::Duration::from_secs(30),
                async move {
                    let config = serde_json::from_str(&config)?;
                    let mut client = daemon::connect_existing().await?;
                    client.warm_profile_capabilities(config).await
                },
                move |result| DashboardIoUpdate::ProfileHydration { key, result },
            );
        }
        DashboardAction::ProbeGitStatus { session_id } => {
            context.spawn_git_probe(session_id);
        }
        DashboardAction::CancelWebAccess => {
            context.web_request_cancel = None;
            context.web_request_generation = context.web_request_generation.wrapping_add(1);
        }
        action @ (DashboardAction::LoadMountHistory | DashboardAction::RefreshProjects { .. }) => {
            let retry = matches!(action, DashboardAction::RefreshProjects { retry: true });
            context.dashboard.set_notice("Refreshing recent projects…");
            let history_tx = context.dashboard_io_tx.clone();
            let picker_context = context.dashboard.project_catalog_context();
            let history_context = picker_context.clone();
            crate::dashboard::io::spawn_async_job(
                None,
                "refreshing projects",
                context.dashboard_io_tx.clone(),
                std::time::Duration::from_secs(125),
                async move {
                    let history = async {
                        daemon::connect_or_start()
                            .await?
                            .stored_session_metadata()
                            .await
                    }
                    .await
                    .map(|metadata| {
                        metadata
                            .mount_history
                            .iter()
                            .map(|(key, value)| (key.clone(), value.clone()))
                            .collect()
                    })
                    .map_err(|error| format!("{error:#}"));
                    // Publish the history before requesting discovery. A late
                    // history snapshot cannot erase locations discovery adds.
                    history_tx
                        .send(DashboardIoUpdate::MountHistory {
                            context: history_context,
                            result: history,
                        })
                        .map_err(|_| anyhow::anyhow!("dashboard closed during project refresh"))?;
                    let mut daemon = daemon::connect_or_start().await?;
                    let mut view = daemon.project_catalog(true, retry).await?;
                    while matches!(
                        view.status,
                        mj_core::project_catalog::ProjectCatalogStatus::Refreshing
                    ) {
                        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
                        view = daemon.project_catalog(false, false).await?;
                    }
                    Ok(view)
                },
                move |result| DashboardIoUpdate::ProjectCatalog {
                    context: picker_context,
                    result,
                },
            );
        }
        DashboardAction::CheckProjectDirectories { paths } => {
            for path in paths {
                let reported = path.clone();
                let label = format!("checking project directory {}", path.display());
                spawn_cancellable_io(
                    context.critical_operations.clone(),
                    label,
                    context.dashboard_io_tx.clone(),
                    move |cancelled| match super::io::machine_request(
                        MachineAction::CheckProjectDirectories { paths: vec![path] },
                        cancelled,
                    )? {
                        MachineReply::ProjectDirectories(paths) => {
                            Ok(paths.first().map(|(_, exists)| *exists))
                        }
                        reply => anyhow::bail!("unexpected directory reply: {reply:?}"),
                    },
                    move |checked| DashboardIoUpdate::ProjectDirectory {
                        path: reported,
                        exists: checked.unwrap_or(None),
                    },
                );
            }
        }
        DashboardAction::TestTarget { target_id } => {
            let config = context.controller.config.clone();
            let reported_id = target_id.clone();
            let (cancelled, _) = spawn_cancellable_io_with_token(
                context.critical_operations.clone(),
                format!("testing target {target_id}"),
                context.dashboard_io_tx.clone(),
                move |cancelled| {
                    if cancelled.load(Ordering::Acquire) {
                        anyhow::bail!("target test cancelled");
                    }
                    let executor = CancellableProcessExecutor::new(cancelled)
                        .with_deadline(std::time::Duration::from_secs(15));
                    config_only_controller(config).test_target(&target_id, &executor)
                },
                move |result| DashboardIoUpdate::TargetTest {
                    target_id: reported_id,
                    result,
                },
            );
            context.target_test_cancel = Some(cancelled);
        }
        DashboardAction::CancelTargetTest => {
            if let Some(cancelled) = context.target_test_cancel.take() {
                cancelled.store(true, Ordering::Release);
            }
        }
        DashboardAction::RenameProfile { old_id, new_id } => {
            let what = format!("profile {old_id} to {new_id}");
            spawn_config_rename(
                ConfigRenameRequest {
                    what,
                    old_id,
                    new_id,
                    profile: true,
                },
                context.dashboard_io_tx.clone(),
                context.critical_operations.clone(),
            );
        }
        DashboardAction::RenameTarget { old_id, new_id } => {
            let what = format!("target {old_id} to {new_id}");
            spawn_config_rename(
                ConfigRenameRequest {
                    what,
                    old_id,
                    new_id,
                    profile: false,
                },
                context.dashboard_io_tx.clone(),
                context.critical_operations.clone(),
            );
        }
        DashboardAction::PasteFromClipboard => {
            if !context.clipboard_read_in_flight {
                context.clipboard_read_in_flight = true;
                context.dashboard.set_notice("Reading clipboard…");
                spawn_clipboard_read(
                    context.dashboard.paste_target(),
                    context.dashboard_io_tx.clone(),
                );
            }
        }
        DashboardAction::MarkAllRead { receipts } => {
            context.acknowledge_dashboard_sessions(receipts);
        }
        DashboardAction::OpenResumeDialog => context.start_resume_discovery(),
        DashboardAction::SearchArchivedSessions { request_id, query } => {
            context.wiki_search_task = None;
            let guard = crate::dashboard::io::spawn_wiki_search(
                request_id,
                query,
                crate::dashboard::io::WIKI_SEARCH_DEBOUNCE,
                context.dashboard_io_tx.clone(),
            );
            context.wiki_search_task = Some((request_id, guard));
        }
        DashboardAction::LoadResumeRecord { session_id } => {
            crate::dashboard::io::spawn_resume_record(session_id, context.dashboard_io_tx.clone());
        }
        DashboardAction::LoadArchivedBrief { wiki_id } => {
            crate::dashboard::io::spawn_wiki_brief(wiki_id, context.dashboard_io_tx.clone());
        }
        DashboardAction::LoadArchivedHits { wiki_id, query } => {
            crate::dashboard::io::spawn_wiki_hits(wiki_id, query, context.dashboard_io_tx.clone());
        }
        action @ DashboardAction::RestoreArchivedSession { .. } => {
            let DashboardAction::RestoreArchivedSession {
                workspace_id,
                wiki_id,
                profile_id,
                target_template_id,
            } = action.clone()
            else {
                return Ok(());
            };
            context
                .dashboard
                .set_notice("Restoring the archived session…");
            crate::dashboard::io::spawn_dashboard_restore_session(
                mj_client::daemon::WikiRestoreRequest {
                    wiki_id,
                    workspace_id,
                    profile_id,
                    target_template_id,
                    project_directory: None,
                    additional_mounts: Vec::new(),
                    resource_allocation: None,
                },
                action,
                context.dashboard_io_tx.clone(),
                context.lifecycle_updates_tx.clone(),
                context.critical_operations.clone(),
            );
        }
        DashboardAction::ImportSession {
            profile_id,
            native_session_id,
            display_title,
        } => {
            if context.dashboard.active_workspace_id().is_none() {
                context
                    .dashboard
                    .set_notice("Select a workspace before importing a session.");
            } else {
                context.start_import(
                    PendingDashboardImport {
                        profile_id,
                        native_session_id,
                        display_title,
                    },
                    DashboardImportSafety {
                        project_root: None,
                        create_managed_worktree: None,
                        accepted: false,
                    },
                );
            }
        }
        DashboardAction::CancelImport => {
            if let Some(active) = context.active_import.as_ref() {
                active.cancelled.store(true, Ordering::Release);
                context
                    .dashboard
                    .update_import_progress(1, None, "Cancelling upload…".into());
            }
        }
        DashboardAction::ConfirmImportBundle {
            create_managed_worktree,
            accepted,
            project_root,
        } => {
            let Some(pending) = context.pending_import.take() else {
                context.dashboard.finish_import();
                context.dashboard.set_notice("Import confirmation expired.");
                return Ok(());
            };
            if accepted {
                if context.dashboard.active_workspace_id().is_none() {
                    context.dashboard.finish_import();
                    context
                        .dashboard
                        .set_notice("Select a workspace before importing a session.");
                } else {
                    context.start_import(
                        pending,
                        DashboardImportSafety {
                            create_managed_worktree,
                            project_root: Some(project_root),
                            accepted: true,
                        },
                    );
                }
            } else {
                context.dashboard.finish_import();
                context
                    .dashboard
                    .set_notice("Import cancelled; no Mjolnir files were changed.");
            }
        }
        DashboardAction::QueueStartupPrompt { session_id, prompt } => {
            // The daemon clears the saved draft only when it still holds the
            // text this prompt was typed from; anything else stays put.
            let inherited_draft = context
                .controller
                .state
                .sessions
                .get(&session_id)
                .filter(|session| session.draft_input == prompt.payload.encode_draft())
                .map(|session| session.draft_input.clone());
            context.startup_prompts.enqueue(
                super::io::NamedSession {
                    name: super::io::session_notice_name(&context.controller.state, &session_id),
                    id: session_id,
                },
                prompt,
                inherited_draft,
                context.dashboard_io_tx.clone(),
                context.critical_operations.clone(),
            );
        }
        DashboardAction::WithdrawStartupPrompt { session_id, prompt } => {
            context.startup_prompts.withdraw(
                super::io::NamedSession {
                    name: super::io::session_notice_name(&context.controller.state, &session_id),
                    id: session_id,
                },
                prompt,
                context.dashboard_io_tx.clone(),
                context.critical_operations.clone(),
            );
        }
        DashboardAction::RenameSession { session_id, title } => {
            context.dashboard.set_notice("Renaming session…");
            let session_name = context.session_notice_name(&session_id);
            spawn_dashboard_rename(
                session_id,
                &session_name,
                title,
                context.dashboard_io_tx.clone(),
                context.critical_operations.clone(),
            );
        }
        DashboardAction::ChangeWorkspace {
            session_id,
            workspace_id,
            workspace_name,
        } => {
            context.dashboard.set_notice("Moving session…");
            let session_name = context.session_notice_name(&session_id);
            spawn_dashboard_change_workspace(
                session_id,
                &session_name,
                workspace_id,
                workspace_name,
                context.dashboard_io_tx.clone(),
                context.critical_operations.clone(),
            );
        }
        DashboardAction::SaveContainerSettings {
            session_id,
            cpus,
            memory,
            additional_mounts,
            mount_history,
        } => {
            context
                .dashboard
                .set_notice("Saving container size and mounts…");
            let session_name = context.session_notice_name(&session_id);
            spawn_dashboard_container_settings(
                ContainerSettingsRequest {
                    session_id,
                    cpus,
                    memory,
                    additional_mounts,
                    mount_history,
                },
                &session_name,
                context.dashboard_io_tx.clone(),
                context.critical_operations.clone(),
            );
        }
        DashboardAction::DiscoverProjects {
            context: picker_context,
            request,
        } => {
            let (cancelled, _) = spawn_cancellable_io_with_token(
                context.critical_operations.clone(),
                "finding projects",
                context.dashboard_io_tx.clone(),
                move |cancelled| match super::io::machine_request(
                    MachineAction::DiscoverProjects { request },
                    cancelled,
                )? {
                    MachineReply::ProjectDiscovery(result) => Ok(result),
                    reply => anyhow::bail!("unexpected discovery reply: {reply:?}"),
                },
                move |result| DashboardIoUpdate::ProjectDiscovery {
                    context: picker_context,
                    result,
                },
            );
            context.track_completion(cancelled);
        }
        DashboardAction::CompletePath { host, kind, prefix } => {
            let input_context = context.dashboard.path_input_context();
            let prefix_for_work = prefix.clone();
            let (cancelled, _) = spawn_cancellable_io_with_token(
                context.critical_operations.clone(),
                "completing path",
                context.dashboard_io_tx.clone(),
                move |cancelled| match super::io::machine_request(
                    MachineAction::CompletePath {
                        host,
                        prefix: prefix_for_work,
                        kind,
                    },
                    cancelled,
                )? {
                    MachineReply::PathCompletion(result) => Ok(result),
                    reply => anyhow::bail!("unexpected completion reply: {reply:?}"),
                },
                move |result| DashboardIoUpdate::PathCompletions {
                    context: input_context,
                    prefix,
                    result,
                },
            );
            context.track_completion(cancelled);
        }
        DashboardAction::ValidateMountSource {
            target_template_id,
            source,
        } => {
            let input_context = context.dashboard.path_input_context();
            context.dashboard.set_notice("Checking attached directory…");
            let requested = source.clone();
            let (cancelled, _) = spawn_cancellable_io_with_token(
                context.critical_operations.clone(),
                "validating attached directory",
                context.dashboard_io_tx.clone(),
                move |cancelled| match super::io::machine_request(
                    MachineAction::ValidateMountSource {
                        target_id: target_template_id,
                        source: requested.into(),
                    },
                    cancelled,
                )? {
                    MachineReply::MountValidation(path, forced) => Ok((path, forced)),
                    reply => anyhow::bail!("unexpected mount reply: {reply:?}"),
                },
                move |result| DashboardIoUpdate::MountValidation {
                    context: input_context,
                    source,
                    result,
                },
            );
            context.track_path_input(cancelled);
        }
        DashboardAction::ValidateSessionMounts {
            target_template_id,
            mounts,
            launch,
        } => {
            context
                .dashboard
                .set_notice("Checking attached directories…");
            let generation = context.dashboard.session_preflight_generation();
            spawn_cancellable_io(
                context.critical_operations.clone(),
                "validating session directories",
                context.dashboard_io_tx.clone(),
                move |cancelled| match super::io::machine_request(
                    MachineAction::ValidateSessionMounts {
                        target_id: target_template_id,
                        mounts,
                    },
                    cancelled,
                )? {
                    MachineReply::SessionMountValidation(result) => Ok(result),
                    reply => anyhow::bail!("unexpected mounts reply: {reply:?}"),
                },
                move |result| DashboardIoUpdate::SessionMountValidation {
                    generation,
                    launch,
                    result,
                },
            );
        }
        DashboardAction::PreflightResumeRepositories { launch } => {
            start_resume_repository_preflight(context, launch)?;
        }
        DashboardAction::ConfirmRawConversion { launch, receipt } => {
            start_preflighted_session_launch(context, *launch, *receipt);
        }
        DashboardAction::ReplaceResumeRepositoryOrigin {
            session_id,
            repository_id,
            replacement,
            launch,
        } => {
            context.dashboard.set_notice("Checking replacement origin…");
            let submitted_repository_id = repository_id.clone();
            let generation = context.dashboard.session_preflight_generation();
            spawn_cancellable_io(
                context.critical_operations.clone(),
                format!(
                    "updating repository for {}",
                    context.session_notice_name(&session_id)
                ),
                context.dashboard_io_tx.clone(),
                move |_cancelled| {
                    let preflight = mj_core::runtime::block_on(async {
                        daemon::connect_or_start()
                            .await?
                            .replace_resume_repository_origin(
                                session_id,
                                repository_id,
                                replacement,
                            )
                            .await
                    })??;
                    Ok(ResumeRepositoryPreflightApply { preflight })
                },
                move |result| DashboardIoUpdate::ResumeRepositoryPreflight {
                    generation,
                    launch,
                    submitted_repository_id: Some(submitted_repository_id),
                    result: Box::new(result),
                },
            );
        }
        DashboardAction::ValidateProjectDirectory {
            target_template_id,
            directory,
        } => {
            let input_context = context.dashboard.path_input_context();
            context.dashboard.set_notice("Checking project directory…");
            let requested = directory.clone();
            let (cancelled, _) = spawn_cancellable_io_with_token(
                context.critical_operations.clone(),
                "validating project directory",
                context.dashboard_io_tx.clone(),
                move |cancelled| match super::io::machine_request(
                    MachineAction::ValidateProjectDirectory {
                        target_id: target_template_id,
                        directory: requested.into(),
                    },
                    cancelled,
                )? {
                    MachineReply::NewSessionPreflight(result) => Ok((
                        result
                            .project_directory
                            .context("bare preflight returned no directory")?,
                        result.managed_worktree,
                        result.review_notice.is_some(),
                    )),
                    reply => anyhow::bail!("unexpected project preflight reply: {reply:?}"),
                },
                move |result| DashboardIoUpdate::ProjectValidation {
                    context: input_context,
                    directory,
                    result,
                },
            );
            context.track_path_input(cancelled);
        }
        DashboardAction::PreflightCreateSession { launch } => {
            start_create_session_preflight(context, *launch);
        }
        DashboardAction::RepairRepositoryRemotes {
            bundle_id,
            repairs,
            retry,
        } => {
            context.dashboard.set_notice("Repairing Git tracking…");
            let generation = matches!(*retry, DashboardAction::PreflightCreateSession { .. })
                .then(|| context.dashboard.session_preflight_generation());
            if let Some(generation) = generation {
                context.cancel_session_preflight();
                context.dashboard.begin_remote_session_preflight(generation);
            }
            let (cancelled, _) = spawn_cancellable_io_with_token(
                context.critical_operations.clone(),
                "repairing Git tracking",
                context.dashboard_io_tx.clone(),
                move |_cancelled| {
                    mj_core::runtime::block_on(async {
                        daemon::connect_or_start()
                            .await?
                            .repair_repository_remotes(bundle_id, repairs)
                            .await
                    })?
                },
                move |result| DashboardIoUpdate::RepositoryRemotesRepaired {
                    generation,
                    retry,
                    result,
                },
            );
            if let Some(generation) = generation {
                context.session_preflight_cancel = Some((generation, cancelled));
            }
        }
        action @ DashboardAction::CreateSession { .. } => start_session_launch(context, action),
        DashboardAction::PreviewContainerUpgrade {
            session_id,
            upgrade_only,
        } => {
            context.dashboard.set_notice("Checking container upgrade…");
            let updates = context.dashboard_io_tx.clone();
            tokio::spawn(async move {
                let result = async {
                    daemon::connect_or_start()
                        .await?
                        .preview_container_upgrade(session_id.clone())
                        .await
                }
                .await
                .map_err(|e: anyhow::Error| format!("{e:#}"));
                report(
                    "checking container upgrade",
                    &updates,
                    DashboardIoUpdate::ContainerUpgradePreview {
                        session_id,
                        upgrade_only,
                        result,
                    },
                );
            });
        }
        DashboardAction::UpgradeContainer(request) => {
            let updates = context.dashboard_io_tx.clone();
            let guard = context
                .critical_operations
                .begin("submitting container upgrade");
            tokio::spawn(async move {
                let result = async {
                    daemon::connect_or_start()
                        .await?
                        .upgrade_container(request)
                        .await
                }
                .await
                .map_err(|e: anyhow::Error| format!("{e:#}"));
                drop(guard);
                report(
                    "queueing container upgrade",
                    &updates,
                    DashboardIoUpdate::ContainerUpgradeQueued { result },
                );
            });
        }
        DashboardAction::RestartSession {
            session_id,
            acknowledge_container_recreation,
        } => {
            let Some(session) = context.controller.state.sessions.get(&session_id).cloned() else {
                return Ok(());
            };
            let request =
                context.begin_lifecycle_operation(&session_id, SessionOperationKind::Resuming);
            context.dashboard.set_notice("Restarting session…");
            spawn_lifecycle_operation(
                request,
                context.critical_operations.clone(),
                move |_cancelled, admission| {
                    mj_core::runtime::block_on(async {
                        let mut daemon = daemon::connect_or_start().await?;
                        let receipt = admission
                            .submit(
                                &mut daemon,
                                daemon::SessionLifecycleRequest::Restart {
                                    session_id,
                                    acknowledge_container_recreation,
                                },
                            )
                            .await?;
                        daemon.wait_session_lifecycle(receipt).await
                    })??;
                    Ok(LifecycleSuccess::Resumed {
                        profile_id: session.last_profile,
                        target_id: session.target_template_id,
                    })
                },
            );
        }
        DashboardAction::Open { session_id } => {
            context.open_chat_session(&session_id);
        }
        DashboardAction::PinSession { session_id, pane } => {
            if context.dashboard.pane_session(pane).is_none() {
                context.pin_session_in(&session_id, pane);
            } else {
                context
                    .dashboard
                    .set_notice("That pane is no longer empty.");
            }
        }
        DashboardAction::UnpinSession { session_id } => context.unpin_session(&session_id),
        DashboardAction::SplitConversation { pane, direction } => {
            context.split_conversation_pane(pane, direction)
        }
        DashboardAction::OpenSessionInSplit {
            session_id,
            direction,
        } => {
            context.open_session_in_split(&session_id, direction);
        }
        DashboardAction::SplitPane { direction } => context.split_empty_pane(direction),
        DashboardAction::ClosePane { pane } => context.close_pane(pane),
        DashboardAction::ConversationPanesChanged { focus_moved } => {
            if focus_moved {
                // The keyboard is in a different conversation now, so the
                // in-flight-attach report and any selection follow it.
                context.sync_opening_session();
                context.selection.clear();
            }
            context.save_active_workspace_layout();
        }
        action @ DashboardAction::ResumeSession { .. } => start_session_launch(context, action),
        action @ DashboardAction::MoveSession {
            preparation_request_id: Some(_),
            ..
        } => start_move_preparation(context, action),
        DashboardAction::MoveSession {
            session_id,
            preparation_request_id: None,
            queue,
            ..
        } => {
            let Some(preparation) = context.dashboard.take_move_preparation(&session_id) else {
                context.dashboard.set_failure_notice(
                    "Move confirmation expired; prepare the move again.".to_owned(),
                );
                return Ok(());
            };
            context.dashboard.set_notice(format!(
                "Moving {}…",
                context.session_notice_name(&session_id)
            ));
            context
                .startup_prompts
                .begin_move(&session_id, Some(preparation.operation_id.clone()));
            let request =
                context.begin_lifecycle_operation(&session_id, SessionOperationKind::Moving);
            context.dashboard.set_standby_move_origin(
                &session_id,
                context.startup_prompts.move_origin(&session_id),
            );
            spawn_lifecycle_operation(
                request,
                context.critical_operations.clone(),
                move |cancelled, admission| {
                    mj_core::runtime::block_on(async {
                        let mut daemon = daemon::connect_or_start().await?;
                        let receipt = admission
                            .submit(
                                &mut daemon,
                                daemon::SessionLifecycleRequest::Move(Box::new(
                                    MoveSessionRequest {
                                        preparation,
                                        queue,
                                        acknowledge_interruption: true,
                                    },
                                )),
                            )
                            .await?;
                        let operation = daemon.wait_session_lifecycle(receipt);
                        let mut operation = Box::pin(operation);
                        let wait_for_cancel = async {
                            loop {
                                if cancelled.load(Ordering::Acquire) {
                                    break;
                                }
                                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                            }
                        };
                        let outcome = tokio::select! {
                            result = &mut operation => result?,
                            () = wait_for_cancel => {
                                drop(operation);
                                let cancel = async {
                                    let mut client = daemon::connect_or_start().await?;
                                    client.cancel_lifecycle(session_id.clone()).await
                                };
                                tokio::time::timeout(std::time::Duration::from_secs(10), cancel)
                                    .await
                                    .context("request move cancellation")??;
                                bail!("move cancellation requested for {session_id}");
                            }
                        };
                        let outcome = lifecycle_move_outcome(outcome)?;
                        if matches!(outcome.outcome.as_str(), "failed" | "cancelled") {
                            bail!(
                                "move {}: {}",
                                outcome.outcome,
                                outcome.error.as_deref().unwrap_or("no further details")
                            );
                        }
                        Ok(LifecycleSuccess::Moved(outcome))
                    })?
                },
            );
        }
        DashboardAction::RetryMove { operation } => {
            let operation = *operation;
            let session_id = operation.selection.session_id.clone();
            if !operation.queue_admission_started
                && context
                    .controller
                    .state
                    .sessions
                    .get(&session_id)
                    .is_some_and(|session| session.state.is_active())
            {
                context.dashboard.begin_move_recovery(operation);
                return Ok(());
            }
            context.dashboard.set_notice(format!(
                "Retrying move for {}…",
                context.session_notice_name(&session_id)
            ));
            context.startup_prompts.begin_move(&session_id, None);
            let request =
                context.begin_lifecycle_operation(&session_id, SessionOperationKind::Moving);
            context.dashboard.set_standby_move_origin(
                &session_id,
                context.startup_prompts.move_origin(&session_id),
            );
            let prepared_updates = context.dashboard_io_tx.clone();
            spawn_lifecycle_operation(
                request,
                context.critical_operations.clone(),
                move |cancelled, admission| {
                    let selection = operation.selection.clone();
                    let queue = Some(operation.queue);
                    let operation_id = operation.operation_id.clone();
                    let session_id = session_id.clone();
                    mj_core::runtime::block_on(async move {
                        let mut daemon = daemon::connect_or_start().await?;
                        let preparation = daemon.prepare_move_session(selection).await?;
                        report(
                            "binding move prompt origin",
                            &prepared_updates,
                            DashboardIoUpdate::MoveOriginPrepared {
                                session_id: session_id.clone(),
                                move_operation_id: preparation.operation_id.clone(),
                                operation: cancelled.clone(),
                            },
                        );
                        let request = MoveSessionRequest {
                            preparation,
                            queue,
                            acknowledge_interruption: true,
                        };
                        let receipt = admission
                            .submit(
                                &mut daemon,
                                daemon::SessionLifecycleRequest::Move(Box::new(request)),
                            )
                            .await?;
                        let mut move_future = Box::pin(daemon.wait_session_lifecycle(receipt));
                        let wait_for_cancel = async {
                            loop {
                                if cancelled.load(Ordering::Acquire) {
                                    break;
                                }
                                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                            }
                        };
                        let outcome = tokio::select! {
                            result = &mut move_future => result?,
                            () = wait_for_cancel => {
                                drop(move_future);
                                let cancel = async {
                                    let mut client = daemon::connect_or_start().await?;
                                    client.cancel_lifecycle(session_id.clone()).await
                                };
                                tokio::time::timeout(std::time::Duration::from_secs(10), cancel)
                                    .await
                                    .context("request move cancellation")??;
                                bail!("move cancellation requested for {session_id}");
                            }
                        };
                        let outcome = lifecycle_move_outcome(outcome)?;
                        if matches!(outcome.outcome.as_str(), "failed" | "cancelled") {
                            bail!(
                                "move retry {} (operation {}): {}",
                                outcome.outcome,
                                operation_id,
                                outcome.error.as_deref().unwrap_or("no further details")
                            );
                        }
                        Ok(LifecycleSuccess::Moved(outcome))
                    })?
                },
            );
        }
        DashboardAction::ResumeMove { operation } => {
            let operation = *operation;
            let workspace_id = operation
                .recovery_session
                .as_ref()
                .map(|session| session.workspace_id.clone())
                .unwrap_or_else(|| context.workspace_id.clone());
            let action = DashboardAction::ResumeSession {
                workspace_id,
                session_id: operation.selection.session_id,
                profile_id: operation.source_profile_id,
                target_template_id: operation.source_target_template_id,
                additional_mounts: operation.source_additional_mounts,
                resource_allocation: operation.source_resource_allocation,
                discard_queue: true,
            };
            start_session_launch(context, action);
        }
        DashboardAction::Suspend {
            session_id,
            acknowledge_unpublished_work,
        } => start_suspension(context, session_id, acknowledge_unpublished_work, false),
        DashboardAction::IgnoreNestedRepositoriesAndSuspend { session_id } => {
            start_suspension(context, session_id, true, true);
        }

        DashboardAction::CreateBundle { sources } => {
            context.dashboard.set_notice("Preparing project…");
            spawn_create_bundle(
                sources,
                context.dashboard_io_tx.clone(),
                context.critical_operations.clone(),
            );
        }
        DashboardAction::RemoveBundle { bundle_id } => {
            spawn_remove_bundle(
                bundle_id,
                context.dashboard_io_tx.clone(),
                context.critical_operations.clone(),
            );
        }
        DashboardAction::DiscardSinceCheckpoint {
            session_id,
            checkpoint,
        } => {
            let request =
                context.begin_lifecycle_operation(&session_id, SessionOperationKind::Suspending);
            mark_active_chat_retiring(context.chats.get_mut(&session_id), &session_id);
            spawn_lifecycle_operation(
                request,
                context.critical_operations.clone(),
                move |_cancelled, admission| {
                    mj_core::runtime::block_on(async {
                        let mut daemon = daemon::connect_or_start().await?;
                        let receipt = admission
                            .submit(
                                &mut daemon,
                                daemon::SessionLifecycleRequest::Discard {
                                    session_id,
                                    checkpoint,
                                },
                            )
                            .await?;
                        daemon.wait_session_lifecycle(receipt).await
                    })??;
                    Ok(LifecycleSuccess::ForceStopped)
                },
            );
        }
        DashboardAction::DestroyStopped {
            session_id,
            delete_branch,
        } => {
            let request =
                context.begin_lifecycle_operation(&session_id, SessionOperationKind::Destroying);
            mark_active_chat_retiring(context.chats.get_mut(&session_id), &session_id);
            spawn_lifecycle_operation(
                request,
                context.critical_operations.clone(),
                move |_cancelled, admission| {
                    mj_core::runtime::block_on(async {
                        let mut daemon = daemon::connect_or_start().await?;
                        let receipt = admission
                            .submit(
                                &mut daemon,
                                daemon::SessionLifecycleRequest::DestroyStopped {
                                    session_id,
                                    delete_branch,
                                },
                            )
                            .await?;
                        daemon.wait_session_lifecycle(receipt).await
                    })??;
                    Ok(LifecycleSuccess::DestroyedStopped)
                },
            );
        }
        DashboardAction::ForceDestroy {
            session_id,
            delete_branch,
        } => {
            let request =
                context.begin_lifecycle_operation(&session_id, SessionOperationKind::Destroying);
            mark_active_chat_retiring(context.chats.get_mut(&session_id), &session_id);
            spawn_lifecycle_operation(
                request,
                context.critical_operations.clone(),
                move |_cancelled, admission| {
                    mj_core::runtime::block_on(async {
                        let mut daemon = daemon::connect_or_start().await?;
                        let receipt = admission
                            .submit(
                                &mut daemon,
                                daemon::SessionLifecycleRequest::ForceDestroy {
                                    session_id,
                                    delete_branch,
                                },
                            )
                            .await?;
                        daemon.wait_session_lifecycle(receipt).await
                    })??;
                    Ok(LifecycleSuccess::ForceDestroyed)
                },
            );
        }
        DashboardAction::CancelOperation { session_id, kind } => {
            if let Some(operation) = context.lifecycle_operations.get(&session_id) {
                operation.cancelled.store(true, Ordering::Release);
            }
            let daemon_session_id = session_id.clone();
            let updates = context.dashboard_io_tx.clone();
            tokio::spawn(async move {
                let result = async {
                    daemon::connect_or_start()
                        .await?
                        .cancel_lifecycle(daemon_session_id.clone())
                        .await
                }
                .await
                .map_err(|error: anyhow::Error| format!("{error:#}"));
                report(
                    "cancelling a lifecycle operation",
                    &updates,
                    DashboardIoUpdate::LifecycleCancellation {
                        session_id: daemon_session_id,
                        result,
                    },
                );
            });
            context.dashboard.set_notice(format!(
                "Cancelling {} for {}…",
                kind.label().to_ascii_lowercase(),
                context.session_notice_name(&session_id)
            ));
        }
    }
    Ok(())
}

/// Begin read-only preparation from either wizard input or an attachment reply.
pub(crate) fn start_move_preparation(context: &mut DashboardContext, action: DashboardAction) {
    let DashboardAction::MoveSession {
        subagents,
        oracle,
        oracle_choice,
        workspace_selection,
        session_id,
        profile_id,
        target_template_id,
        additional_mounts,
        resource_allocation,
        clear_resource_allocation,
        preparation_request_id: Some(request_id),
        ..
    } = action
    else {
        return;
    };
    let updates = context.dashboard_io_tx.clone();
    tokio::spawn(async move {
        let result = async {
            let mut daemon = daemon::connect_or_start().await?;
            daemon
                .prepare_move_session(MoveSelection {
                    oracle,
                    oracle_choice,
                    subagents,
                    workspace: workspace_selection,
                    session_id: session_id.clone(),
                    profile_id: Some(profile_id),
                    target_template_id: Some(target_template_id),
                    additional_mounts: Some(additional_mounts),
                    resource_allocation,
                    clear_resource_allocation,
                })
                .await
        }
        .await
        .map_err(|error: anyhow::Error| format!("{error:#}"));
        report(
            "preparing a move",
            &updates,
            DashboardIoUpdate::MovePrepared {
                session_id,
                request_id,
                result,
            },
        );
    });
}

fn resume_launch_destination(action: &DashboardAction) -> Result<(&str, &str)> {
    match action {
        DashboardAction::ResumeSession {
            session_id,
            target_template_id,
            ..
        } => Ok((session_id, target_template_id)),
        _ => bail!("repository preflight did not receive a resume launch"),
    }
}

pub(crate) fn start_resume_repository_preflight(
    context: &mut DashboardContext,
    launch: Box<DashboardAction>,
) -> Result<()> {
    let generation = context.dashboard.session_preflight_generation();
    let (session_id, target_id) = resume_launch_destination(&launch)?;
    let session_id = session_id.to_owned();
    let target_id = target_id.to_owned();
    context
        .dashboard
        .set_notice("Checking checkpoint repository history…");
    super::io::spawn_io(
        "checking checkpoint repositories",
        context.dashboard_io_tx.clone(),
        move || {
            let preflight = mj_core::runtime::block_on(async {
                daemon::connect_or_start()
                    .await?
                    .resume_repository_preflight(session_id, target_id)
                    .await
            })??;
            Ok(ResumeRepositoryPreflightApply { preflight })
        },
        move |result| DashboardIoUpdate::ResumeRepositoryPreflight {
            generation,
            launch,
            submitted_repository_id: None,
            result: Box::new(result),
        },
    );
    Ok(())
}

/// Resolve the network plan for an isolated creation while the wizard remains
/// visible. Every Git and network operation is supervised by the cancellable
/// blocking worker; the terminal event loop only receives the finished plan.
pub(crate) fn start_create_session_preflight(
    context: &mut DashboardContext,
    launch: DashboardAction,
) {
    let DashboardAction::CreateSession {
        bundle_id,
        target_template_id,
        ..
    } = &launch
    else {
        fail_create_session_preflight(context, "invalid session preflight request".to_owned());
        return;
    };
    let Some(target) = context.controller.config.targets.get(target_template_id) else {
        fail_create_session_preflight(
            context,
            format!("unknown target template {target_template_id:?}"),
        );
        return;
    };
    if is_bare_project_target(target) {
        // Raw local targets have no network source plan and are validated by
        // the existing project-directory flow.
        context.dashboard.finish_session_mount_preflight();
        start_session_launch(context, launch);
        return;
    }
    let DashboardAction::CreateSession {
        profile_id,
        workspace_id,
        additional_mounts,
        ..
    } = &launch
    else {
        unreachable!()
    };
    let request = mj_client::daemon::NewSessionPreflightRequest {
        profile_id: profile_id.clone(),
        workspace_id: workspace_id.clone(),
        bundle_id: bundle_id.clone(),
        target_id: target_template_id.clone(),
        project_directory: None,
        additional_mounts: additional_mounts.clone(),
        remote_repairs: Vec::new(),
    };
    let generation = context.dashboard.session_preflight_generation();
    context.cancel_session_preflight();
    context.dashboard.begin_remote_session_preflight(generation);
    let (cancelled, _) = spawn_cancellable_io_with_token(
        context.critical_operations.clone(),
        "resolving session repositories",
        context.dashboard_io_tx.clone(),
        move |cancelled| match super::io::machine_request(
            MachineAction::PreflightNewSession { request },
            cancelled,
        )? {
            MachineReply::NewSessionPreflight(result) => {
                Ok(super::io::RemotePreflightOutcome::from(result))
            }
            reply => anyhow::bail!("unexpected new-session preflight reply: {reply:?}"),
        },
        move |result| DashboardIoUpdate::RemotePreflight {
            generation,
            launch: Box::new(launch),
            result,
        },
    );
    context.session_preflight_cancel = Some((generation, cancelled));
}

/// Ends a creation check that could not start, so the review offers Retry
/// instead of waiting on a check that is not running.
fn fail_create_session_preflight(context: &mut DashboardContext, error: String) {
    let generation = context.dashboard.session_preflight_generation();
    context
        .dashboard
        .apply_remote_session_preflight(generation, Err(error.clone()));
    context.dashboard.set_failure_notice(error);
}

pub(crate) fn start_session_launch(context: &mut DashboardContext, action: DashboardAction) {
    start_session_launch_with_repository_preflight(context, action, None);
}

pub(crate) fn start_preflighted_session_launch(
    context: &mut DashboardContext,
    action: DashboardAction,
    repository_preflight: ResumeRepositorySourceReceipt,
) {
    start_session_launch_with_repository_preflight(context, action, Some(repository_preflight));
}

fn start_session_launch_with_repository_preflight(
    context: &mut DashboardContext,
    action: DashboardAction,
    repository_preflight: Option<ResumeRepositorySourceReceipt>,
) {
    let workspace_available = match &action {
        DashboardAction::CreateSession { workspace_id, .. }
        | DashboardAction::ResumeSession { workspace_id, .. } => !workspace_id.is_empty(),
        _ => context.dashboard.active_workspace_id().is_some(),
    };
    if !workspace_available {
        context
            .dashboard
            .set_notice("Select a workspace before starting a session.");
        return;
    }
    match action {
        action @ DashboardAction::CreateSession { .. } => {
            debug_assert!(repository_preflight.is_none());
            context.dashboard.set_notice("Preparing session launch…");
            let go_save = context.dashboard.remember_go_launch(&action);
            spawn_dashboard_create_session(
                action,
                go_save,
                context.dashboard_io_tx.clone(),
                context.lifecycle_updates_tx.clone(),
                context.critical_operations.clone(),
            );
        }
        DashboardAction::ResumeSession {
            workspace_id,
            session_id,
            profile_id,
            target_template_id,
            additional_mounts,
            resource_allocation,
            discard_queue,
        } => {
            context.dashboard.set_notice(resume_progress_notice(
                &context.session_notice_name(&session_id),
                &profile_id,
                &target_template_id,
            ));
            // Resume places historical work in the workspace where the
            // explicit request began, even if another tab is selected later.
            let request =
                context.begin_lifecycle_operation(&session_id, SessionOperationKind::Resuming);
            context.dashboard.set_resume_destination(
                &session_id,
                profile_id.clone(),
                target_template_id.clone(),
            );
            spawn_lifecycle_operation(
                request,
                context.critical_operations.clone(),
                move |_cancelled, admission| {
                    mj_core::runtime::block_on(async {
                        let mut daemon = daemon::connect_or_start().await?;
                        let receipt = admission
                            .submit(
                                &mut daemon,
                                daemon::SessionLifecycleRequest::Resume(Box::new(
                                    daemon::ResumeSessionRequest {
                                        session_id: session_id.clone(),
                                        workspace_id,
                                        profile_id: profile_id.clone(),
                                        target_template_id: target_template_id.clone(),
                                        additional_mounts: Some(additional_mounts),
                                        resource_allocation,
                                        discard_queue,
                                        repository_preflight,
                                    },
                                )),
                            )
                            .await?;
                        daemon.wait_session_lifecycle(receipt).await
                    })??;
                    Ok(LifecycleSuccess::Resumed {
                        profile_id,
                        target_id: target_template_id,
                    })
                },
            );
        }
        _ => unreachable!("mount preflight only carries create or resume actions"),
    }
}

/// Tells the open chat, if it is this session's, that the session is being
/// retired on purpose. The stop closes the relay feed, and the chat must read
/// that as the expected end rather than a lost actor to chase.
pub(crate) fn mark_active_chat_retiring(
    active_chat: Option<&mut mj_chat::chat::ActiveChat>,
    session_id: &str,
) {
    if let Some(chat) = active_chat.filter(|chat| chat.session_id() == session_id) {
        chat.set_session_retiring(true);
    }
}

impl DashboardContext {
    /// Marks a session busy in the UI and hands back what runs its operation
    /// off the loop. The daemon owns lifecycle/recovery serialization.
    fn begin_lifecycle_operation(
        &mut self,
        session_id: &str,
        kind: SessionOperationKind,
    ) -> LifecycleOperationRequest {
        self.cancel_chat_open_for(session_id);
        self.dashboard
            .begin_session_operation(session_id.to_owned(), kind, None);
        // Park this session's composer without changing navigation. A Move
        // can belong to an unfocused pane; its draft must stay with it.
        if matches!(
            kind,
            SessionOperationKind::Launching
                | SessionOperationKind::Resuming
                | SessionOperationKind::Moving
        ) {
            if !self.dashboard.has_standby_composer(session_id)
                && let Some(chat) = self.chats.get_mut(session_id)
                && let Some(composer) = chat.take_live_composer()
            {
                self.dashboard.adopt_standby_composer(session_id, composer);
            }
            if kind != SessionOperationKind::Moving {
                self.dashboard.set_standby_move_origin(session_id, None);
            }
            // Resume is an explicit request to open a suspended conversation.
            if kind == SessionOperationKind::Resuming {
                self.dashboard.select_active_session(session_id);
                self.dashboard.focus_prompt();
            }
        }
        let cancelled = Arc::new(AtomicBool::new(false));
        let notice_name = self.session_notice_name(session_id);
        self.lifecycle_operations.insert(
            session_id.to_owned(),
            crate::dashboard::io::ActiveLifecycleOperation {
                retirement: Some(super::attachment::ChatRetirement::capture(
                    session_id,
                    &self.chats,
                    &self.attachments,
                )),
                cancelled: cancelled.clone(),
                kind,
                retry_launch: None,
                notice_name,
            },
        );
        LifecycleOperationRequest {
            session_id: session_id.to_owned(),
            session_name: self.session_notice_name(session_id),
            kind,
            cancelled,
            updates: self.lifecycle_updates_tx.clone(),
        }
    }
}

/// Own each dialog request, cancel stale polling, and observe background task failures.
/// Restart the daemon from this surface, off the event loop.
///
/// Stopping and starting a daemon takes seconds and holds a file lock, so it
/// can never run on the render loop. The outcome is a sentence for the notice
/// bar rather than a silent success, for the same reason `mj daemon restart`
/// now names the build that came up: a restart that lands on someone else's
/// build must not look like the one that was asked for.
fn spawn_daemon_restart(context: &mut DashboardContext) {
    let updates = context.dashboard_io_tx.clone();
    let guard = context
        .critical_operations
        .begin("restarting the Mjolnir daemon");
    context
        .dashboard
        .set_notice("Restarting the Mjolnir daemon...");
    tokio::spawn(async move {
        let result = daemon::restart_daemon().await.map_or_else(
            |error| Err(format!("Could not restart the Mjolnir daemon: {error:#}")),
            |restarted| {
                Ok(match restarted.runs_this_build {
                    Some(true) => format!(
                        "Mjolnir daemon restarted as PID {}, running this build.",
                        restarted.pid
                    ),
                    _ => format!("Mjolnir daemon restarted as PID {}.", restarted.pid),
                })
            },
        );
        if let Err(error) = updates.send(DashboardIoUpdate::DaemonRestarted(result)) {
            tracing::warn!(%error, "daemon restart outcome dropped because the surface closed");
        }
        drop(guard);
    });
}

/// Move the daemon where a saved `[daemon]` setting says, off the event loop.
///
/// Startup owns that decision: it compares where the daemon runs with the
/// configuration and hands off to a daemon launched in the right place, so
/// workers keep running. This only asks it now rather than at the next start.
pub(super) fn spawn_daemon_placement(context: &mut DashboardContext) {
    let updates = context.dashboard_io_tx.clone();
    let guard = context
        .critical_operations
        .begin("moving the Mjolnir daemon");
    context
        .dashboard
        .set_notice("Moving the Mjolnir daemon where Settings say...");
    tokio::spawn(async move {
        let result = daemon::connect_or_start().await.map_or_else(
            |error| Err(format!("Could not move the Mjolnir daemon: {error:#}")),
            |_| Ok("The Mjolnir daemon runs where Settings say.".to_owned()),
        );
        if let Err(error) = updates.send(DashboardIoUpdate::DaemonRestarted(result)) {
            tracing::warn!(%error, "daemon move outcome dropped because the surface closed");
        }
        drop(guard);
    });
}

fn spawn_web_request(context: &mut DashboardContext, action: DashboardAction) {
    context.web_request_cancel = None;
    context.web_request_generation = context.web_request_generation.wrapping_add(1);
    let generation = context.web_request_generation;
    let cancellation = tokio_util::sync::CancellationToken::new();
    context.web_request_cancel = Some(cancellation.clone().drop_guard());
    let updates = context.dashboard_io_tx.clone();
    let task_updates = updates.clone();
    let inspecting = matches!(action, DashboardAction::InspectWebListener);
    let mut task = tokio::spawn(async move {
        let mut daemon = daemon::connect_existing().await?;
        match action {
            DashboardAction::RecoverWebViewer(recovery) => {
                if let Err(error) = daemon.recover_web_viewer(recovery).await {
                    // Another client may have recovered it since this dialog loaded.
                    let access = daemon.web_access().await?;
                    if matches!(access, WebViewerAccess::Failed { .. }) {
                        return Err(error);
                    }
                }
            }
            DashboardAction::InspectWebListener => {
                let result = daemon
                    .inspect_web_listener()
                    .await
                    .map_err(|error| format!("{error:#}"));
                task_updates.send(DashboardIoUpdate::WebListeners { generation, result })?;
                return Ok::<(), anyhow::Error>(());
            }
            _ => {}
        }
        loop {
            let access = daemon.web_access().await?;
            let starting = matches!(access, WebViewerAccess::Starting);
            task_updates.send(DashboardIoUpdate::WebAccess { generation, access })?;
            if !starting {
                return Ok(());
            }
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        }
    });
    tokio::spawn(async move {
        let result = tokio::select! {
            result = &mut task => result.context("web viewer request task failed").and_then(std::convert::identity),
            _ = cancellation.cancelled() => {
                task.abort();
                match task.await {
                    Ok(Err(error)) => tracing::warn!(%error, "web viewer request failed during cancellation"),
                    Err(error) if !error.is_cancelled() => tracing::warn!(%error, "web viewer request task failed during cancellation"),
                    _ => {},
                }
                return;
            }
        };
        if let Err(error) = result {
            let error = format!("Could not complete the web viewer request: {error:#}");
            tracing::warn!(%error, "web viewer request failed");
            let update = if inspecting {
                DashboardIoUpdate::WebListeners {
                    generation,
                    result: Err(error),
                }
            } else {
                DashboardIoUpdate::WebAccessError { generation, error }
            };
            report("web viewer request", &updates, update);
        }
    });
}

/// Ends each turn Interrupt all found, through the daemon, so a sub-agent's
/// turn stops whether or not this terminal has its conversation open. One
/// failure does not keep the rest from being interrupted; each is returned
/// with the session or native sub-agent it belongs to.
async fn interrupt_all(targets: &mj_tui::InterruptAllTargets) -> Vec<(String, String)> {
    let mut client = match daemon::connect_existing().await {
        Ok(client) => client,
        Err(error) => {
            let error = format!("{error:#}");
            return targets
                .sessions
                .iter()
                .cloned()
                .chain(
                    targets
                        .native
                        .iter()
                        .map(|(owner, child)| mj_core::native_agent::view_id(owner, child)),
                )
                .map(|id| (id, error.clone()))
                .collect();
        }
    };
    let mut failures = Vec::new();
    for session_id in &targets.sessions {
        let result = async {
            let command_id = mj_client::session::new_command_id("tui-interrupt-all")?;
            client
                .submit_session_command(
                    session_id.clone(),
                    command_id,
                    mj_core::relay::RelayCommand::CancelTurn,
                    None,
                )
                .await
        }
        .await;
        if let Err(error) = result {
            failures.push((session_id.clone(), format!("{error:#}")));
        }
    }
    for (owner, child) in &targets.native {
        if let Err(error) = client
            .stop_background_task(owner.clone(), format!("native-agent:{child}"))
            .await
        {
            failures.push((
                mj_core::native_agent::view_id(owner, child),
                format!("{error:#}"),
            ));
        }
    }
    failures
}

fn lifecycle_move_outcome(reply: daemon::DaemonReply) -> Result<mj_core::state::MoveOutcome> {
    match reply {
        daemon::DaemonReply::MoveOutcome(outcome) => Ok(outcome),
        _ => bail!("daemon returned an unexpected move reply"),
    }
}

#[cfg(test)]
async fn dispatch_restart(
    daemon: &mut daemon::DaemonClient,
    session_id: String,
    acknowledge_container_recreation: bool,
) -> Result<()> {
    daemon
        .restart_session_with_ack(session_id, acknowledge_container_recreation)
        .await
}

/// Run `mj login` for one profile with the dashboard's terminal handed over.
///
/// The interactive child must own standard input for browser and device-code
/// prompts, so the event stream is dropped before the child starts and the
/// terminal is restored on every path, including a failed spawn.
async fn run_setup_login(context: &mut DashboardContext, profile_id: &str) -> Result<(), String> {
    let executable =
        std::env::current_exe().map_err(|error| format!("find the mj executable: {error}"))?;
    // The event stream must be gone before the child owns stdin. Crossterm
    // stops its reader thread when the stream is dropped, so releasing the
    // last handle here (rather than after the child) is what makes the
    // handover safe.
    let events = context.events.take();
    drop(events);
    if let Err(error) = context.terminal.suspend() {
        context.events = Some(crossterm::event::EventStream::new());
        return Err(format!("{error:#}"));
    }
    let profile = profile_id.to_owned();
    // Ctrl+C belongs to the login child, not to the dashboard's shutdown
    // coordinator, while the terminal is handed over.
    let _interrupts = mj_controller::termination::suppress_interrupts();
    // The interactive child cannot run on a runtime worker. The dashboard
    // waits for the handover, but the Tokio worker remains available for the
    // daemon's other tasks.
    let joined = tokio::task::spawn_blocking(move || {
        std::process::Command::new(executable)
            .args([
                "--instance",
                &mj_core::config::instance_name().unwrap_or_else(|| "default".to_owned()),
                "login",
                "--profile",
                &profile,
            ])
            .status()
    })
    .await;
    // Restore the terminal on every path: a failed spawn or a panicking child
    // must not leave the dashboard on the primary screen with no input.
    let resumed = context.terminal.resume();
    context.events = Some(crossterm::event::EventStream::new());
    if let Err(error) = resumed {
        context.request_shutdown();
        return Err(format!("{error:#}"));
    }
    let status = joined
        .map_err(|error| format!("sign-in task failed: {error}"))?
        .map_err(|error| format!("could not start `mj login --profile {profile_id}`: {error}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!(
            "`mj login --profile {profile_id}` exited with {status}; rerun it from a shell for the full output"
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mj_chat::chat::{ActiveChat, Notices, SessionHeaderIdentity};

    fn open_chat(session_id: &str) -> ActiveChat {
        let fixture = mj_client::session::replacement_session_test_fixture(session_id, 1);
        ActiveChat::open(
            fixture.stopped,
            "bundle-1",
            None,
            fixture.control,
            SessionHeaderIdentity::default(),
            String::new(),
            Notices::default(),
        )
    }

    /// Stopping a session retires its relay actor, which closes the open
    /// chat's feed. The chat is told before that happens so it reads the
    /// closure as the expected end of a deliberate stop.
    // Hard-won: 66c73159: an intentional stop triggered a misleading feed-loss warning and reconnect loop.
    #[tokio::test]
    async fn stopping_the_open_chats_session_marks_that_chat_retiring() {
        let mut chat = open_chat("session-open");
        assert!(!chat.session_retiring());

        mark_active_chat_retiring(Some(&mut chat), "session-open");

        assert!(chat.session_retiring());
    }

    #[tokio::test]
    async fn stopping_another_session_leaves_the_open_chat_alone() {
        let mut chat = open_chat("session-open");

        mark_active_chat_retiring(Some(&mut chat), "session-other");

        assert!(!chat.session_retiring());
    }

    // Hard-won: f2fc48f3: Restart removed the workspace and temporary volumes before restarting.
    #[tokio::test]
    async fn restart_dispatch_sends_one_daemon_restart_action() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let metadata = daemon::DaemonMetadata {
            protocol_version: daemon::PROTOCOL_VERSION,
            pid: std::process::id(),
            address: listener.local_addr().unwrap(),
            token: "restart-test".into(),
            started_at: "test".into(),
            build_version: env!("CARGO_PKG_VERSION").into(),
            instance_id: String::new(),
            placement: crate::daemon::DaemonPlacement::Host,
        };
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let request: daemon::RequestEnvelope = daemon::read_frame(&mut stream).await.unwrap();
            assert!(matches!(
                request.action,
                daemon::DaemonAction::RestartSession { session_id, acknowledge_container_recreation: true } if session_id == "session-1"
            ));
            daemon::write_frame(
                &mut stream,
                &daemon::ResponseEnvelope {
                    protocol_version: request.protocol_version,
                    request_id: request.request_id,
                    result: Ok(daemon::DaemonReply::Done),
                },
            )
            .await
            .unwrap();
        });
        let mut client = daemon::DaemonClient::connect(metadata).await.unwrap();

        dispatch_restart(&mut client, "session-1".into(), true)
            .await
            .unwrap();
        server.await.unwrap();
    }
}
