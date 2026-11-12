use super::*;

pub(super) async fn handle_action(
    action: DaemonAction,
    metadata: &DaemonMetadata,
    state: &Arc<RuntimeState>,
    cancellation: &CancellationToken,
) -> Result<DaemonReply> {
    let _work = upgrade_request_activity(&action)?;
    match action {
        action @ (DaemonAction::PreviewBuildCache { .. }
        | DaemonAction::InstallMbx { .. }
        | DaemonAction::CheckpointArchiveSizes { .. }
        | DaemonAction::TestTarget { .. }
        | DaemonAction::DiscoverSetup { .. }
        | DaemonAction::CheckAvailability
        | DaemonAction::ProfileLoginStatuses
        | DaemonAction::PreflightNewSession { .. }
        | DaemonAction::ValidateProjectDirectory { .. }
        | DaemonAction::CompletePath { .. }
        | DaemonAction::CheckProjectDirectories { .. }
        | DaemonAction::DiscoverProjects { .. }
        | DaemonAction::ResolveContainerPath { .. }
        | DaemonAction::ResolveSetupPath { .. }
        | DaemonAction::ValidateMountSource { .. }
        | DaemonAction::ValidateSessionMounts { .. }) => {
            super::machine::execute(action, state, Arc::new(AtomicBool::new(false)))
                .await?
                .0
        }
        DaemonAction::RecordOperatorActivity => {
            state.record_operator_activity().await;
            Ok(DaemonReply::Done)
        }
        DaemonAction::PrepareUpgrade => {
            if crate::upgrade::gate().try_close() {
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
        DaemonAction::Ping => Ok(DaemonReply::Pong),
        // Each blocker carries its count and age, so the waiting client can say
        // what the upgrade waits for and for how long.
        DaemonAction::UpgradeBlockers => Ok(DaemonReply::UpgradeBlockers(
            crate::upgrade::blockers()
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
        DaemonAction::RunFirstRunSetup => {
            let report = blocking(|| {
                let executor = crate::targets::ProcessExecutor;
                let report = crate::setup::run_setup(
                    &mj_core::config::config_path(),
                    &mj_core::config::data_dir(),
                    false,
                    &executor,
                    &|| false,
                    || {},
                )?;
                Ok(report.map(|report| FirstRunSetupReport {
                    agents: report.agents,
                    repository: report.repository,
                }))
            })
            .await?;
            if report.is_some() {
                refresh_runtime_controller(state).await;
            }
            Ok(DaemonReply::FirstRunSetup(report))
        }
        DaemonAction::RunDoctor => {
            let errors = blocking(|| {
                // Daemon-owned work is bounded: one deadline per probe, as
                // `mj doctor` and `mj setup` have.
                let executor = crate::doctor::probe_executor();
                let checks = crate::doctor::run_with_config_path(
                    &mj_core::config::config_path(),
                    &executor,
                    crate::doctor::current_apple_platform(&executor),
                    crate::doctor::DoctorOptions { smoke: false },
                );
                Ok(crate::setup::actionable_errors(&checks))
            })
            .await?;
            Ok(DaemonReply::DoctorErrors(errors))
        }
        DaemonAction::NewSessionRemoteRepairs {
            bundle_id,
            target_id,
        } => {
            let controller = state
                .with_config(|config| crate::controller::config_only_controller(config.clone()));
            let repairs = blocking(move || {
                let executor = crate::targets::CancellableProcessExecutor::new(
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
            let controller = state
                .with_config(|config| crate::controller::config_only_controller(config.clone()));
            blocking(move || {
                let bundle = controller
                    .config
                    .bundles
                    .get(&bundle_id)
                    .context("unknown bundle")?;
                let executor = crate::targets::CancellableProcessExecutor::new(
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
        DaemonAction::WebViewerAccess => {
            Ok(DaemonReply::WebViewerAccess(state.web_viewer.access()))
        }
        // Git runs in the daemon's environment, which is the one its
        // sessions inherit; a client's environment may differ.
        DaemonAction::GitIdentity => {
            tokio::task::spawn_blocking(crate::controller::controller_git_identity)
                .await
                .context("Git identity read panicked")?
                .map(DaemonReply::GitIdentity)
        }
        DaemonAction::SetGitIdentity { name, email } => tokio::task::spawn_blocking(move || {
            crate::controller::set_controller_git_identity(&name, &email)
        })
        .await
        .context("Git identity save panicked")?
        .map(DaemonReply::GitIdentity),
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
        DaemonAction::PersistTerminalDraft {
            client_id,
            session_id,
            owner_pid,
            revision,
            draft,
        } => {
            blocking(move || {
                crate::database::save_terminal_draft(
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
                crate::database::record_abandoned_prompt(&session_id, &bundle_id, &text)
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
        DaemonAction::PersistImportedSession { session, bundle } => {
            blocking(move || {
                if let Some(imported) = bundle {
                    let ImportedBundle { bundle_id, bundle } = imported;
                    Config::update(|config| {
                        if let Some(existing) = config.bundles.get(&bundle_id) {
                            anyhow::ensure!(
                                existing == &bundle,
                                "bundle {bundle_id:?} changed during import; retry the import"
                            );
                        } else {
                            config.bundles.insert(bundle_id.clone(), bundle);
                        }
                        Ok(())
                    })?;
                }
                crate::import::persist_imported_session_locally(&session)
            })
            .await?;
            refresh_runtime_controller(state).await;
            Ok(DaemonReply::Done)
        }
        DaemonAction::SessionProjectSource { session_id } => {
            let controller = state.with_controller(|controller| crate::controller::Controller {
                config: controller.config.clone(),
                state: controller.state.clone(),
            });
            let source = blocking(move || {
                let executor = crate::targets::CancellableProcessExecutor::new(
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
        DaemonAction::SetSessionWorkspace {
            session_id,
            workspace_id,
        } => {
            ensure!(
                !crate::controller::move_session::move_owns_session(&session_id),
                "session is moving; change its workspace after Move finishes"
            );
            blocking(move || Controller::load()?.set_session_workspace(&session_id, &workspace_id))
                .await?;
            refresh_runtime_controller(state).await;
            Ok(DaemonReply::Done)
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
                    &crate::targets::ProcessExecutor,
                )
            })
            .await?;
            refresh_runtime_controller(state).await;
            Ok(DaemonReply::Done)
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
        DaemonAction::CheckpointSession { session_id } => Ok(DaemonReply::Checkpoint(
            state.checkpoint_session_now(&session_id).await?,
        )),
        DaemonAction::ProjectCatalog { refresh, retry } => Ok(DaemonReply::ProjectCatalog(
            state.project_catalog(refresh, retry).await?,
        )),
        DaemonAction::CreateProject { sources } => {
            let created = state.create_bundle_from_sources(sources).await?;
            let id = created.bundle_id;
            let bundle = created
                .config
                .bundles
                .get(&id)
                .context("created project is missing")?
                .clone();
            let project = blocking(move || {
                crate::project_catalog::snapshot(
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
                crate::database::load_materialized_projection_tail(&session_id, items)
            })
            .await?;
            Ok(DaemonReply::StoredProjectionTail(
                tail.map(|(session, _)| Box::new(session)),
            ))
        }
        DaemonAction::StoredSessionSummary { session_id } => {
            let summary =
                blocking(move || crate::database::load_materialized_session_summary(&session_id))
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
                !crate::controller::move_session::move_owns_session(&session_id),
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
                blocking(move || crate::sessionwiki::archive_space_preview(older_than_days))
                    .await?,
            ))
        }
        DaemonAction::SaveSetup { original, updated } => {
            let saved = blocking(move || {
                let sessions = crate::database::load_state()?;
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
        DaemonAction::SessionRecord { session_id } => Ok(DaemonReply::SessionRecord(
            state.session_record(&session_id).map(Box::new),
        )),
        DaemonAction::GoStartupSession {
            workspace_id,
            last_session_id,
        } => Ok(DaemonReply::GoStartupSession(
            state
                .go_startup_session(&workspace_id, last_session_id.as_deref())
                .map(Box::new),
        )),
        DaemonAction::RefreshQuota => {
            state.request_quota_refresh()?;
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
                    crate::database::record_user_prompt(&history_id, &history_command, &text)
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
                    crate::database::clear_session_draft_input_if_matches(
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
                    crate::database::startup_prompt_outcomes(&session_id, &command_ids)
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
            let page =
                blocking(move || crate::database::native_agent_history(&owner, &child, before))
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
        DaemonAction::StartSessionLifecycle(request) => Ok(DaemonReply::LifecycleAccepted(
            state.start_session_lifecycle(request).await?,
        )),
        DaemonAction::WaitSessionLifecycle(receipt) => state.wait_session_lifecycle(receipt).await,
        DaemonAction::SuspendSession {
            session_id,
            acknowledge_unpublished_work,
        } => {
            state
                .suspend_session_with_ack(session_id, acknowledge_unpublished_work)
                .await?;
            Ok(DaemonReply::Done)
        }
        DaemonAction::IgnoreNestedRepositoriesAndSuspend { session_id } => {
            state
                .ignore_nested_repositories_and_suspend(session_id)
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
        DaemonAction::MoveSources {
            session_id,
            cleanup_operation_id,
        } => {
            let sources = blocking(move || {
                let _guard =
                    crate::controller::move_session::MoveMutationGuard::reserve(&session_id)?;
                if let Some(operation_id) = cleanup_operation_id {
                    Controller::load()?.cleanup_retained_move_source(
                        &session_id,
                        &operation_id,
                        &ProcessExecutor,
                    )?;
                }
                crate::database::retained_move_sources(&session_id)
            })
            .await?;
            Ok(DaemonReply::MoveSources(sources))
        }
        DaemonAction::DiscardSinceCheckpoint {
            session_id,
            checkpoint,
        } => {
            state
                .discard_since_checkpoint(session_id, checkpoint)
                .await?;
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
        DaemonAction::CancelContainerUpgrade { session_id } => {
            state.cancel_container_upgrade(&session_id).await?;
            Ok(DaemonReply::Done)
        }
        DaemonAction::CancelLifecycle { session_id } => {
            blocking({
                let session_id = session_id.clone();
                move || crate::database::request_move_cancellation(&session_id)
            })
            .await?;
            state.cancel_lifecycle_with_intent(&session_id).await?;
            Ok(DaemonReply::Done)
        }
        DaemonAction::RecoverDraft { draft_id } => {
            blocking(move || crate::database::recover_detached_draft(&draft_id)).await?;
            Ok(DaemonReply::Done)
        }
        DaemonAction::Stop => {
            // A destroy in flight was acknowledged as accepted and nothing
            // about it is durable, so the stop waits for it, up to a bound
            // (#1191). The reply does not wait.
            if crate::upgrade::gate().destroys_hold_the_stop() {
                let cancellation = cancellation.clone();
                tokio::spawn(async move {
                    while crate::upgrade::gate().destroys_hold_the_stop() {
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
) -> Result<Option<crate::upgrade::Work>> {
    if let DaemonAction::PreflightNewSession { request } = action
        && !request.remote_repairs.is_empty()
    {
        return crate::upgrade::activity("new-session preflight remote repairs").map(Some);
    }
    if matches!(action, DaemonAction::SaveSpinnerStyle { .. }) {
        return crate::upgrade::activity("spinner style setting").map(Some);
    }
    if matches!(action, DaemonAction::SaveSidebarSize { .. }) {
        return crate::upgrade::activity("sidebar size setting").map(Some);
    }
    if matches!(action, DaemonAction::RemoveProject { .. }) {
        return crate::upgrade::activity("project removal").map(Some);
    }
    if matches!(action, DaemonAction::SetOperatorAway { .. }) {
        return crate::upgrade::activity("operator away setting").map(Some);
    }
    if action.is_cancellable_machine_action() {
        return Ok(None);
    }
    if matches!(
        action,
        DaemonAction::RecordOperatorActivity
            | DaemonAction::Ping
            | DaemonAction::Identity
            | DaemonAction::Configuration
            | DaemonAction::WaitSessionLifecycle(_)
            | DaemonAction::MoveSession(_)
            | DaemonAction::Status
            | DaemonAction::PrepareUpgrade
            | DaemonAction::UpgradeBlockers
            | DaemonAction::Stop
            | DaemonAction::RuntimeChanges { .. }
            | DaemonAction::SessionTail { .. }
            | DaemonAction::SubagentOptions { .. }
            | DaemonAction::WarmProfileCapabilities { .. }
            | DaemonAction::ProjectCatalog { .. }
    ) {
        Ok(None)
    } else {
        crate::upgrade::activity("client request").map(Some)
    }
}

/// Repository preflights fetch over the network. The 30 seconds the terminal
/// always allowed them also bound how long a handoff waits for one.
fn repository_preflight_executor() -> CancellableProcessExecutor {
    CancellableProcessExecutor::new(Arc::new(AtomicBool::new(false)))
        .with_deadline(Duration::from_secs(30))
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

#[cfg(all(test, unix))]
mod missing_target_compat_tests {
    use super::*;

    // Hard-won: #1444: an old client could mark a newly resumed session missing from a stale poll.
    #[tokio::test]
    async fn stale_client_missing_target_report_cannot_invalidate_a_resumed_session() {
        const CHILD: &str = "MJ_TEST_STALE_MISSING_TARGET_ACTION_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let directory = tempfile::tempdir().unwrap();
            crate::controller::test_support::IsolatedTest::new(
                crate::controller::test_support::test_name(
                    module_path!(),
                    "stale_client_missing_target_report_cannot_invalidate_a_resumed_session",
                ),
            )
            .env(CHILD, "1")
            .isolated_store(directory.path())
            .run();
            return;
        }
        let _writer = crate::database::install_isolated_test_writer();
        let workspace = crate::database::create_workspace("Resumed session").unwrap();
        let mut session = super::super::tests::runtime_test_session(
            "resumed",
            &workspace.id,
            SessionState::Running,
        );
        session.updated_at = "2026-11-02T18:00:00Z".into();
        crate::database::save_session(&session).unwrap();

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
        let stored = crate::database::load_session_record(&session.id)
            .unwrap()
            .unwrap();
        assert_eq!(stored.state, SessionState::Running);
        assert_eq!(stored.updated_at, session.updated_at);
        assert_eq!(stored.last_error, None);
    }
}
