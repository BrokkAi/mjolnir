use super::*;

pub(super) async fn handle_action(
    action: DaemonAction,
    metadata: &DaemonMetadata,
    state: &Arc<RuntimeState>,
    cancellation: &CancellationToken,
) -> Result<DaemonReply> {
    let _work = upgrade_request_activity(&action)?;
    match action {
        DaemonAction::LoginTerminal { .. } => {
            bail!("login requires a dedicated terminal connection")
        }
        action @ (DaemonAction::PreviewBuildCache { .. }
        | DaemonAction::InstallMbx { .. }
        | DaemonAction::CheckpointArchiveSizes { .. }
        | DaemonAction::TestTarget { .. }
        | DaemonAction::DiscoverSetup { .. }
        | DaemonAction::ProfileLoginStatuses
        | DaemonAction::PreflightNewSession { .. }
        | DaemonAction::SuspendPreflight { .. }
        | DaemonAction::ValidateProjectDirectory { .. }
        | DaemonAction::CompletePath { .. }
        | DaemonAction::CheckProjectDirectories { .. }
        | DaemonAction::DiscoverProjects { .. }
        | DaemonAction::ResolveContainerPath { .. }
        | DaemonAction::ResolveSetupPath { .. }
        | DaemonAction::ValidateMountSource { .. }
        | DaemonAction::ValidateSessionMounts { .. }) => {
            super::machine::execute(action, state, Arc::new(AtomicBool::new(false))).await
        }
        DaemonAction::CheckAvailability => {
            state.request_availability_check()?;
            Ok(DaemonReply::Done)
        }
        DaemonAction::UploadChunk { .. } => bail!("upload fragment outside upload transport"),
        DaemonAction::InstallAttachment { .. } | DaemonAction::TranscribeAudio { .. } => {
            bail!("blob action outside connection transport")
        }
        DaemonAction::TranscriptionAvailable { preferred_profile } => {
            let config = state.with_config(Clone::clone);
            let available =
                blocking(move || Ok(transcription_auth(&config, &preferred_profile).is_some()))
                    .await?;
            Ok(DaemonReply::TranscriptionAvailable(available))
        }
        DaemonAction::RecordOperatorActivity => {
            state.record_operator_activity().await;
            Ok(DaemonReply::Done)
        }
        DaemonAction::PrepareUpgrade => {
            if mj_controller::upgrade::gate().try_close() {
                cancellation.cancel();
                Ok(DaemonReply::Done)
            } else {
                Ok(DaemonReply::UpgradePending)
            }
        }
        DaemonAction::SetOperatorAway { away } => {
            state.set_operator_away(away).await?;
            Ok(DaemonReply::Done)
        }
        DaemonAction::ProveIdentity { .. } => {
            unreachable!("identity proof is served before authentication")
        }
        DaemonAction::Ping => Ok(DaemonReply::Pong),
        // Each blocker carries its count and age, so the waiting client can say
        // what the upgrade waits for and for how long.
        DaemonAction::UpgradeBlockers => Ok(DaemonReply::UpgradeBlockers(
            mj_controller::upgrade::blockers()
                .iter()
                .map(ToString::to_string)
                .collect(),
        )),
        DaemonAction::Identity => Ok(DaemonReply::Identity(DaemonIdentity {
            instance_id: metadata.instance_id.clone(),
            build_version: metadata.build_version.clone(),
            placement: metadata.placement.clone(),
        })),
        DaemonAction::Configuration => Ok(DaemonReply::Configuration(Box::new(
            state.with_config(Clone::clone),
        ))),
        DaemonAction::FirstRunSetupDue => {
            // Not `blocking`: this only reads the setup marker and the
            // configuration, so it takes no upgrade admission.
            let due = mj_core::runtime::spawn_blocking(|| {
                mj_setup::setup::automatic_setup_due(
                    &mj_core::config::config_path(),
                    &mj_core::config::data_dir(),
                )
            })
            .await
            .context("daemon first-run check task panicked")??;
            Ok(DaemonReply::FirstRunSetupDue(due))
        }
        DaemonAction::RunFirstRunSetup | DaemonAction::RunFirstRunSetupIn { .. } => {
            let project_directory = match action {
                DaemonAction::RunFirstRunSetupIn { project_directory } => Some(project_directory),
                _ => None,
            };
            let report = blocking(move || {
                let executor = mj_controller::targets::ProcessExecutor;
                mj_setup::setup::run_setup(
                    &mj_core::config::config_path(),
                    &mj_core::config::data_dir(),
                    project_directory.as_deref(),
                    &executor,
                    &|| false,
                    || {},
                )
            })
            .await?;
            if report.is_some() {
                refresh_runtime_controller(state).await;
            }
            Ok(DaemonReply::FirstRunSetup(report))
        }
        DaemonAction::RunDoctor => {
            // Not `blocking`: the doctor only reads, so it takes no upgrade
            // admission, and a handoff abandons it instead of waiting for it.
            let errors = mj_core::runtime::spawn_blocking(|| {
                let executor = mj_setup::doctor::probe_executor();
                let checks = mj_setup::doctor::run_with_config_path(
                    &mj_core::config::config_path(),
                    &executor,
                    mj_setup::doctor::current_apple_platform(&executor),
                    mj_setup::doctor::DoctorOptions { smoke: false },
                );
                mj_setup::setup::welcome_errors(&checks)
            })
            .await
            .context("daemon doctor task panicked")?;
            Ok(DaemonReply::DoctorErrors(errors))
        }
        DaemonAction::NewSessionRemoteRepairs {
            bundle_id,
            target_id,
        } => {
            let controller = state.with_config(|config| {
                mj_controller::controller::config_only_controller(config.clone())
            });
            let repairs = blocking(move || {
                let executor = mj_controller::targets::CancellableProcessExecutor::new(
                    std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
                )
                .with_deadline(Duration::from_secs(30));
                let result =
                    controller.preflight_new_session(&bundle_id, &target_id, None, &executor)?;
                Ok(result.remote_repairs)
            })
            .await?;
            Ok(DaemonReply::RemoteRepairs(repairs))
        }
        DaemonAction::RepairRepositoryRemotes { bundle_id, repairs } => {
            let controller = state.with_config(|config| {
                mj_controller::controller::config_only_controller(config.clone())
            });
            blocking(move || {
                let bundle = controller
                    .config
                    .bundles
                    .get(&bundle_id)
                    .context("unknown bundle")?;
                let executor = mj_controller::targets::CancellableProcessExecutor::new(
                    std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
                )
                .with_deadline(Duration::from_secs(30));
                mj_core::local_git::apply_repository_remote_repairs(bundle, &repairs, &executor)
            })
            .await?;
            refresh_runtime_controller(state).await;
            Ok(DaemonReply::Done)
        }
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
        DaemonAction::ApiCredential => Ok(DaemonReply::ApiCredential(
            state
                .api_credential
                .get()
                .context("the HTTP API credential is not ready")?
                .clone(),
        )),
        DaemonAction::WebViewerAccess => {
            Ok(DaemonReply::WebViewerAccess(state.web_viewer.access()))
        }
        // Git runs in the daemon's environment, which is the one its
        // sessions inherit; a client's environment may differ.
        DaemonAction::GitIdentity => {
            mj_core::runtime::spawn_blocking(mj_controller::controller::controller_git_identity)
                .await
                .context("Git identity read panicked")?
                .map(DaemonReply::GitIdentity)
        }
        DaemonAction::SetGitIdentity { name, email } => {
            mj_core::runtime::spawn_blocking(move || {
                mj_controller::controller::set_controller_git_identity(&name, &email)
            })
            .await
            .context("Git identity save panicked")?
            .map(DaemonReply::GitIdentity)
        }
        DaemonAction::SubagentOptions {
            profile,
            model,
            config,
        } => {
            // Discovery is restartable preparation and must not hold up handoff.
            tokio::select! {
                biased;
                _ = cancellation.cancelled() => bail!("subagent discovery cancelled by daemon shutdown"),
                options = async move {
                    let config = match config {
                        Some(config) => *config,
                        None => state.worker_controller_projection().config,
                    };
                    state.profile_catalog.get().context("profile catalogue unavailable")?
                        .options_for(&config, &profile, model).await
                } => {
                    Ok(DaemonReply::SubagentOptions(options?))
                }
            }
        }
        DaemonAction::WarmProfileCapabilities { config } => {
            state
                .profile_catalog
                .get()
                .context("profile catalogue unavailable")?
                .ensure(&config);
            Ok(DaemonReply::Done)
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
            state.refresh_workspaces().await?;
            let workspaces = state.workspaces().borrow().clone();
            Ok(DaemonReply::Workspaces(
                workspaces
                    .into_iter()
                    .map(|workspace| WorkspaceListing { workspace })
                    .collect(),
            ))
        }
        DaemonAction::ListWorkspaceDrafts => {
            let drafts = blocking(mj_controller::database::list_all_detached_drafts).await?;
            let mut grouped: Vec<WorkspaceDrafts> = Vec::new();
            for draft in drafts {
                let preview = DraftPreview {
                    id: draft.id,
                    session_id: draft.session_id,
                    source: draft.source,
                    owner_pid: draft.owner_pid,
                    saved_at: draft.saved_at,
                };
                match grouped.last_mut() {
                    Some(group) if group.workspace_id == draft.workspace_id => {
                        group.drafts.push(preview);
                    }
                    _ => grouped.push(WorkspaceDrafts {
                        workspace_id: draft.workspace_id,
                        drafts: vec![preview],
                    }),
                }
            }
            Ok(DaemonReply::WorkspaceDrafts(grouped))
        }
        DaemonAction::CreateWorkspace { name } => {
            // Two setup selectors can both have observed an empty workspace
            // list. The daemon operation is create-or-get so both attach to
            // the same normalized name instead of leaking a SQLite conflict.
            let workspace =
                blocking(move || mj_controller::database::create_or_get_workspace(&name)).await?;
            refresh_runtime_workspaces(state).await?;
            Ok(DaemonReply::Workspace(workspace))
        }
        DaemonAction::RenameWorkspace { workspace_id, name } => {
            blocking(move || mj_controller::database::rename_workspace(&workspace_id, &name))
                .await?;
            refresh_runtime_workspaces(state).await?;
            Ok(DaemonReply::Done)
        }
        DaemonAction::TouchWorkspace { workspace_id } => {
            blocking(move || mj_controller::database::touch_workspace(&workspace_id)).await?;
            refresh_runtime_workspaces(state).await?;
            Ok(DaemonReply::Done)
        }
        DaemonAction::CloseWorkspace { workspace_id } => {
            state.close_workspace(workspace_id).await?;
            Ok(DaemonReply::Done)
        }
        DaemonAction::CancelWorkspaceClose { workspace_id } => {
            state.cancel_workspace_close(&workspace_id)?;
            Ok(DaemonReply::Done)
        }
        DaemonAction::DeleteWorkspace { workspace_id } => {
            state.close_workspace(workspace_id).await?;
            Ok(DaemonReply::Done)
        }
        DaemonAction::Attach { client_id } => {
            state.attachments().insert(
                client_id,
                Attachment {
                    renewed: std::time::Instant::now(),
                },
            );
            state.ever_attached.store(true, Ordering::Release);
            Ok(DaemonReply::Done)
        }
        DaemonAction::Detach { client_id } => {
            state.attachments().remove(&client_id);
            Ok(DaemonReply::Done)
        }
        DaemonAction::DismissNotification { session_id, target } => {
            let ordinal = state.dismiss_notification(session_id, target).await?;
            Ok(DaemonReply::Ordinal(ordinal))
        }
        DaemonAction::PersistReadReceipt {
            client_id,
            workspace_id,
            session_id,
            through,
        } => {
            let frontier = blocking(move || {
                mj_controller::database::persist_read_receipt(
                    &client_id,
                    &workspace_id,
                    &session_id,
                    through,
                )
            })
            .await?;
            Ok(DaemonReply::Ordinal(frontier))
        }
        DaemonAction::PersistTerminalDraft {
            client_id,
            session_id,
            owner_pid,
            revision,
            draft,
        } => {
            blocking(move || {
                mj_controller::database::save_terminal_draft(
                    &session_id,
                    &client_id,
                    owner_pid,
                    revision,
                    draft,
                )
                .map(|_| ())
            })
            .await?;
            Ok(DaemonReply::Done)
        }
        DaemonAction::RecordAbandonedPrompt {
            session_id,
            bundle_id,
            text,
        } => {
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
            Ok(DaemonReply::Done)
        }
        DaemonAction::SaveSpinnerStyle { style } => {
            // Hold the configuration owner through persistence and publication:
            // an acknowledged save must be visible to the next dashboard.
            let mut inactivity = state.config_mutation.lock().await;
            let config = blocking(move || {
                Config::update(|config| {
                    config.spinner = style;
                    Ok(())
                })
                .map(|(config, ())| config)
            })
            .await?;
            state.configure_inactivity(&mut inactivity, config.advanced.auto_afk_minutes);
            state
                .review_config
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clone_from(&config.review);
            state.owner().install_config(config);
            state.publish_revision();
            Ok(DaemonReply::Done)
        }
        DaemonAction::SetQuotaPreferences { prefer } => {
            // Hold the configuration owner through persistence and publication:
            // an acknowledged save must be visible to the next dashboard.
            let mut inactivity = state.config_mutation.lock().await;
            let config = blocking(move || {
                Config::update(|config| {
                    config.quota.prefer = prefer;
                    Ok(())
                })
                .map(|(config, ())| config)
            })
            .await?;
            state.configure_inactivity(&mut inactivity, config.advanced.auto_afk_minutes);
            state
                .review_config
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clone_from(&config.review);
            state.owner().install_config(config);
            state.publish_revision();
            Ok(DaemonReply::Done)
        }
        DaemonAction::SaveSidebarSize { size } => {
            // Hold the configuration owner through persistence and publication:
            // an acknowledged save must be visible to the next dashboard.
            let mut inactivity = state.config_mutation.lock().await;
            let config = blocking(move || {
                Config::update(|config| {
                    config.sidebar_size = size;
                    Ok(())
                })
                .map(|(config, ())| config)
            })
            .await?;
            state.configure_inactivity(&mut inactivity, config.advanced.auto_afk_minutes);
            state
                .review_config
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clone_from(&config.review);
            state.owner().install_config(config);
            state.publish_revision();
            Ok(DaemonReply::Done)
        }
        DaemonAction::SaveWorkspacePaneSizes {
            workspace_id,
            sizes,
        } => {
            blocking(move || {
                mj_controller::database::save_workspace_pane_sizes(&workspace_id, sizes)
            })
            .await?;
            Ok(DaemonReply::Done)
        }
        DaemonAction::SaveWorkspaceLayout {
            workspace_id,
            layout,
        } => {
            blocking(move || mj_controller::database::save_workspace_layout(&workspace_id, layout))
                .await?;
            Ok(DaemonReply::Done)
        }
        DaemonAction::ImportSession { .. } => bail!("blob action outside connection transport"),
        DaemonAction::SessionProjectSource { session_id } => {
            let controller =
                state.with_controller(|controller| mj_controller::controller::Controller {
                    config: controller.config.clone(),
                    state: controller.state.clone(),
                });
            let source = blocking(move || {
                let executor = mj_controller::targets::CancellableProcessExecutor::new(
                    std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
                )
                .with_deadline(Duration::from_secs(8));
                controller.resolve_session_project_source(&session_id, &executor)
            })
            .await?;
            Ok(DaemonReply::ProjectSource(source))
        }
        DaemonAction::SetSessionTitle { session_id, title } => {
            let title =
                blocking(move || Controller::load()?.rename_session(&session_id, &title)).await?;
            refresh_runtime_controller(state).await;
            Ok(DaemonReply::Text(title))
        }
        DaemonAction::SetSessionNotificationsIgnored {
            session_id,
            ignored,
        } => {
            blocking(move || {
                Controller::load()?.set_session_notifications_ignored(&session_id, ignored)
            })
            .await?;
            refresh_runtime_controller(state).await;
            Ok(DaemonReply::Done)
        }
        DaemonAction::SetSessionWorkspace {
            session_id,
            workspace_id,
        } => {
            ensure!(
                !mj_controller::controller::move_session::move_owns_session(&session_id),
                "session is moving; change its workspace after Move finishes"
            );
            blocking(move || Controller::load()?.set_session_workspace(&session_id, &workspace_id))
                .await?;
            refresh_runtime_controller(state).await;
            Ok(DaemonReply::Done)
        }
        DaemonAction::SetSessionContainerSettings {
            session_id,
            resource_allocation,
            mounts,
            mount_history,
        } => {
            let settings = mj_core::state::ContainerSettingsChange {
                resource_allocation,
                mounts,
                mount_history: Some(mount_history),
            };
            Ok(
                match state.set_container_settings(session_id, settings).await? {
                    Some(preview) => DaemonReply::ContainerUpgradePreview(preview),
                    None => DaemonReply::Done,
                },
            )
        }
        DaemonAction::MarkSessionTargetMissing {
            session_id: _,
            detail: _,
            updated_at: _,
        } => {
            // Older clients still send this action. Only the daemon's worker
            // view owns a definitive missing-target observation and its
            // revision; a client-supplied detail may already be stale.
            Ok(DaemonReply::OptionalSessionState(None))
        }
        DaemonAction::CheckpointSession { .. } => {
            anyhow::bail!(mj_client::daemon::REMOVED_CHECKPOINT_MESSAGE)
        }
        DaemonAction::ProjectCatalog { refresh, retry } => Ok(DaemonReply::ProjectCatalog(
            state.project_catalog(refresh, retry).await?,
        )),
        DaemonAction::CreateProject { sources } => {
            let created = state.create_bundle_from_sources(sources, false).await?;
            let id = created.bundle_id;
            let bundle = created
                .config
                .bundles
                .get(&id)
                .context("created project is missing")?
                .clone();
            let project = blocking(move || {
                mj_controller::project_catalog::snapshot(
                    &bundle,
                    &CancellableProcessExecutor::with_timeout(Duration::from_secs(15)),
                    false,
                )
            })
            .await?;
            Ok(DaemonReply::ProjectCreated(
                mj_core::project_catalog::SavedProject {
                    bundle_id: id,
                    name: project.name(),
                    project,
                },
            ))
        }
        DaemonAction::RemoveProject { bundle_id } => {
            state.remove_project(bundle_id).await?;
            Ok(DaemonReply::Done)
        }
        DaemonAction::WikiSyncRequest => {
            let target = state.wiki().request_sync(false);
            let mut status = state.wiki().state();
            status.target = Some(target);
            Ok(DaemonReply::WikiSyncStatus(status))
        }
        DaemonAction::WikiSyncWatch { epoch, after } => Ok(DaemonReply::WikiSyncStatus(
            state.wiki().watch_after(epoch, after).await?,
        )),
        DaemonAction::WikiSearch { query, limit } => Ok(DaemonReply::WikiRows(
            state.wiki_search(query, limit).await?,
        )),
        DaemonAction::SessionTextSearch { query } => Ok(DaemonReply::SessionTextMatches(
            state.session_text_search(query).await?,
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
        DaemonAction::WikiSession { wiki_id } => Ok(DaemonReply::WikiSession(
            state.wiki_session(wiki_id).await?.map(Box::new),
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
        DaemonAction::AdoptRecovery(_) => {
            bail!("adoption requires its dedicated cancellable connection")
        }
        DaemonAction::DestroyRecovery {
            session_id,
            target_id,
            confirmation,
            all_instances,
        } => {
            ensure_no_active_lifecycle(state)?;
            let admission = state
                .session_gate
                .enter_foreground(
                    &session_id,
                    "destroy recovered environment",
                    mj_controller::recovery_gate::LifecycleOrigin::Person,
                    &AtomicBool::new(false),
                    &ProcessExecutor,
                )
                .await?;
            let owner =
                mj_controller::worker_lifecycle::WorkerPermit::acquire(&session_id, &admission)?;
            blocking(move || {
                owner.scope_blocking(|| {
                    Controller::load()?.destroy_orphan_worker(
                        &session_id,
                        &target_id,
                        &confirmation,
                        all_instances,
                        &ProcessExecutor,
                    )
                })
            })
            .await?;
            Ok(DaemonReply::Done)
        }
        DaemonAction::Snapshot { workspace_id } => {
            let snapshot = blocking(move || workspace_snapshot(&workspace_id)).await?;
            Ok(DaemonReply::Snapshot(snapshot))
        }
        DaemonAction::DashboardStartup {
            client_id,
            workspace_id,
        } => {
            let stored = blocking(move || dashboard_reads::dashboard_stored(&workspace_id)).await?;
            let frame = state
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
            Ok(DaemonReply::DashboardStartup(Box::new(DashboardStartup {
                runtime: mj_client::runtime_feed::RuntimeBootstrap {
                    cursor,
                    projection: *projection,
                },
                metadata: stored.metadata,
                pane_sizes: stored.layouts.pane_sizes,
                conversation_layouts: stored.layouts.conversation_layouts,
                queued_prompts,
            })))
        }
        DaemonAction::WorkspaceLayouts { workspace_ids } => Ok(DaemonReply::WorkspaceLayouts(
            blocking(move || dashboard_reads::workspace_layouts(workspace_ids)).await?,
        )),
        DaemonAction::StoredSessionMetadata => Ok(DaemonReply::StoredSessionMetadata(
            blocking(dashboard_reads::stored_session_metadata).await?,
        )),
        DaemonAction::StoredProjectionTail { session_id, items } => {
            let tail = blocking(move || {
                mj_controller::database::load_materialized_projection_tail(&session_id, items)
            })
            .await?;
            Ok(DaemonReply::StoredProjectionTail(
                tail.map(|(session, _)| Box::new(session)),
            ))
        }
        DaemonAction::StoredTranscriptItem {
            session_id,
            stable_id,
        } => {
            let item = blocking(move || {
                mj_controller::database::load_materialized_transcript_item(&session_id, &stable_id)
            })
            .await?;
            Ok(DaemonReply::StoredTranscriptItem(item.map(Box::new)))
        }
        DaemonAction::StoredAgentMessageThrough {
            session_id,
            through,
        } => {
            let item = blocking(move || {
                mj_controller::database::load_materialized_agent_item_through(&session_id, through)
            })
            .await?;
            Ok(DaemonReply::StoredTranscriptItem(item.map(Box::new)))
        }
        DaemonAction::StoredSessionSummary { session_id } => {
            let summary = blocking(move || {
                mj_controller::database::load_materialized_session_summary(&session_id)
            })
            .await?;
            Ok(DaemonReply::StoredSessionSummary(summary.map(Box::new)))
        }
        DaemonAction::ResumeRepositoryPreflight {
            session_id,
            target_id,
        } => {
            let controller = blocking(Controller::load).await?;
            let executor = repository_preflight_executor();
            let preflight = controller
                .preflight_resume_repository_sources(&session_id, &target_id, &executor)
                .await?;
            Ok(DaemonReply::ResumeRepositoryPreflight(Box::new(preflight)))
        }
        DaemonAction::ReplaceResumeRepositoryOrigin {
            session_id,
            repository_id,
            replacement,
        } => {
            ensure!(
                !mj_controller::controller::move_session::move_owns_session(&session_id),
                "session is moving; repair its repository after Move finishes"
            );
            let mut controller = blocking(Controller::load).await?;
            let executor = repository_preflight_executor();
            let preflight = controller
                .replace_resume_repository_origin(
                    &session_id,
                    &repository_id,
                    &replacement,
                    &executor,
                )
                .await?;
            refresh_runtime_controller(state).await;
            Ok(DaemonReply::ResumeRepositoryPreflight(Box::new(preflight)))
        }
        DaemonAction::ArchiveSpacePreview { older_than_days } => {
            Ok(DaemonReply::ArchiveSpacePreview(
                blocking(move || {
                    mj_controller::sessionwiki::archive_space_preview(older_than_days)
                })
                .await?,
            ))
        }
        DaemonAction::SaveSetup { original, updated } => {
            let saved = blocking(move || {
                let sessions = mj_controller::database::load_state()?;
                mj_client::settings::save::save_at(
                    &mj_core::config::config_path(),
                    &original,
                    &updated,
                    &sessions,
                )
            })
            .await?;
            Ok(DaemonReply::SavedSetup(Box::new(saved)))
        }
        DaemonAction::RuntimeChanges {
            cursor,
            wait,
            subagent_parents,
        } => Ok(DaemonReply::RuntimeChanges(Box::new(
            state
                .runtime_changes_for_subagent_parents(cursor, wait, subagent_parents)
                .await?,
        ))),
        DaemonAction::SessionTail { session_id, cursor } => Ok(DaemonReply::SessionTail(Box::new(
            state.session_tail(&session_id, &cursor),
        ))),
        DaemonAction::ResumeCandidates => Ok(DaemonReply::ResumeCandidates(Box::new(
            state.resume_candidates(),
        ))),
        DaemonAction::SessionRecord { session_id } => {
            let capture = state.runtime_publication()?;
            Ok(DaemonReply::SessionRecord(
                capture.records.get(&session_id).map(|record| {
                    Box::new(SessionRecordWithVerdict {
                        record: record.clone(),
                        verdict: capture.verdicts[&session_id],
                    })
                }),
            ))
        }
        DaemonAction::GoStartupSession {
            workspace_id,
            last_session_id,
        } => Ok(DaemonReply::GoStartupSession(
            state
                .go_startup_session(&workspace_id, last_session_id.as_deref())
                .map(Box::new),
        )),
        DaemonAction::HostCapacity => Ok(DaemonReply::HostCapacity(
            state
                .capacity_feed()
                .context("the daemon capacity service is not running")?
                .host_rows(),
        )),
        DaemonAction::RefreshCapacity => {
            anyhow::ensure!(
                state
                    .capacity_feed()
                    .is_some_and(|feed| feed.request_refresh()),
                "the daemon capacity service is not running"
            );
            Ok(DaemonReply::Done)
        }
        DaemonAction::RefreshQuota => {
            state.request_quota_refresh()?;
            Ok(DaemonReply::Done)
        }
        DaemonAction::ProbeQuotaProfiles {
            profile_ids,
            forced,
        } => {
            let config = state.owner().controller().config.clone();
            let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));

            let mut requests = Vec::new();
            for id in &profile_ids {
                let profile = config
                    .profiles
                    .get(id)
                    .with_context(|| format!("no profile named {id:?} is configured"))?;
                requests.push(mj_controller::quota::QuotaRefreshRequest::for_profile(
                    id,
                    profile,
                    cwd.clone(),
                ));
            }
            state.probe_quota_profiles(requests, forced)?;
            Ok(DaemonReply::Done)
        }
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
        DaemonAction::InterruptAll { session_id } => Ok(DaemonReply::InterruptedAll(
            state.interrupt_all(&session_id).await?,
        )),
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
                Some(mj_core::transcript::materialized_content_text(values))
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
                state.owner().edit_sessions(|sessions| {
                    if let Some(record) = sessions.get_mut(&session_id)
                        && record.draft_input == expected
                    {
                        record.draft_input.clear();
                    }
                });
                state.publish_revision();
            }
            Ok(DaemonReply::Ordinal(ordinal))
        }
        DaemonAction::QueueStartupPrompt {
            session_id,
            command_id,
            text,
            prompt,
            encoded_draft,
            move_operation_id,
            inherited_draft,
        } => {
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
            match state
                .queue_startup_steps_with_ids(
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
                    cancellation,
                )
                .await
            {
                Ok(()) => Ok(DaemonReply::Done),
                Err(error) if error.is::<mj_client::daemon::StartupPromptIdentityConflict>() => {
                    Ok(DaemonReply::StartupPromptIdentityConflict)
                }
                Err(error) => Err(error),
            }
        }
        DaemonAction::WithdrawStartupPrompt {
            session_id,
            command_id,
        } => Ok(DaemonReply::PromptWithdrawn(
            state
                .withdraw_startup_prompt(&session_id, &command_id)
                .await?,
        )),
        DaemonAction::StartupPromptOutcomes {
            session_id,
            command_ids,
        } => {
            ensure!(
                command_ids.len() <= 128,
                "request at most 128 startup prompt outcomes"
            );
            Ok(DaemonReply::StartupPromptOutcomes(
                blocking(move || {
                    mj_controller::database::startup_prompt_outcomes(&session_id, &command_ids)
                })
                .await?,
            ))
        }
        DaemonAction::StartTurnReview { session_id } => {
            state
                .review_host()
                .start(&session_id)
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
            let target = state
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
            state
                .session_manager
                .session(session_id)
                .await?
                .respond_elicitation(elicitation_id, response)
                .await?;
            Ok(DaemonReply::Done)
        }
        DaemonAction::NativeAgentHistory {
            owner,
            child,
            before,
        } => {
            let page = blocking(move || {
                mj_controller::database::native_agent_history(&owner, &child, before)
            })
            .await?;
            Ok(DaemonReply::NativeAgentHistory(page))
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
        DaemonAction::StartSessionLifecycle(request) => {
            match state.start_session_lifecycle(request).await {
                Ok(receipt) => Ok(DaemonReply::LifecycleAccepted(receipt)),
                Err(error) if is_upgrade_handoff_refusal(&error) => {
                    // Only unaccepted lifecycle admission carries this code.
                    // The client retries its original request and selections.
                    Ok(DaemonReply::UpgradePending)
                }
                Err(error) => Err(error),
            }
        }
        DaemonAction::WaitSessionLifecycle(receipt) => state.wait_session_lifecycle(receipt).await,
        DaemonAction::SuspendSession {
            session_id,
            discard_untracked,
            abort_operations,
            without_checkpoint,
            confirm_no_checkpoint,
        } => {
            state
                .suspend_session_with_consent(
                    session_id,
                    mj_core::suspend_consent::SuspendConsent {
                        discard_untracked,
                        abort_operations,
                        without_checkpoint,
                        confirm_no_checkpoint,
                    },
                )
                .await?;
            Ok(DaemonReply::Done)
        }
        DaemonAction::PreviewContainerUpgrade { session_id } => {
            Ok(DaemonReply::ContainerUpgradePreview(
                state.preview_container_upgrade(session_id).await?,
            ))
        }
        DaemonAction::UpgradeContainer(request) => {
            state.upgrade_container(request).await?;
            Ok(DaemonReply::Done)
        }
        DaemonAction::RestartSession {
            session_id,
            acknowledge_container_recreation,
        } => {
            state
                .restart_session(session_id, acknowledge_container_recreation)
                .await?;
            Ok(DaemonReply::Done)
        }
        DaemonAction::StartCreateSession(request) => Ok(DaemonReply::RegisteredSession(Box::new(
            state.start_create_session(request).await?,
        ))),
        DaemonAction::WaitCreateSession { session_id } => {
            match state.wait_create_outcome(&session_id).await {
                crate::daemon::create::CreateWait::Finished => Ok(DaemonReply::Done),
                crate::daemon::create::CreateWait::Failed(error) => Err(error),
            }
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
        DaemonAction::MoveSources {
            session_id,
            cleanup_operation_id,
        } => {
            let admission = if cleanup_operation_id.is_some() {
                Some(
                    state
                        .session_gate
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
            let sources = blocking(move || {
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
            .await?;
            Ok(DaemonReply::MoveSources(sources))
        }
        DaemonAction::DestroyStoppedSession { session_id } => {
            match state
                .start_destroy_stopped_session(session_id, BranchDisposition::Keep)
                .await
            {
                Ok(started) => {
                    RuntimeState::wait_lifecycle_result(started.result).await?;
                    Ok(DaemonReply::Done)
                }
                Err(error) if is_upgrade_handoff_refusal(&error) => Ok(DaemonReply::UpgradePending),
                Err(error) => Err(error),
            }
        }
        DaemonAction::ForceDestroySession { session_id } => {
            state
                .force_destroy_session(session_id, BranchDisposition::Keep)
                .await?;
            Ok(DaemonReply::Done)
        }
        DaemonAction::CancelContainerUpgrade { session_id } => {
            state.cancel_container_upgrade(&session_id).await?;
            Ok(DaemonReply::Done)
        }
        DaemonAction::CancelLifecycle { session_id } => {
            blocking({
                let session_id = session_id.clone();
                move || mj_controller::database::request_move_cancellation(&session_id)
            })
            .await?;
            state.cancel_lifecycle_with_intent(&session_id).await?;
            Ok(DaemonReply::Done)
        }
        DaemonAction::RecoverDraft { draft_id } => {
            blocking(move || mj_controller::database::recover_detached_draft(&draft_id)).await?;
            Ok(DaemonReply::Done)
        }
        DaemonAction::Stop => {
            if cancellation.is_cancelled() {
                return Ok(DaemonReply::Done);
            }
            mj_controller::upgrade::gate().begin_draining();
            // A destroy in flight was acknowledged as accepted and nothing
            // about it is durable, so the stop waits for it, up to a bound
            // (#1191). The reply does not wait.
            if mj_controller::upgrade::gate().destroys_hold_the_stop() {
                let cancellation = cancellation.clone();
                mj_core::runtime::spawn(async move {
                    while mj_controller::upgrade::gate().destroys_hold_the_stop() {
                        tokio::time::sleep(Duration::from_millis(50)).await;
                    }
                    cancellation.cancel();
                });
            } else {
                cancellation.cancel();
            }
            Ok(DaemonReply::Done)
        }
    }
}

pub(super) fn upgrade_request_activity(
    action: &DaemonAction,
) -> Result<Option<mj_controller::upgrade::Work>> {
    if matches!(action, DaemonAction::InterruptAll { .. }) {
        return mj_controller::upgrade::activity("interrupt session work").map(Some);
    }
    if matches!(action, DaemonAction::WikiSyncRequest) {
        return mj_controller::upgrade::activity_unless_draining("wiki sync request").map(Some);
    }
    if let DaemonAction::PreflightNewSession { request } = action
        && !request.remote_repairs.is_empty()
    {
        return mj_controller::upgrade::activity("new-session preflight remote repairs").map(Some);
    }
    if matches!(action, DaemonAction::SaveSpinnerStyle { .. }) {
        return mj_controller::upgrade::activity("spinner style setting").map(Some);
    }
    if matches!(action, DaemonAction::DismissNotification { .. }) {
        return mj_controller::upgrade::activity("notification dismissal").map(Some);
    }
    if matches!(action, DaemonAction::SaveSidebarSize { .. }) {
        return mj_controller::upgrade::activity("sidebar size setting").map(Some);
    }
    if matches!(action, DaemonAction::RemoveProject { .. }) {
        return mj_controller::upgrade::activity("project removal").map(Some);
    }
    if matches!(action, DaemonAction::SetOperatorAway { .. }) {
        return mj_controller::upgrade::activity("operator away setting").map(Some);
    }
    if matches!(action, DaemonAction::LoginTerminal { .. })
        || action.is_cancellable_machine_action()
    {
        return Ok(None);
    }
    if matches!(
        action,
        DaemonAction::TranscribeAudio { .. }
            | DaemonAction::RecordOperatorActivity
            | DaemonAction::ProveIdentity { .. }
            | DaemonAction::Ping
            | DaemonAction::Identity
            | DaemonAction::Configuration
            | DaemonAction::WaitSessionLifecycle(_)
            | DaemonAction::DestroyStoppedSession { .. }
            // Answered from durable state, so it holds no daemon work and a
            // handoff does not wait for it; the client asks again.
            | DaemonAction::WaitCreateSession { .. }
            | DaemonAction::AdoptRecovery(_)
            | DaemonAction::WikiSyncWatch { .. }
            | DaemonAction::MoveSession(_)
            | DaemonAction::SetSessionContainerSettings { .. }
            | DaemonAction::Status
            | DaemonAction::PrepareUpgrade
            | DaemonAction::UpgradeBlockers
            | DaemonAction::Stop
            | DaemonAction::CheckAvailability
            | DaemonAction::RuntimeChanges { .. }
            | DaemonAction::SessionTail { .. }
            | DaemonAction::SubagentOptions { .. }
            | DaemonAction::WarmProfileCapabilities { .. }
            | DaemonAction::ProjectCatalog { .. }
            | DaemonAction::ImportSession { .. }
            // Read-only and repeatable; the client reports an interrupted run.
            | DaemonAction::RunDoctor
            | DaemonAction::FirstRunSetupDue
    ) {
        Ok(None)
    } else {
        mj_controller::upgrade::activity("client request").map(Some)
    }
}

pub(super) fn is_upgrade_handoff_refusal(error: &anyhow::Error) -> bool {
    Refusal::of(error)
        .is_some_and(|refusal| refusal.code() == Some(mj_controller::upgrade::HANDOFF_REFUSAL_CODE))
}

/// Repository preflights fetch over the network. The 30 seconds the terminal
/// always allowed them also bound how long a handoff waits for one.
fn repository_preflight_executor() -> CancellableProcessExecutor {
    CancellableProcessExecutor::new(Arc::new(AtomicBool::new(false)))
        .with_deadline(Duration::from_secs(30))
}

#[cfg(all(test, unix))]
mod missing_target_compat_tests {
    use super::*;

    // Hard-won: #1444: an old client could mark a newly resumed session missing from a stale poll.
    #[tokio::test]
    async fn stale_client_missing_target_report_cannot_invalidate_a_resumed_session() {
        const CHILD: &str = "MJ_TEST_STALE_MISSING_TARGET_ACTION_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let directory = tempfile::tempdir().unwrap();
            mj_controller::controller::test_support::IsolatedTest::new(
                mj_controller::controller::test_support::test_name(
                    module_path!(),
                    "stale_client_missing_target_report_cannot_invalidate_a_resumed_session",
                ),
            )
            .env(CHILD, "1")
            .isolated_store(directory.path())
            .run();
            return;
        }
        let _writer = mj_controller::database::install_isolated_test_writer();
        let workspace = mj_controller::database::create_workspace("Resumed session").unwrap();
        let mut session = super::super::tests::runtime_test_session(
            "resumed",
            &workspace.id,
            SessionState::Running,
        );
        session.updated_at = "2026-11-02T18:00:00Z".into();
        mj_controller::database::save_session(&session).unwrap();

        let state = super::super::tests::test_runtime_state();
        let metadata = super::super::tests::test_metadata("127.0.0.1:1".parse().unwrap());
        let reply = handle_action(
            DaemonAction::MarkSessionTargetMissing {
                session_id: session.id.clone(),
                detail: "old working directory was missing".into(),
                updated_at: "2026-11-02T17:00:00Z".into(),
            },
            &metadata,
            &state,
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        assert!(matches!(reply, DaemonReply::OptionalSessionState(None)));
        let stored = mj_controller::database::load_session_record(&session.id)
            .unwrap()
            .unwrap();
        assert_eq!(stored.state, SessionState::Running);
        assert_eq!(stored.updated_at, session.updated_at);
        assert_eq!(stored.last_error, None);
    }
}

/// Credential selection lives on the machine owning the profiles.
pub(super) fn transcription_auth(
    config: &mj_core::config::Config,
    preferred: &str,
) -> Option<PathBuf> {
    available_auth(auth_paths(config, preferred))
}

/// Find candidate Codex subscription auth files, preferring the session's
/// current profile and then sorting all remaining profile IDs.
fn auth_paths(config: &mj_core::config::Config, preferred: &str) -> Vec<PathBuf> {
    let mut profiles = config
        .profiles
        .iter()
        .filter(|(_, profile)| profile.kind == mj_core::config::HarnessKind::Codex)
        .collect::<Vec<_>>();
    profiles.sort_by_key(|(id, _)| (*id != preferred, *id));
    profiles
        .into_iter()
        .map(|(_, profile)| profile.home.join(profile.kind.credential_file_name()))
        .collect()
}

/// Return the first auth file containing complete ChatGPT-subscription OAuth
/// tokens. API-key auth files are deliberately skipped.
fn available_auth(paths: Vec<PathBuf>) -> Option<PathBuf> {
    paths.into_iter().find(
        |path| match anvil_client::codex_auth::read_auth_dot_json_at(path) {
            Ok(Some(auth)) => auth.tokens.is_some_and(|tokens| {
                !tokens.access_token.trim().is_empty()
                    && !tokens.refresh_token.trim().is_empty()
                    && !tokens.account_id.trim().is_empty()
            }),
            Ok(None) => false,
            Err(error) => {
                tracing::warn!(%error, "could not inspect Codex dictation credentials");
                false
            }
        },
    )
}

impl RuntimeState {
    pub(crate) async fn dismiss_notification(
        &self,
        session_id: String,
        target: mj_core::attention::NotificationDismissal,
    ) -> Result<u64> {
        use mj_core::attention::{NotificationDismissal, NotificationKind};

        let runtime = self.runtime_publication()?;
        if matches!(
            target,
            NotificationDismissal::Question(_) | NotificationDismissal::Failure(_)
        ) && runtime.metadata.notification_dismissals.get(&session_id) != Some(&target)
        {
            return Err(mj_core::refusal::Refusal::precondition(
                "notification is no longer current",
            )
            .into());
        }
        let (through, command_ordinal) = match target {
            NotificationDismissal::Failure(failure) => {
                blocking(move || {
                    mj_controller::database::dismiss_recorded_failure(&session_id, failure)
                })
                .await?;
                return Ok(0);
            }
            NotificationDismissal::Question(question) => {
                let through = question.completed_ordinal;
                let ordinal = self
                    .session_manager
                    .session(&session_id)
                    .await?
                    .submit(
                        format!("dismiss-question-{through}"),
                        RelayCommand::DismissQuestion { question },
                    )
                    .await?;
                (Some(through), Some(ordinal))
            }
            NotificationDismissal::ReadThrough(through) => (Some(through), None),
            NotificationDismissal::Stack { revision } => {
                let card = runtime
                    .metadata
                    .notification_cards
                    .as_ref()
                    .and_then(|cards| cards.get(&session_id))
                    .filter(|card| card.dismissal.as_ref() == Some(&target))
                    .ok_or_else(|| {
                        mj_core::refusal::Refusal::precondition(
                            "notification stack is no longer current",
                        )
                    })?;
                let through = card
                    .entries
                    .iter()
                    .filter(|entry| entry.kind == NotificationKind::Important)
                    .map(|entry| entry.completed_ordinal)
                    .max();
                let ordinal = self
                    .session_manager
                    .session(&session_id)
                    .await?
                    .submit(
                        format!("clear-notifications-{revision}"),
                        RelayCommand::ClearNotifications { revision },
                    )
                    .await?;
                (through, Some(ordinal))
            }
        };
        let Some(through) = through else {
            return Ok(command_ordinal.unwrap_or_default());
        };
        let workspace_id = runtime
            .records
            .get(&session_id)
            .context("session missing")?
            .workspace_id
            .clone();
        let frontier = blocking(move || {
            mj_controller::database::persist_read_receipt(
                "notification-dismissal",
                &workspace_id,
                &session_id,
                through,
            )
        })
        .await?;
        Ok(command_ordinal.unwrap_or(frontier))
    }
}
