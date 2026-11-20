use super::*;

/// The live, per-session projections the phone snapshot layers on top of the
/// controller's durable state. They arrive from relay snapshots rather than
/// from disk, so they travel together instead of as separate arguments.
pub(super) struct PhoneSessionViews<'a> {
    pub(super) native_agents:
        &'a std::collections::BTreeMap<String, Vec<mj_core::native_agent::NativeAgent>>,
    pub(super) conversations:
        &'a mj_core::snapshot_map::SnapshotMap<String, crate::server::BrowserTranscript>,
    pub(super) sessions:
        &'a mj_core::snapshot_map::SnapshotMap<String, mj_client::daemon::RuntimeSessionView>,
    pub(super) project_sources: &'a PhoneProjectSources,
    /// Lifecycle operations running now, keyed by session.
    pub(super) operations: &'a std::collections::BTreeMap<String, crate::server::ViewerOperation>,
    /// Durable Move records, projected without diagnostics or checkpoint paths.
    pub(super) move_recoveries: &'a std::collections::BTreeMap<String, ViewerMoveRecovery>,
    /// The most recent capacity reading per probe target.
    pub(super) capacity: &'a [crate::server::ViewerTargetCapacity],
    pub(super) launch_failures: &'a [crate::server::ViewerLaunchFailure],
    /// Reviews the daemon is running, keyed by session. The phone renders the
    /// same review the terminal does, from the same host.
    pub(super) reviews:
        &'a std::collections::BTreeMap<String, crate::review_host::RuntimeReviewView>,
}

/// Project the capacity service's readings and refresh status for the phone.
pub(super) fn viewer_capacity(
    metadata: &mj_client::runtime_feed::RuntimeMetadata,
) -> Vec<crate::server::ViewerTargetCapacity> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    metadata
        .host_capacities
        .iter()
        .map(|capacity| {
            let reading = &capacity.reading;
            let hosts = reading
                .filesystems
                .iter()
                .map(|filesystem| filesystem.host.as_str())
                .collect::<std::collections::BTreeSet<_>>();
            let storage = metadata
                .storage
                .iter()
                .filter(|view| hosts.contains(view.host.as_str()))
                .collect::<Vec<_>>();
            crate::server::ViewerTargetCapacity {
                storage: crate::server::ViewerTargetStorage::list(
                    &storage,
                    matches!(
                        reading.kind,
                        mj_controller::targets::DeploymentCapacityKind::AwsFleet
                    ),
                ),
                id: reading.id.clone(),
                label: reading.host.clone(),
                target_ids: reading.target_ids.clone(),
                cpu_percent: reading.cpu_percent,
                memory_used_bytes: reading.has_reading.then_some(reading.memory_used_bytes),
                memory_total_bytes: reading.has_reading.then_some(reading.memory_total_bytes),
                logical_cores: reading.has_reading.then_some(reading.logical_cores),
                container_capacity: metadata
                    .container_capacities
                    .get(&reading.id)
                    .map(|host| host.capacity),
                disk_total_bytes: capacity.disk_total_bytes,
                virtual_machines: reading.fleet_instances.map(|count| count as u64),
                sampled_at_epoch_seconds: reading.sampled_at_epoch_seconds,
                refreshing: capacity.refreshing
                    || (reading.sampled_at_epoch_seconds.is_none()
                        && reading.probe_error.is_none()),
                stale: reading.sampled_at_epoch_seconds.is_some_and(|sampled| {
                    mj_core::targets::capacity_sample_is_stale(sampled, now)
                }),
                has_error: reading.probe_error.is_some(),
            }
        })
        .collect()
}

/// Turn one lifecycle operation into the projection a phone follows.
pub(super) fn viewer_operation(
    view: &crate::daemon::RuntimeLifecycleView,
) -> crate::server::ViewerOperation {
    use crate::server::{ViewerOperationKind, ViewerOperationStage};

    crate::server::ViewerOperation {
        // RuntimeLifecycle owns the identity. A session can have consecutive
        // operations, and a client must be able to retire a late response from
        // the previous one without mistaking it for the current operation.
        id: view.operation_id.clone(),
        session_id: view.session_id.clone(),
        kind: match view.kind {
            crate::daemon::RuntimeLifecycleKind::Create => ViewerOperationKind::Create,
            crate::daemon::RuntimeLifecycleKind::Resume => ViewerOperationKind::Resume,
            crate::daemon::RuntimeLifecycleKind::Move => ViewerOperationKind::Move,
            // Stop, destroy, and retained cleanup remain distinct so a phone
            // can describe which part of teardown owns the session.
            crate::daemon::RuntimeLifecycleKind::Suspend
            | crate::daemon::RuntimeLifecycleKind::ForceStop => ViewerOperationKind::Suspend,
            crate::daemon::RuntimeLifecycleKind::DestroyStopped
            | crate::daemon::RuntimeLifecycleKind::ForceDestroy => ViewerOperationKind::Destroy,
            crate::daemon::RuntimeLifecycleKind::StopSubagent => ViewerOperationKind::Stop,
            crate::daemon::RuntimeLifecycleKind::Cleanup => ViewerOperationKind::Cleanup,
        },
        started_at_epoch_seconds: view.started_at_epoch_seconds,
        stages: view
            .active_stages
            .iter()
            .map(|(stage, started_at)| ViewerOperationStage {
                label: stage.label(),
                started_at_epoch_seconds: *started_at,
            })
            .collect(),
        notice: view.notice.clone(),
        cancellable: view.cancellable,
    }
}

/// What the phone may do with one session.
///
/// Everything here is a fact the controller holds and the browser cannot:
/// whether the session manager is driving this session, what the agent said it
/// supports, and whether a lifecycle operation already owns it.
fn viewer_capabilities(
    session: &crate::server::ViewerSession,
    operational: Option<&mj_core::relay::RelayOperationalState>,
    operation: Option<&crate::server::ViewerOperation>,
    facts: Option<&mj_core::acp::AcpSessionFacts>,
) -> crate::server::ViewerSessionCapabilities {
    use crate::server::ViewerLifecycleCategory;

    let live = session.lifecycle == ViewerLifecycleCategory::Live;
    // A session the manager is not driving cannot be talked to, whatever its
    // durable state says.
    let attached = operational.is_some();
    let verdict = session.verdict;
    let mutation_busy =
        operation.is_some() || verdict.transition.is_some() || verdict.move_queue_held;
    let idle = operational
        .is_some_and(|state| state.execution == mj_core::relay::RelayExecutionState::Idle);
    crate::server::ViewerSessionCapabilities {
        clear_context: live
            && !mutation_busy
            && operational.is_some_and(|state| state.clear_context),
        open: session.conversation_available
            && !session.transitioning
            && session.lifecycle == ViewerLifecycleCategory::Live,
        // Settled by the caller, which holds the state that defines it.
        read_history: false,
        prompt: live && attached && !mutation_busy,
        run_shell: live && attached && !mutation_busy,
        interrupt_turn: verdict.interrupt,
        cancel_operation: operation.is_some_and(|operation| operation.cancellable),
        suspend: verdict.stop.offered(),
        destroy: !operation.is_some_and(|op| {
            matches!(
                op.kind,
                crate::server::ViewerOperationKind::Destroy
                    | crate::server::ViewerOperationKind::Stop
                    | crate::server::ViewerOperationKind::Cleanup
            )
        }),
        dismiss_notification: session.notification_dismissal.is_some(),
        rename: !session.transitioning,
        resume: verdict.resume.offered(),
        move_session: verdict.move_session.offered(),
        set_config: live && attached && facts.is_some() && !mutation_busy,
        // Plan mode is a turn boundary: the terminal offers it only while the
        // agent is idle, and the phone must not be looser.
        set_plan_mode: live
            && !mutation_busy
            && idle
            && facts.is_some_and(mj_core::acp::AcpSessionFacts::supports_plan_mode),
        change_workspace: verdict.change_workspace.offered(),
        container_settings: verdict.container_settings.offered(),
        restart: verdict.restart.offered(),
        interrupt_all: verdict.interrupt_all,
    }
}

/// The Mjolnir commands a phone may offer for one session.
///
/// The list is built here, from what this session can actually do, and
/// published: the browser used to keep its own copy, which is how `/review`
/// was missing from the phone while the terminal had it.
pub(super) fn phone_commands(
    session: &crate::server::ViewerSession,
    operational: Option<&mj_core::relay::RelayOperationalState>,
) -> std::sync::Arc<Vec<crate::server::ViewerMjCommand>> {
    use crate::server::ViewerCommandSource;
    use agent_client_protocol::v1::schema::AvailableCommandInput;

    static BASE_COMMANDS: std::sync::OnceLock<std::sync::Arc<Vec<crate::server::ViewerMjCommand>>> =
        std::sync::OnceLock::new();
    let command =
        |name: &str, description: &str, argument: Option<&str>| crate::server::ViewerMjCommand {
            name: name.to_owned(),
            description: description.to_owned(),
            source: ViewerCommandSource::Mj,
            argument: argument.map(str::to_owned),
        };
    let mut commands = BASE_COMMANDS
        .get_or_init(|| {
            std::sync::Arc::new(vec![
                command("help", "show available Mjolnir and agent commands", None),
                command(
                    "detach",
                    "leave the conversation without stopping the worker",
                    None,
                ),
            ])
        })
        .clone();
    let option = |key: &str| {
        session
            .config_options
            .iter()
            .any(|option| option.key == key)
    };
    if session.capabilities.set_config && !session.config_options.is_empty() {
        std::sync::Arc::make_mut(&mut commands).push(command(
            "config",
            "change any setting advertised by the active agent",
            Some("key [value]"),
        ));
    }
    if option("model") {
        std::sync::Arc::make_mut(&mut commands).push(command(
            "model",
            "change the active model",
            Some("value"),
        ));
    }
    // Same condition as the dashboard (`AcpSurface::supports_fast_mode`):
    // the harness advertises `fast-mode` with both on and off.
    if session.config_options.iter().any(|option| {
        option.key == "fast-mode"
            && ["on", "off"]
                .iter()
                .all(|value| option.choices.iter().any(|choice| choice.value == *value))
    }) {
        std::sync::Arc::make_mut(&mut commands).push(command(
            "fast",
            "toggle Codex Fast mode",
            None,
        ));
    }
    if option("effort") {
        std::sync::Arc::make_mut(&mut commands).push(command(
            "effort",
            "change the active reasoning effort",
            Some("value"),
        ));
    }
    if session.plan_mode_active.is_some() && session.capabilities.set_plan_mode {
        std::sync::Arc::make_mut(&mut commands).push(command(
            "plan",
            "toggle plan mode",
            Some("message"),
        ));
        std::sync::Arc::make_mut(&mut commands).push(command(
            "implement",
            "leave plan mode and implement",
            Some("instruction"),
        ));
    }
    if session.capabilities.prompt || session.turn_review.is_some() {
        std::sync::Arc::make_mut(&mut commands).push(command(
            "review",
            "review the finished turn now, or report how review is configured",
            Some("status"),
        ));
    }
    // These names are handled by Mjolnir even when the corresponding control
    // is unavailable for this session. An agent cannot claim one and turn a
    // locally interpreted slash command into a misleading palette entry.
    if session.capabilities.clear_context {
        std::sync::Arc::make_mut(&mut commands).push(command(
            "clear",
            "start a new conversation and clear pending mail, keeping workspace and history",
            None,
        ));
    }
    let reserved = [
        "clear",
        "help",
        "detach",
        "config",
        "model",
        "fast",
        "effort",
        "plan",
        "implement",
        "review",
    ];
    for advertised in operational
        .into_iter()
        .flat_map(|state| state.available_commands.iter())
    {
        let name = advertised.name.trim();
        if name.is_empty()
            || reserved
                .iter()
                .any(|local| name.eq_ignore_ascii_case(local))
            || commands
                .iter()
                .any(|existing| existing.name.eq_ignore_ascii_case(name))
        {
            continue;
        }
        let argument = advertised.input.as_ref().and_then(|input| match input {
            AvailableCommandInput::Unstructured(input) => {
                let hint = input.hint.trim();
                (!hint.is_empty()).then(|| hint.to_owned())
            }
            _ => None,
        });
        std::sync::Arc::make_mut(&mut commands).push(crate::server::ViewerMjCommand {
            name: name.to_owned(),
            description: advertised.description.trim().to_owned(),
            source: ViewerCommandSource::Agent,
            argument,
        });
    }
    commands
}

/// The open reviews, keyed by session, for one snapshot.
pub(super) type ViewerMoveRecoveries =
    std::collections::BTreeMap<String, crate::server::ViewerMoveRecovery>;

#[cfg(test)]
pub(super) fn viewer_snapshot(
    controller: &Controller,
    workspaces: &[mj_core::workspace::WorkspaceRecord],
    quotas: &std::collections::BTreeMap<String, ProfileQuota>,
    views: &PhoneSessionViews<'_>,
    revision: u64,
) -> ViewerSnapshot {
    let mut children = crate::server::ViewerChildren::default();
    for relation in controller.state.subagents.values() {
        children
            .entry(relation.parent_session_id.clone())
            .or_insert_with(Default::default)
            .insert(relation.child_session_id.clone(), relation.kind);
    }
    let mut capture = mj_client::runtime_feed::RuntimeProjection {
        records: controller.state.sessions.clone(),
        subagents: controller.state.subagents.clone(),
        sessions: views.sessions.clone(),
        metadata: mj_client::runtime_feed::RuntimeMetadata {
            config: controller.config.clone().into(),
            workspaces: workspaces.to_vec(),
            ..Default::default()
        },
        ..Default::default()
    };
    use crate::daemon::{RuntimeLifecycleKind, RuntimeLifecycleView};
    use crate::server::ViewerOperationKind;
    capture.metadata.lifecycles = views
        .operations
        .iter()
        .filter_map(|(id, op)| {
            let kind = match op.kind {
                ViewerOperationKind::Create => RuntimeLifecycleKind::Create,
                ViewerOperationKind::Resume => RuntimeLifecycleKind::Resume,
                ViewerOperationKind::Suspend => RuntimeLifecycleKind::Suspend,
                ViewerOperationKind::Destroy => RuntimeLifecycleKind::ForceDestroy,
                ViewerOperationKind::Stop => RuntimeLifecycleKind::StopSubagent,
                ViewerOperationKind::Cleanup => RuntimeLifecycleKind::Cleanup,
                ViewerOperationKind::Move => RuntimeLifecycleKind::Move,
                ViewerOperationKind::Checkpoint => return None,
            };
            Some(RuntimeLifecycleView {
                operation_id: format!("fixture-{id}"),
                move_operation_id: None,
                session_id: id.clone(),
                kind,
                cancellable: op.cancellable,
                active_stages: Vec::new(),
                started_at_epoch_seconds: op.started_at_epoch_seconds,
                resume_destination: None,
                notice: None,
            })
        })
        .collect();
    capture.native_agents = views
        .native_agents
        .values()
        .flatten()
        .map(|agent| {
            let view = mj_core::native_agent::NativeAgentSummary {
                agent: agent.clone(),
                generation_ordinal: 0,
                projection_ordinal: 0,
                projection_digest: String::new(),
                pending_input: false,
                pending_input_request_id: None,
                pending_input_message: None,
                pending_input_raised_at_ms: None,
            };
            (agent.view_id(), view)
        })
        .collect();
    capture.metadata.failed_launch_retries = capture
        .records
        .iter()
        .filter_map(|(id, record)| {
            mj_controller::database::failed_first_launch(
                record,
                capture.subagents.contains_key(id),
                None,
            )
            .then_some(id.clone())
        })
        .collect();
    crate::daemon::verdicts::project(&mut capture, &Default::default());
    viewer_snapshot_selected(
        controller,
        workspaces,
        quotas,
        views,
        revision,
        ViewerRecordSelection {
            ids: &controller.state.sessions.keys().cloned().collect(),
            children: &children,
            failed_launch_retries: None,
        },
        &capture,
    )
}

#[derive(Clone, Copy)]
pub(super) struct ViewerRecordSelection<'a> {
    pub(super) ids: &'a std::collections::BTreeSet<String>,
    pub(super) children: &'a crate::server::ViewerChildren,
    pub(super) failed_launch_retries: Option<&'a std::collections::BTreeSet<String>>,
}

pub(super) fn viewer_snapshot_selected(
    controller: &Controller,
    workspaces: &[mj_core::workspace::WorkspaceRecord],
    quotas: &std::collections::BTreeMap<String, ProfileQuota>,
    views: &PhoneSessionViews<'_>,
    revision: u64,
    selection: ViewerRecordSelection<'_>,
    capture: &mj_client::runtime_feed::RuntimeProjection,
) -> ViewerSnapshot {
    let metadata = &capture.metadata;
    let verdicts = &capture.verdicts;
    let PhoneSessionViews {
        native_agents,
        conversations,
        reviews,
        sessions,
        project_sources,
        operations,
        move_recoveries,
        capacity,
        launch_failures,
    } = views;
    let mut snapshot = ViewerSnapshot::from_config_records(
        &controller.config,
        &controller.state,
        revision,
        selection
            .ids
            .iter()
            .filter_map(|id| controller.state.sessions.get(id)),
        selection.children,
        selection.failed_launch_retries,
    );
    let availability = &metadata.availability;
    snapshot.target_availability_notes = availability.browser.target_notes.clone();
    for profile in &mut snapshot.profiles {
        profile.availability = browser_availability(
            availability.profiles.get(&profile.id),
            availability.browser.profiles.get(&profile.id),
        );
    }
    for target in &mut snapshot.targets {
        target.availability = browser_availability(
            availability.targets.get(&target.id),
            availability.browser.targets.get(&target.id),
        );
        target.runtime_missing = availability.uninstalled_engines.contains(&target.id);
        let Some(template) = controller.config.targets.get(&target.id) else {
            continue;
        };
        target.resource_allocation_kind = template.into();
        if mj_core::config::is_container_target(template) {
            let host_capacity = capacity
                .iter()
                .find(|host| host.target_ids.iter().any(|id| id == &target.id))
                .and_then(|host| host.container_capacity);
            let limits = host_capacity.map(|host| mj_core::state::HostContainerSize {
                cpus: host.total.cpus,
                memory_bytes: host.total.memory_bytes,
            });
            target.remembered_container_size = mj_core::config::container_size_host(template)
                .and_then(|host| controller.state.container_sizes.get(host))
                .copied();
            target.container_host_limits = limits;
            target.container_capacity = host_capacity;
            let size =
                mj_core::state::default_container_size(target.remembered_container_size, limits);
            target.default_resource_allocation =
                Some(mj_core::state::SessionResourceAllocation::Container {
                    cpus: size.cpus,
                    memory_bytes: size.memory_bytes,
                });
        } else {
            target.remembered_container_size = None;
            target.container_host_limits = None;
            target.container_capacity = None;
            target.default_resource_allocation = None;
        }
    }
    snapshot.launch_failures = launch_failures.to_vec();
    snapshot.workspaces = workspaces
        .iter()
        .map(|workspace| crate::server::ViewerWorkspace {
            id: workspace.id.clone(),
            name: workspace.name.clone(),
            last_opened_at: workspace.last_opened_at.clone(),
        })
        .collect();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    for profile in &mut snapshot.profiles {
        let Some(quota) = quotas.get(&profile.id) else {
            continue;
        };
        let mut quota = mj_controller::quota::surface_quota_report(quota);
        // The viewer reads stored reports, which keep every pool; show the
        // pools the profile uses, as the published quota does.
        if let Ok(config) = controller.config.named_profile(&profile.id) {
            quota.retain_antigravity_pools(&config.antigravity_pools());
        }
        // Only plans that keep Argon apart report its pool. A reading lists
        // the pools the account has; a failed one shows every pool as failed.
        if quota.harness == mj_core::config::HarnessKind::Antigravity && !quota.windows.is_empty() {
            profile.quota_pools.retain(|label| {
                quota.windows.iter().any(|window| {
                    mj_client::quota::antigravity_pool_of_window(window)
                        .is_some_and(|pool| pool.label() == label)
                })
            });
        }
        let master_weekly = quota.weekly_window();
        let fable_remaining = master_weekly.and_then(|master| {
            master.remaining_percent?;
            quota
                .fable_weekly_window()
                .and_then(|window| window.remaining_percent)
        });
        profile.quota = Some(ViewerQuota {
            summary: quota.compact(),
            windows: quota
                .windows
                .iter()
                .filter(|window| {
                    window.model_scope.is_none()
                        || quota.harness == mj_core::config::HarnessKind::Antigravity
                })
                .map(|window| crate::server::ViewerQuotaWindow {
                    label: window.label.clone(),
                    pool: (quota.harness == mj_core::config::HarnessKind::Antigravity)
                        .then(|| mj_client::quota::antigravity_pool_of_window(window))
                        .flatten()
                        .map(|pool| pool.label().to_owned()),
                    resets_at_epoch_seconds: window.resets_at_epoch_seconds,
                    reset_countdown_style: window.reset_countdown_style(),
                    banked_resets: quota.banked_resets_for_window(window),
                    // The controller reports headroom; a bar fills as a limit
                    // is consumed, so the phone is given the complement.
                    percent_used: window
                        .remaining_percent
                        .map(|left| 100_u8.saturating_sub(left)),
                    fable_remaining_percent: master_weekly
                        .filter(|weekly| std::ptr::eq(*weekly, window))
                        .and(fable_remaining),
                    resets_at: window.resets.clone(),
                    projects_exhaustion_before_reset: mj_controller::quota::projects_exhaustion(
                        window,
                        quota.refreshed_at_epoch_seconds,
                    ),
                })
                .collect(),
            resets_at: quota
                .windows
                .iter()
                .find_map(|window| window.resets.clone()),
            stale: now.saturating_sub(quota.refreshed_at_epoch_seconds)
                > QUOTA_STALE_AFTER.as_secs(),
            refreshed_at_epoch_seconds: quota.refreshed_at_epoch_seconds,
            rate_limited_until_epoch_seconds: quota.rate_limited_until_epoch_seconds,
            next_check_at_epoch_seconds: quota.next_check_at(),
            has_error: quota.error.is_some(),
        });
    }
    let session_ids = snapshot.sessions.0.keys().cloned().collect::<Vec<_>>();
    for session_id in session_ids {
        let session = snapshot
            .sessions
            .0
            .get_mut(&session_id)
            .expect("selected session");
        session.move_recovery = move_recoveries.get(&session.id).cloned().map(|recovery| {
            Box::new(recovery.with_public_failure(session.launch_error.as_deref()))
        });
        if let Some(record) = controller.state.sessions.get(&session.id)
            && let Some(source) = project_sources.source(record, controller)
        {
            session.set_project_source(source);
        }
        let view = sessions.get(&session.id);
        let live = view.and_then(|view| view.operational.as_deref());
        let materialized = view.and_then(|view| view.materialized.as_deref());
        session.last_activity_at_ms = materialized.and_then(|view| view.last_activity_at_ms);
        session.queued_prompts = materialized
            .into_iter()
            .flat_map(|view| &view.queued_prompts)
            .map(|prompt| ViewerQueuedPrompt {
                id: prompt.id.clone(),
                kind: prompt.kind.clone(),
                text: prompt.text.clone(),
                created_at: prompt.created_at_ms.to_string(),
            })
            .collect();
        session.active_user_shells = live
            .into_iter()
            .flat_map(|state| &state.active_user_shells)
            .map(|shell| ViewerUserShell {
                id: shell.command_id.clone(),
                command: shell.command.clone(),
                started_at_ms: shell.started_at_ms,
            })
            .collect();
        session.background_tasks = live
            .into_iter()
            .flat_map(|state| &state.background_commands)
            .map(|task| ViewerBackgroundTask {
                id: task.id.clone(),
                command: task.command.clone(),
                started_at_ms: task.started_at_ms,
                can_stop: task.can_stop,
            })
            .collect();
        session.pending_elicitations = materialized
            .map(|view| view.pending_elicitations.clone())
            .unwrap_or_default();
        // A reviewing harness's form is this session's question too: the
        // person answers it here, and its id routes the answer to that role.
        if let Some(review) = reviews.get(&session.id) {
            session.pending_elicitations.extend(
                review
                    .questions
                    .iter()
                    .map(crate::review_host::ReviewerQuestion::session_request),
            );
        }
        session.prompt_images_supported =
            live.is_some_and(mj_core::relay::RelayOperationalState::accepts_prompt_images);
        session.operation = operations.get(&session.id).cloned().map(Box::new);
        session.verdict = verdicts.get(&session.id).copied().unwrap_or_default();
        session.transitioning = session.verdict.transition.is_some();
        if session.state == mj_core::state::SessionState::Disconnected.as_str()
            && session.operation.as_ref().is_some_and(|operation| {
                matches!(
                    operation.kind,
                    crate::server::ViewerOperationKind::Create
                        | crate::server::ViewerOperationKind::Resume
                )
            })
        {
            session.state = crate::server::LAUNCHING_STATE.to_owned();
        }
        session.storage_problem = controller
            .state
            .sessions
            .get(&session.id)
            .and_then(|record| {
                mj_controller::target_storage::session_problem_in(&metadata.storage, record)
            });
        session.native_subagents = native_agents.get(&session.id).cloned().unwrap_or_default();
        session.targeted_turn_control_supported =
            live.is_some_and(|state| state.supports_targeted_turn_control());
        session.turn_controls = live.and_then(|s| s.turn_controls.clone()).map(Box::new);
        session.steering = live.and_then(|s| s.steering.clone()).map(Box::new);
        session.active_prompt_id =
            live.and_then(|s| s.active_prompt.as_ref().map(|p| p.command_id.clone()));
        session.cancelling_prompt_id = live.and_then(|s| s.cancelling_prompt_id.clone());
        // One answer for every session, whether or not the daemon can see its
        // worker. A worker the daemon has lost is reported as what was last
        // known about it, never as idle: a turn that outlives a daemon
        // restart is still running, and calling it idle made automation treat
        // it as finished (#1025).
        let activity_state = match live {
            _ if session.verdict.idle && session.state == "parked" => {
                mj_core::activity::ActivityState::Idle {
                    since_ms: session.last_activity_at_ms,
                }
            }
            Some(state) => state.activity_state(),
            None => mj_core::activity::while_disconnected(
                materialized.map_or(Default::default(), |view| view.execution),
                session.last_activity_at_ms,
            ),
        };
        // A session the daemon can see reports the phase its own flag names,
        // corrected for a turn that flag has not caught up with; one it cannot
        // see reports what was last known.
        let chat_phase = match live {
            _ if session.verdict.idle && session.state == "parked" => {
                mj_core::relay::RelayExecutionState::Idle
            }
            Some(state) => mj_core::activity::chat_phase(&state.facts()),
            None => activity_state.chat_phase(),
        };
        session.chat_phase = match chat_phase {
            mj_core::relay::RelayExecutionState::Idle => crate::server::ViewerChatPhase::Idle,
            mj_core::relay::RelayExecutionState::Running => crate::server::ViewerChatPhase::Running,
            mj_core::relay::RelayExecutionState::Closing => crate::server::ViewerChatPhase::Closing,
            mj_core::relay::RelayExecutionState::Closed => crate::server::ViewerChatPhase::Closed,
        };
        session.is_idle = session.verdict.idle;
        session.attention = metadata
            .session_attention
            .get(&session.id)
            .copied()
            .unwrap_or_default();
        session.unread_frontier = metadata.unread_frontiers.get(&session.id).copied();
        session.notification_dismissal = metadata.notification_dismissals.get(&session.id).cloned();
        session.notification_card = metadata
            .notification_cards
            .as_ref()
            .and_then(|cards| cards.get(&session.id))
            .cloned();
        session.question_message = match session
            .notification_card
            .as_ref()
            .and_then(mj_client::runtime_feed::NotificationCard::shown_question)
        {
            Some(entry) => match &entry.message {
                mj_core::attention::EntryMessage::Found(id) => Some(id.clone()),
                mj_core::attention::EntryMessage::Locating
                | mj_core::attention::EntryMessage::None => None,
            },
            None => question_message(live, session.attention),
        };
        session.form_holder = metadata.form_holders.get(&session.id).cloned();
        session.native_subagent_attention = session
            .native_subagents
            .iter()
            .map(|native| {
                (
                    native.session_id.clone(),
                    metadata
                        .session_attention
                        .get(&native.view_id())
                        .copied()
                        .unwrap_or_default(),
                )
            })
            .collect();
        session.supports_open_request = live.is_some_and(|state| state.supports_open_request());
        session.activity_state = Some(activity_state);
        let facts = live.map(|state| {
            mj_core::acp::AcpSessionFacts::from_operational(
                controller
                    .state
                    .sessions
                    .get(&session.id)
                    .map_or(mj_core::config::HarnessKind::Codex, |record| {
                        record.harness_kind
                    }),
                &state.config,
                &state.config_options,
                state.modes.as_ref(),
            )
        });
        if let Some(state) = live {
            session.latest_event_ordinal = state.latest_ordinal;
            session.config_options = crate::server::viewer_config_options(
                &state.config_options,
                facts
                    .as_ref()
                    .expect("live operational state always has ACP session facts"),
            );
            session.policy_config_keys = crate::server::filter_policy_config_options(
                &mut session.config_options,
                session.execution_enforcement,
                &state.config_options,
            );
            // Share activity classification with the terminal. The browser
            // retains its detailed turn/step/background clock presentation.
            let turn_started_at_ms = state
                .active_prompt
                .as_ref()
                .map(|prompt| prompt.started_at_ms)
                .or_else(|| state.harness_turn.map(|turn| turn.started_at_ms));
            let turn_started_at = turn_started_at_ms
                .and_then(|started_at_ms| u64::try_from(started_at_ms).ok())
                .map(|started_at_ms| started_at_ms / 1_000);
            session.capacity_retry = state.capacity_retry.clone().map(Box::new);
            session.quota_recovery = state
                .continuation
                .quota_recovery
                .clone()
                .filter(|r| !r.submitted)
                .map(Box::new);
            let activity = mj_client::usage_format::SessionActivity::of(state);
            session.retry_assessment_pending = activity.checking_response;
            let activity_details =
                activity.details(turn_started_at_ms, state.current_step_started_at_ms);
            session.activity_details = Some(viewer_activity_details(&activity_details));
            session.activity = mj_client::usage_format::format_activity_columns(
                now,
                turn_started_at,
                state
                    .current_step_started_at_ms
                    .and_then(|value| u64::try_from(value).ok()),
                &activity,
            )
            .join("  ")
            .trim()
            .to_owned();
        }
        session.plan_mode_active = facts
            .as_ref()
            .filter(|facts| facts.supports_plan_mode())
            .map(mj_core::acp::AcpSessionFacts::plan_mode_active);
        session.turn_review = reviews
            .get(&session.id)
            .map(crate::server::ViewerTurnReview::from_runtime)
            .map(Box::new);
        session.capabilities =
            viewer_capabilities(session, live, operations.get(&session.id), facts.as_ref());
        // The state owns what "stopped sub-agent" means, so the terminal and
        // the viewer agree on which children can be read.
        session.capabilities.read_history =
            controller.state.is_stopped_subagent(&session.id) && !session.transitioning;
        session.available_commands = phone_commands(session, live);
        if let Some(transcript) = conversations.get(&session.id) {
            session.conversation_available = true;
            // The card's last-message clock moves only on top-level
            // conversation entries, never on tool calls or status lines. It
            // is computed even while transitioning, unlike the preview below.
            session.last_message_at_ms = transcript
                .entries
                .iter()
                .filter(|entry| matches!(entry.role, "user" | "agent"))
                .filter_map(|entry| entry.recorded_at_ms)
                .max();
            if !session.transitioning {
                let mut lines = transcript
                    .entries
                    .iter()
                    .flat_map(|entry| {
                        entry
                            .lines
                            .iter()
                            .enumerate()
                            .filter_map(move |(index, line)| {
                                let line = line.trim();
                                (!line.is_empty()).then(|| {
                                    if index == 0 {
                                        format!("{}: {line}", entry.label)
                                    } else {
                                        line.to_owned()
                                    }
                                })
                            })
                    })
                    .collect::<Vec<_>>();
                session.preview = lines.split_off(lines.len().saturating_sub(4));
            }
        }
        // `conversation_available` is only known after the transcript loop
        // above, so the capability that depends on it is settled here.
        session.capabilities.open = session.conversation_available && !session.transitioning;
    }
    snapshot.capacity = capacity.to_vec();
    snapshot
}

fn browser_availability(
    verdict: Option<&mj_client::availability::AvailabilityVerdict>,
    public_reason: Option<&String>,
) -> mj_client::availability::AvailabilityVerdict {
    use mj_client::availability::AvailabilityVerdict;
    match verdict {
        Some(AvailabilityVerdict::Available) => AvailabilityVerdict::Available,
        Some(AvailabilityVerdict::Missing { .. }) => AvailabilityVerdict::Missing {
            reason: public_reason
                .cloned()
                .unwrap_or_else(|| "Failed its availability check.".to_owned()),
        },
        Some(AvailabilityVerdict::Unknown) | None => AvailabilityVerdict::Unknown,
    }
}

fn question_message(
    live: Option<&mj_core::relay::RelayOperationalState>,
    attention: mj_core::attention::SessionAttention,
) -> Option<String> {
    if attention != mj_core::attention::SessionAttention::NeedsInput {
        return None;
    }
    live?.question_message().map(str::to_owned)
}

pub(super) fn viewer_activity_details(
    details: &mj_client::usage_format::SessionActivityDetails,
) -> ViewerActivityDetails {
    ViewerActivityDetails {
        kind: match details.kind {
            mj_client::usage_format::SessionActivityKind::Turn => ViewerActivityKind::Turn,
            mj_client::usage_format::SessionActivityKind::Step => ViewerActivityKind::Step,
            mj_client::usage_format::SessionActivityKind::Background => {
                ViewerActivityKind::Background
            }
            mj_client::usage_format::SessionActivityKind::Idle => ViewerActivityKind::Idle,
            mj_client::usage_format::SessionActivityKind::Lifecycle
            | mj_client::usage_format::SessionActivityKind::Goal
            | mj_client::usage_format::SessionActivityKind::Expecting
            | mj_client::usage_format::SessionActivityKind::CheckingQuota => {
                ViewerActivityKind::Lifecycle
            }
        },
        turn_started_at_ms: details.turn_started_at_ms,
        step_started_at_ms: details.step_started_at_ms,
        background_started_at_ms: details.background_started_at_ms,
        idle_since_ms: details.idle_since_ms,
        last_activity_at_ms: details.last_activity_at_ms,
        label: details.label.clone(),
    }
}

#[cfg(test)]
mod question_message_tests {
    use super::question_message;

    #[test]
    fn legacy_unknown_request_and_current_empty_request_are_distinct() {
        use mj_core::assessment::{Action, Status, TurnAssessment};
        use mj_core::attention::SessionAttention;
        let mut live =
            mj_core::relay::RelaySnapshot::new("compatibility".into()).operational_state();
        let mut assessment = TurnAssessment::pending(
            "turn".into(),
            1,
            1,
            mj_core::activity::verdict::CompletionEvidence::bounded("EndTurn", None),
        );
        assessment.status = Status::Assessed;
        assessment.action = Some(Action::AwaitInput);
        assessment.question_message = Some("agent:legacy".into());
        live.assessment = Some(std::sync::Arc::new(assessment));
        live.relay_protocol_version = Some(mj_core::relay::RELAY_OPEN_REQUEST_PROTOCOL - 1);
        assert_eq!(
            question_message(Some(&live), SessionAttention::NeedsInput).as_deref(),
            Some("agent:legacy")
        );
        live.relay_protocol_version = Some(mj_core::relay::RELAY_OPEN_REQUEST_PROTOCOL);
        assert_eq!(
            question_message(Some(&live), SessionAttention::NeedsInput),
            None
        );
        live.open_request = Some(mj_core::assessment::OpenRequest {
            question: mj_core::assessment::InferredQuestion {
                turn_id: "older".into(),
                completed_ordinal: 0,
            },
            question_message: Some("agent:original".into()),
        });
        assert_eq!(
            question_message(Some(&live), SessionAttention::NeedsInput).as_deref(),
            Some("agent:original")
        );
        assert_eq!(
            question_message(Some(&live), SessionAttention::Working),
            None
        );
    }
}
