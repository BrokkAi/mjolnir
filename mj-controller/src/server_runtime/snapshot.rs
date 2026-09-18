use super::*;

/// The live, per-session projections the phone snapshot layers on top of the
/// controller's durable state. They arrive from relay snapshots rather than
/// from disk, so they travel together instead of as separate arguments.
pub(super) struct PhoneSessionViews<'a> {
    pub(super) conversations:
        &'a std::collections::BTreeMap<String, crate::server::BrowserTranscript>,
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
    pub(super) operational:
        &'a std::collections::BTreeMap<String, mj_core::relay::RelayOperationalState>,
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

/// What the phone server remembers about one probe target between readings.
///
/// The last good reading is kept beside any failure, because one failed probe
/// is not a reason to forget what a machine was doing a minute ago; the phone
/// is told both, and says so.
#[derive(Debug, Clone)]
pub(super) struct PhoneCapacity {
    pub(super) target: crate::targets::DeploymentCapacityTarget,
    pub(super) usage: Option<crate::targets::DeploymentCapacityUsage>,
    pub(super) on_demand: bool,
    pub(super) sampled_at_epoch_seconds: Option<u64>,
    pub(super) refreshing: bool,
    pub(super) failed: bool,
}

/// How old a reading may be before the page says so.
pub(super) const CAPACITY_STALE_AFTER: Duration = Duration::from_secs(120);

/// Tell the poller which targets to probe, and keep the state map in step.
pub(super) fn publish_capacity_targets(
    controller: &Controller,
    targets_tx: &tokio::sync::watch::Sender<Vec<crate::targets::DeploymentCapacityTarget>>,
    state: &mut std::collections::BTreeMap<String, PhoneCapacity>,
) {
    let targets = controller.deployment_capacity_targets();
    state.retain(|id, _| targets.iter().any(|target| target.id == *id));
    for target in &targets {
        state
            .entry(target.id.clone())
            .and_modify(|entry| entry.target = target.clone())
            .or_insert_with(|| PhoneCapacity {
                target: target.clone(),
                usage: None,
                on_demand: false,
                sampled_at_epoch_seconds: None,
                // A target with no reading yet is loading, not idle.
                refreshing: true,
                failed: false,
            });
    }
    if targets_tx.borrow().as_slice() != targets.as_slice() {
        targets_tx.send_replace(targets);
    }
}

/// Project the capacity readings for the phone.
pub(super) fn viewer_capacity(
    state: &std::collections::BTreeMap<String, PhoneCapacity>,
) -> Vec<crate::server::ViewerTargetCapacity> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    state
        .values()
        .map(|entry| {
            let usage = entry.usage.as_ref();
            crate::server::ViewerTargetCapacity {
                id: entry.target.id.clone(),
                label: entry.target.host.clone(),
                target_ids: entry.target.target_ids.clone(),
                cpu_percent: usage.and_then(|usage| usage.cpu_percent),
                memory_used_bytes: usage.map(|usage| usage.memory_used_bytes),
                memory_total_bytes: usage.map(|usage| usage.memory_total_bytes),
                logical_cores: usage.map(|usage| usage.logical_cores),
                disk_total_bytes: usage.and_then(|usage| usage.disk_total_bytes),
                // A fleet reports how many machines it is running; a plain host
                // has no such count and says nothing rather than zero.
                virtual_machines: matches!(
                    entry.target.kind,
                    crate::targets::DeploymentCapacityKind::AwsFleet
                )
                .then(|| u64::from(!entry.on_demand)),
                sampled_at_epoch_seconds: entry.sampled_at_epoch_seconds,
                refreshing: entry.refreshing,
                stale: entry.sampled_at_epoch_seconds.is_some_and(|sampled| {
                    now.saturating_sub(sampled) > CAPACITY_STALE_AFTER.as_secs()
                }),
                has_error: entry.failed,
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
            crate::daemon::RuntimeLifecycleKind::Close
            | crate::daemon::RuntimeLifecycleKind::ForceStop => ViewerOperationKind::Stop,
            crate::daemon::RuntimeLifecycleKind::DestroyStopped
            | crate::daemon::RuntimeLifecycleKind::ForceDestroy => ViewerOperationKind::Destroy,
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
        open: session.conversation_available
            && !session.transitioning
            && session.lifecycle == ViewerLifecycleCategory::Live,
        prompt: live && attached && !mutation_busy,
        run_shell: live && attached && !mutation_busy,
        cancel_turn: live
            && !mutation_busy
            && operational.is_some_and(|state| {
                state.active_prompt.is_some() || state.capacity_retry.is_some()
            }),
        cancel_operation: operation.is_some_and(|operation| operation.cancellable),
        // Stopping a session that is already stopping asks for something that
        // is happening; resuming one that is running asks for a second copy.
        stop: session.lifecycle.is_dashboard_visible() && !mutation_busy,
        rename: !session.transitioning,
        resume: !session.lifecycle.is_dashboard_visible() && !mutation_busy,
        move_session: live && !busy,
        set_config: live && attached && facts.is_some() && !mutation_busy,
        // Plan mode is a turn boundary: the terminal offers it only while the
        // agent is idle, and the phone must not be looser.
        set_plan_mode: live
            && !mutation_busy
            && idle
            && facts.is_some_and(mj_core::acp::AcpSessionFacts::supports_plan_mode),
    }
}

/// The ACP content blocks one phone prompt becomes: its text, then each
/// attached image as the image block the prompt path already carries.
pub(super) fn phone_prompt_blocks(
    text: String,
    images: Vec<crate::server::ViewerPromptImage>,
) -> Vec<agent_client_protocol::schema::v1::ContentBlock> {
    use agent_client_protocol::schema::v1::{ContentBlock, ImageContent, TextContent};

    let mut prompt = Vec::with_capacity(images.len() + 1);
    if !text.is_empty() {
        prompt.push(ContentBlock::Text(TextContent::new(text)));
    }
    prompt.extend(images.into_iter().map(|image| match image.attachment {
        Some(reference) => reference.content_block(),
        None => ContentBlock::Image(ImageContent::new(image.data_base64, image.mime_type)),
    }));
    prompt
}

/// The Mjolnir commands a phone may offer for one session.
///
/// The list is built here, from what this session can actually do, and
/// published: the browser used to keep its own copy, which is how `/review`
/// was missing from the phone while the terminal had it.
pub(super) fn phone_commands(
    session: &crate::server::ViewerSession,
    operational: Option<&mj_core::relay::RelayOperationalState>,
) -> Vec<crate::server::ViewerMjCommand> {
    use crate::server::ViewerCommandSource;
    use agent_client_protocol::schema::v1::AvailableCommandInput;

    let command =
        |name: &str, description: &str, argument: Option<&str>| crate::server::ViewerMjCommand {
            name: name.to_owned(),
            description: description.to_owned(),
            source: ViewerCommandSource::Mj,
            argument: argument.map(str::to_owned),
        };
    let mut commands = vec![
        command("help", "show available Mjolnir and agent commands", None),
        command(
            "detach",
            "leave the conversation without stopping the worker",
            None,
        ),
    ];
    let option = |key: &str| {
        session
            .config_options
            .iter()
            .any(|option| option.key == key)
    };
    if option("model") {
        commands.push(command("model", "change the active model", Some("value")));
        commands.push(command("fast", "toggle Codex Fast mode", None));
    }
    if option("effort") {
        commands.push(command(
            "effort",
            "change the active reasoning effort",
            Some("value"),
        ));
    }
    if session.plan_mode_active.is_some() && session.capabilities.set_plan_mode {
        commands.push(command("plan", "toggle plan mode", Some("message")));
        commands.push(command(
            "implement",
            "leave plan mode and implement",
            Some("instruction"),
        ));
    }
    if session.capabilities.prompt || session.turn_review.is_some() {
        commands.push(command(
            "review",
            "review the finished turn now, or report how review is configured",
            Some("status"),
        ));
    }
    // These names are handled by Mjolnir even when the corresponding control
    // is unavailable for this session. An agent cannot claim one and turn a
    // locally interpreted slash command into a misleading palette entry.
    let reserved = [
        "help",
        "detach",
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
        commands.push(crate::server::ViewerMjCommand {
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

/// Refresh durable Move records away from the phone event loop. A Move can
/// finish after its initiating request disconnects, so active lifecycle views
/// alone are not enough to render Retry move or Resume with previous settings.
pub(super) fn request_move_recovery_reload(
    completed: &tokio::sync::mpsc::UnboundedSender<Result<ViewerMoveRecoveries, String>>,
    in_flight: &mut bool,
    jobs: &mut tokio::task::JoinSet<()>,
) {
    if *in_flight {
        return;
    }
    *in_flight = true;
    let completed = completed.clone();
    jobs.spawn(async move {
        let result = match tokio::task::spawn_blocking(|| {
            crate::database::load_move_operations()
                .map(|operations| {
                    operations
                        .into_iter()
                        .filter_map(|operation| {
                            crate::server::ViewerMoveRecovery::from_operation(&operation)
                                .map(|recovery| (operation.selection.session_id.clone(), recovery))
                        })
                        .collect()
                })
                .map_err(|error| format!("{error:#}"))
        })
        .await
        {
            Ok(result) => result,
            Err(error) => Err(format!("move recovery projection task failed: {error}")),
        };
        let _ = completed.send(result);
    });
}

pub(super) fn viewer_snapshot(
    controller: &Controller,
    workspaces: &[mj_core::workspace::WorkspaceRecord],
    quotas: &std::collections::BTreeMap<String, ProfileQuota>,
    views: &PhoneSessionViews<'_>,
    revision: u64,
) -> ViewerSnapshot {
    let PhoneSessionViews {
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
    let mut snapshot =
        ViewerSnapshot::from_config_state(&controller.config, &controller.state, revision);
    snapshot.launch_failures = launch_failures.to_vec();
    snapshot.workspaces = workspaces
        .iter()
        .map(|workspace| crate::server::ViewerWorkspace {
            id: workspace.id.clone(),
            name: workspace.name.clone(),
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
        profile.quota = Some(ViewerQuota {
            summary: quota.compact(),
            windows: quota
                .windows
                .iter()
                .map(|window| crate::server::ViewerQuotaWindow {
                    label: window.label.clone(),
                    // The controller reports headroom; a bar fills as a limit
                    // is consumed, so the phone is given the complement.
                    percent_used: window
                        .remaining_percent
                        .map(|left| 100_u8.saturating_sub(left)),
                    resets_at: window.resets.clone(),
                    projects_exhaustion_before_reset: crate::quota::projects_exhaustion(
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
            has_error: quota.error.is_some(),
        });
    }
    for session in &mut snapshot.sessions {
        session.move_recovery = move_recoveries.get(&session.id).cloned();
        if let Some(record) = controller.state.sessions.get(&session.id)
            && let Some(source) = project_sources.source(record, &controller.config)
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
        session.prompt_images_supported = prompt_images.contains(&session.id);
        session.operation = operations.get(&session.id).cloned();
        // Runtime ownership takes precedence over the durable record. Move
        // can still look Running while its old conversation is no longer a
        // valid destination, and stopped cleanup/destroy operations have no
        // live lifecycle category of their own.
        if let Some(operation) = session.operation.as_ref()
            && operation.kind.transition_kind().is_some()
        {
            session.transitioning = true;
        }
        let live = operational.get(&session.id);
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
        session.is_idle = activity_state.is_idle()
            && controller
                .state
                .sessions
                .get(&session.id)
                .is_some_and(|record| record.state == mj_core::state::SessionState::Running)
            && session.operation.is_none();
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
            session.capacity_retry = state.capacity_retry.clone();
            let activity = mj_client::usage_format::SessionActivity::of(state);
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
            .map(crate::server::ViewerTurnReview::from_runtime);
        session.capabilities =
            session_capabilities(session, live, operations.get(&session.id), facts.as_ref());
        session.available_commands = phone_commands(session, live);
        if let Some(transcript) = conversations.get(&session.id) {
            session.conversation_available = true;
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
            | mj_client::usage_format::SessionActivityKind::Goal => ViewerActivityKind::Lifecycle,
        },
        turn_started_at_ms: details.turn_started_at_ms,
        step_started_at_ms: details.step_started_at_ms,
        background_started_at_ms: details.background_started_at_ms,
        idle_since_ms: details.idle_since_ms,
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
    tokio::task::spawn_blocking(move || {
        session_ids
            .into_iter()
            .map(|session_id| {
                crate::database::load_materialized_session_summary(&session_id).map(|summary| {
                    let activity = summary
                        .map(|summary| MaterializedActivity {
                            last_activity_at_ms: summary.last_activity_at_ms,
                            execution: summary.execution,
                        })
                        .unwrap_or_default();
                    (session_id, activity)
                })
            })
            .collect()
    })
    .await
    .context("materialized activity startup task failed")?
}
