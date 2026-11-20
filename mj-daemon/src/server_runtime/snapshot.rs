use super::*;

/// The live, per-session projections the phone snapshot layers on top of the
/// controller's durable state. They arrive from relay snapshots rather than
/// from disk, so they travel together instead of as separate arguments.
pub(super) struct PhoneSessionViews<'a> {
    pub(super) native_agents:
        &'a std::collections::BTreeMap<String, Vec<mj_core::native_agent::NativeAgent>>,
    pub(super) conversations:
        &'a mj_core::snapshot_map::SnapshotMap<String, crate::server::BrowserTranscript>,
    pub(super) queued_prompts:
        &'a std::collections::BTreeMap<String, Vec<mj_core::relay::QueuedPrompt>>,
    pub(super) active_user_shells:
        &'a std::collections::BTreeMap<String, Vec<mj_core::relay::ActiveUserShell>>,
    pub(super) pending_elicitations:
        &'a std::collections::BTreeMap<String, Vec<mj_core::elicitation::ElicitationRequest>>,
    /// Sessions whose agent advertised image support in prompts.
    pub(super) prompt_images: &'a std::collections::BTreeSet<String>,
    /// What each managed session's relay last reported. This is where the
    /// projection learns what the agent can do, rather than guessing from the
    /// durable record, which knows only what was configured.
    pub(super) operational: &'a std::collections::BTreeMap<
        String,
        mj_core::snapshot_map::SharedSnapshot<mj_core::relay::RelayOperationalState>,
    >,
    /// Durable activity watermarks delivered with the materialized worker
    /// snapshots. Keeping this in the control-loop cache avoids a database
    /// read while rendering each viewer snapshot.
    pub(super) materialized_activity: &'a std::collections::BTreeMap<String, MaterializedActivity>,
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

/// What the phone server remembers about one probe target: the target itself
/// and the phone's own nudge of the poller. The readings, their times and
/// failures belong to the daemon's capacity service and are read from it, so
/// the page can never disagree with `mj hosts`.
#[derive(Debug, Clone)]
pub(super) struct PhoneCapacity {
    pub(super) target: mj_controller::targets::DeploymentCapacityTarget,
    pub(super) refresh: PhoneRefresh,
}

/// The phone's latest request for a new reading.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PhoneRefresh {
    Idle,
    /// Accepted with this generation; over once the target's
    /// `answered_refresh` reaches it.
    Waiting {
        generation: u64,
    },
    /// The poller was gone and refused the request.
    Rejected,
}

/// Keep the state map in step with the targets the daemon's capacity service
/// probes.
pub(super) fn track_capacity_targets(
    targets: &[mj_controller::targets::DeploymentCapacityTarget],
    state: &mut std::collections::BTreeMap<String, PhoneCapacity>,
) {
    state.retain(|id, _| targets.iter().any(|target| target.id == *id));
    for target in targets {
        state
            .entry(target.id.clone())
            .and_modify(|entry| entry.target = target.clone())
            .or_insert_with(|| PhoneCapacity {
                target: target.clone(),
                refresh: PhoneRefresh::Idle,
            });
    }
}

/// Project the capacity readings for the phone.
pub(super) fn viewer_capacity(
    state: &std::collections::BTreeMap<String, PhoneCapacity>,
    readings: &std::collections::BTreeMap<String, mj_controller::targets::TargetCapacityState>,
    container_capacities: &std::collections::BTreeMap<
        String,
        mj_client::runtime_feed::RuntimeContainerCapacity,
    >,
) -> Vec<crate::server::ViewerTargetCapacity> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let storage = mj_controller::target_storage::views_snapshot();
    state
        .values()
        .map(|entry| {
            let reading = readings.get(&entry.target.id).cloned().unwrap_or_default();
            let usage = reading.usage.as_ref();
            let rejected = entry.refresh == PhoneRefresh::Rejected;
            let waiting = matches!(
                entry.refresh,
                PhoneRefresh::Waiting { generation } if reading.answered_refresh < generation
            );
            let hosts = mj_core::targets::storage::capacity_storage_hosts(&entry.target, usage);
            let container_capacity = container_capacities
                .get(&entry.target.id)
                .map(|capacity| capacity.capacity);
            crate::server::ViewerTargetCapacity {
                storage: crate::server::ViewerTargetStorage::list(
                    &mj_core::targets::storage::views_for(&storage, &hosts),
                    matches!(
                        entry.target.kind,
                        mj_controller::targets::DeploymentCapacityKind::AwsFleet
                    ),
                ),
                id: entry.target.id.clone(),
                label: entry.target.host.clone(),
                target_ids: entry.target.target_ids.clone(),
                cpu_percent: usage.and_then(|usage| usage.cpu_percent),
                memory_used_bytes: usage.map(|usage| usage.memory_used_bytes),
                memory_total_bytes: usage.map(|usage| usage.memory_total_bytes),
                logical_cores: usage.map(|usage| usage.logical_cores),
                container_capacity,
                disk_total_bytes: usage.and_then(|usage| usage.disk_total_bytes),
                // The same count `mj hosts` reports: absent for a plain host
                // and for a fleet whose size is unknown.
                virtual_machines: reading
                    .fleet_instances(&entry.target)
                    .map(|count| count as u64),
                sampled_at_epoch_seconds: reading.sampled_at_epoch_seconds,
                // A target with no result yet is loading, not idle.
                refreshing: !rejected && (waiting || !reading.has_answered()),
                stale: reading.sampled_at_epoch_seconds.is_some_and(|sampled| {
                    mj_core::targets::capacity_sample_is_stale(sampled, now)
                }),
                has_error: rejected || reading.probe_error.is_some(),
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
pub(super) fn session_capabilities(
    session: &crate::server::ViewerSession,
    operational: Option<&mj_core::relay::RelayOperationalState>,
    operation: Option<&crate::server::ViewerOperation>,
    facts: Option<&mj_core::acp::AcpSessionFacts>,
    workspace_count: usize,
    container_target: bool,
    tree_turn_running: bool,
) -> crate::server::ViewerSessionCapabilities {
    use crate::server::ViewerLifecycleCategory;

    let live = session.lifecycle == ViewerLifecycleCategory::Live;
    // A session the manager is not driving cannot be talked to, whatever its
    // durable state says.
    let attached = operational.is_some();
    // A failed close/destroy can leave its durable record in an intermediate
    // state after the lifecycle owner has gone away. Keep the status card
    // selectable, but do not turn the failure into an endless mutation lock:
    // the published Stop capability is the recovery action.
    let transition_busy = session.transitioning && !session.has_error;
    let busy = operation.is_some() || transition_busy;
    // A failed move can retain a live destination while queue admission is
    // incomplete. The durable move hold owns all mutating session controls in
    // that interval; retry Move is the one intentional exception.
    let partial_move_queue = session.move_recovery.as_ref().is_some_and(|recovery| {
        recovery.queue_admission_started && !recovery.queue_admission_finished
    });
    let mutation_busy = busy || partial_move_queue;
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
        // A turn Claude Code started on its own after a background task can
        // be stopped too. A Codex turn of that kind is a native goal, which
        // has its own controls.
        interrupt_turn: live
            && !mutation_busy
            && operational.is_some_and(|state| {
                state.active_prompt.is_some()
                    || (state.harness_turn.is_some() && !state.goal.active())
                    || state.capacity_retry.is_some()
                    || state
                        .continuation
                        .quota_recovery
                        .as_ref()
                        .is_some_and(|r| !r.submitted)
            }),
        cancel_operation: operation.is_some_and(|operation| operation.cancellable),
        // Stopping a session that is already stopping asks for something that
        // is happening; resuming one that is running asks for a second copy.
        suspend: session.lifecycle.is_dashboard_visible() && !mutation_busy,
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
        // A Move that holds the source environment refuses Resume; it is
        // retried or the session is destroyed.
        resume: session.has_resume_source()
            && !session.lifecycle.is_dashboard_visible()
            && !mutation_busy
            && !session
                .move_recovery
                .as_ref()
                .is_some_and(|recovery| recovery.environment_retained),
        move_session: live && !busy,
        set_config: live && attached && facts.is_some() && !mutation_busy,
        // Plan mode is a turn boundary: the terminal offers it only while the
        // agent is idle, and the phone must not be looser.
        set_plan_mode: live
            && !mutation_busy
            && idle
            && facts.is_some_and(mj_core::acp::AcpSessionFacts::supports_plan_mode),
        // Moving between workspaces is a durable-record edit, but offering it
        // needs the one fact the record does not hold: that another workspace
        // exists. The terminal refuses with "there is no other workspace".
        change_workspace: workspace_count >= 2 && !mutation_busy,
        // The container gate shares the terminal's classifier: the session's
        // configured target template decides, never the worker's report.
        container_settings: container_target && !mutation_busy,
        // A live session restarts from a fresh recovery copy; a suspended one
        // comes back from the checkpoint it already has. The terminal gates
        // the same way, plus idle, which `mutation_busy` stands in for here.
        restart: (session.lifecycle.is_dashboard_visible() || session.has_checkpoint)
            && !mutation_busy,
        // True when this session or any sub-agent under it has a turn running,
        // which the execution arm re-checks against the live handles.
        interrupt_all: !mutation_busy && tree_turn_running,
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
pub(super) fn review_views(
    daemon_runtime: &Arc<RuntimeState>,
) -> std::collections::BTreeMap<String, crate::review_host::RuntimeReviewView> {
    daemon_runtime
        .review_host()
        .views()
        .into_iter()
        .map(|review| (review.session_id.clone(), review))
        .collect()
}

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
    viewer_snapshot_selected(
        controller,
        workspaces,
        quotas,
        views,
        revision,
        None,
        &Default::default(),
    )
}

pub(super) struct ViewerRecordSelection<'a> {
    pub(super) ids: &'a std::collections::BTreeSet<String>,
    pub(super) children: &'a crate::server::ViewerChildren,
    pub(super) failed_launch_retries: &'a std::collections::BTreeSet<String>,
}

pub(super) fn viewer_snapshot_selected(
    controller: &Controller,
    workspaces: &[mj_core::workspace::WorkspaceRecord],
    quotas: &std::collections::BTreeMap<String, ProfileQuota>,
    views: &PhoneSessionViews<'_>,
    revision: u64,
    selection: Option<ViewerRecordSelection<'_>>,
    availability: &mj_client::availability::RuntimeAvailability,
) -> ViewerSnapshot {
    let PhoneSessionViews {
        native_agents,
        conversations,
        reviews,
        queued_prompts,
        active_user_shells,
        pending_elicitations,
        prompt_images,
        operational,
        materialized_activity,
        project_sources,
        operations,
        move_recoveries,
        capacity,
        launch_failures,
    } = views;
    let mut snapshot = match selection {
        Some(selection) => ViewerSnapshot::from_config_records(
            &controller.config,
            &controller.state,
            revision,
            selection
                .ids
                .iter()
                .filter_map(|id| controller.state.sessions.get(id)),
            selection.children,
            Some(selection.failed_launch_retries),
        ),
        None => ViewerSnapshot::from_config_state(&controller.config, &controller.state, revision),
    };
    snapshot.target_availability_notes = availability.target_notes.clone();
    for profile in &mut snapshot.profiles {
        profile.availability = availability
            .profiles
            .get(&profile.id)
            .cloned()
            .unwrap_or_default();
    }
    for target in &mut snapshot.targets {
        target.availability = availability
            .targets
            .get(&target.id)
            .cloned()
            .unwrap_or_default();
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
        let durable_activity = materialized_activity
            .get(&session.id)
            .copied()
            .unwrap_or_default();
        session.last_activity_at_ms = durable_activity.last_activity_at_ms;
        session.queued_prompts = queued_prompts
            .get(&session.id)
            .into_iter()
            .flatten()
            .map(|prompt| ViewerQueuedPrompt {
                id: prompt.id.clone(),
                kind: prompt.kind.clone(),
                text: prompt.text.clone(),
                created_at: prompt.created_at_ms.to_string(),
            })
            .collect();
        session.active_user_shells = active_user_shells
            .get(&session.id)
            .into_iter()
            .flatten()
            .map(|shell| ViewerUserShell {
                id: shell.command_id.clone(),
                command: shell.command.clone(),
                started_at_ms: shell.started_at_ms,
            })
            .collect();
        session.background_tasks = operational
            .get(&session.id)
            .map(|state| {
                state
                    .background_commands
                    .iter()
                    .map(|task| ViewerBackgroundTask {
                        id: task.id.clone(),
                        command: task.command.clone(),
                        started_at_ms: task.started_at_ms,
                        can_stop: task.can_stop,
                    })
                    .collect()
            })
            .unwrap_or_default();
        session.pending_elicitations = pending_elicitations
            .get(&session.id)
            .cloned()
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
        session.prompt_images_supported = prompt_images.contains(&session.id);
        session.operation = operations.get(&session.id).cloned().map(Box::new);
        // Runtime ownership takes precedence over the durable record. Move
        // can still look Running while its old conversation is no longer a
        // valid destination, and stopped cleanup/destroy operations have no
        // live lifecycle category of their own.
        if let Some(operation) = session.operation.as_ref()
            && operation.kind.transition_kind().is_some()
        {
            session.transitioning = true;
        }
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
        let live = operational.get(&session.id).map(|state| &**state);
        session.storage_problem = controller
            .state
            .sessions
            .get(&session.id)
            .and_then(mj_controller::target_storage::session_problem);
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
            Some(state) => state.activity_state(),
            None => mj_core::activity::while_disconnected(
                durable_activity.execution,
                durable_activity.last_activity_at_ms,
            ),
        };
        // A session the daemon can see reports the phase its own flag names,
        // corrected for a turn that flag has not caught up with; one it cannot
        // see reports what was last known.
        let chat_phase = match live {
            Some(state) => mj_core::activity::chat_phase(&state.facts()),
            None => activity_state.chat_phase(),
        };
        session.chat_phase = match chat_phase {
            mj_core::relay::RelayExecutionState::Idle => crate::server::ViewerChatPhase::Idle,
            mj_core::relay::RelayExecutionState::Running => crate::server::ViewerChatPhase::Running,
            mj_core::relay::RelayExecutionState::Closing => crate::server::ViewerChatPhase::Closing,
            mj_core::relay::RelayExecutionState::Closed => crate::server::ViewerChatPhase::Closed,
        };
        // A parked sub-agent is idle: its turn ended and its worker is
        // stopped until its parent sends it more input.
        session.is_idle = activity_state.is_idle()
            && controller
                .state
                .sessions
                .get(&session.id)
                .is_some_and(|record| {
                    matches!(
                        record.state,
                        mj_core::state::SessionState::Running
                            | mj_core::state::SessionState::Parked
                    )
                })
            && session.operation.is_none();
        session.question_message = question_message(live, session.attention);
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
        // Two capability inputs the session row itself does not carry: whether
        // its configured target template runs a container, and whether any
        // session in its sub-agent tree has a turn running. The InterruptAll
        // execution walks the same tree this measures.
        let container_target = controller
            .state
            .sessions
            .get(&session.id)
            .and_then(|record| controller.config.targets.get(&record.target_template_id))
            .is_some_and(mj_core::config::is_container_target);
        let tree_turn_running = std::iter::once(session.id.as_str())
            .chain(
                controller
                    .state
                    .subagents
                    .values()
                    .filter(|child| child.parent_session_id == session.id)
                    .map(|child| child.child_session_id.as_str()),
            )
            .any(|id| {
                operational.get(id).is_some_and(|state| {
                    state.active_prompt.is_some() || state.harness_turn.is_some()
                })
            });
        session.capabilities = session_capabilities(
            session,
            live,
            operations.get(&session.id),
            facts.as_ref(),
            workspaces.len(),
            container_target,
            tree_turn_running,
        );
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

fn question_message(
    live: Option<&mj_core::relay::RelayOperationalState>,
    attention: mj_core::attention::SessionAttention,
) -> Option<String> {
    if attention != mj_core::attention::SessionAttention::NeedsInput {
        return None;
    }
    live?.question_message().map(str::to_owned)
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

/// What the durable projection last recorded about a session's turn.
///
/// The daemon keeps this for every session, including ones it currently has
/// no live connection to, because that is the only honest thing it can report
/// about a worker it cannot see. Reporting the default value of an enum
/// instead is what made a running turn look finished after a daemon restart
/// (#1025).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct MaterializedActivity {
    pub(super) last_activity_at_ms: Option<i64>,
    pub(super) execution: mj_core::state::MaterializedExecutionState,
}

/// Seed the viewer's in-memory activity cache from the durable projection
/// before publishing its first snapshot. Later worker snapshots update this
/// cache without adding a database read to the render path.
pub(super) async fn load_materialized_activity(
    controller: &Controller,
) -> Result<std::collections::BTreeMap<String, MaterializedActivity>> {
    let session_ids = controller
        .state
        .sessions
        .keys()
        .cloned()
        .collect::<Vec<_>>();
    mj_core::runtime::spawn_blocking(move || {
        let activity_by_session = mj_controller::database::load_materialized_activity()?;
        Ok(session_ids
            .into_iter()
            .map(|session_id| {
                let activity = activity_by_session
                    .get(&session_id)
                    .map(|(last_activity_at_ms, execution)| MaterializedActivity {
                        last_activity_at_ms: *last_activity_at_ms,
                        execution: *execution,
                    })
                    .unwrap_or_default();
                (session_id, activity)
            })
            .collect::<std::collections::BTreeMap<_, _>>())
    })
    .await
    .context("materialized activity startup task failed")?
}
