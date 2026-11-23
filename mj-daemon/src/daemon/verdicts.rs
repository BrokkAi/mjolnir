use super::*;
use mj_client::runtime_feed::{RuntimeProjection, SessionOffer, SessionVerdict};
use mj_core::attention::SessionAttention;
use mj_core::native_agent::NativeAgentState;
use mj_core::state::SessionTransitionKind;
use mj_core::subagent::SubagentKind;

type Children<'a> = BTreeMap<String, Vec<(&'a str, Option<SubagentKind>)>>;

fn is_active_member(record: &SessionRecord) -> bool {
    record.state.has_live_worker() && !record.archived
}

fn children(runtime: &RuntimeProjection) -> Children<'_> {
    let mut children = Children::new();
    for (id, child) in &runtime.subagents {
        if runtime.records.get(id).is_some_and(is_active_member) {
            children
                .entry(child.parent_session_id.clone())
                .or_default()
                .push((id, Some(child.kind)));
        }
    }
    for (id, child) in &runtime.native_agents {
        if child.agent.state != NativeAgentState::Running {
            continue;
        }
        let parent = child.agent.parent_view_id();
        children.entry(parent).or_default().push((id, None));
    }
    children
}

fn interrupt_targets<'a>(
    root: &'a str,
    runtime: &RuntimeProjection,
    children: &Children<'a>,
    verdict: impl Fn(&str) -> Option<SessionVerdict>,
) -> Vec<&'a str> {
    let mut pending = vec![root];
    let mut seen = BTreeSet::new();
    let mut targets = Vec::new();
    while let Some(id) = pending.pop() {
        if !seen.insert(id) {
            continue;
        }
        let native_owner_held = runtime
            .native_agents
            .get(id)
            .and_then(|native| verdict(&native.agent.owner_session_id))
            .is_some_and(|v| v.move_queue_held);
        // Native children share their owner's worker. Managed descendants
        // have independent workers and are visited even under a held Move.
        if !native_owner_held && verdict(id).is_some_and(|v| v.interrupt_member) {
            targets.push(id);
        }
        pending.extend(children.get(id).into_iter().flatten().map(|(id, _)| *id));
    }
    targets
}

fn lifecycle_transition(kind: RuntimeLifecycleKind) -> SessionTransitionKind {
    use SessionTransitionKind::*;
    match kind {
        RuntimeLifecycleKind::Create => Starting,
        RuntimeLifecycleKind::Resume => Resuming,
        RuntimeLifecycleKind::Move => Moving,
        RuntimeLifecycleKind::Suspend => Suspending,
        RuntimeLifecycleKind::DestroyStopped
        | RuntimeLifecycleKind::ForceDestroy
        | RuntimeLifecycleKind::Cleanup => Destroying,
        RuntimeLifecycleKind::StopSubagent => Stopping,
    }
}

pub(super) fn transition_kind(
    record: &SessionRecord,
    lifecycle: Option<RuntimeLifecycleKind>,
    move_op: Option<&mj_core::state::MoveOperation>,
) -> Option<SessionTransitionKind> {
    lifecycle
        .map(lifecycle_transition)
        .or_else(|| {
            move_op
                .filter(|m| m.is_active())
                .map(|_| SessionTransitionKind::Moving)
        })
        .or_else(|| {
            record
                .last_error
                .is_none()
                .then(|| record.state.transition_kind())
                .flatten()
        })
}

fn needs_person(attention: Option<&SessionAttention>) -> bool {
    matches!(
        attention,
        Some(
            SessionAttention::NeedsInput
                | SessionAttention::Important
                | SessionAttention::Unread
                | SessionAttention::Failed
                | SessionAttention::Unreachable
        )
    )
}

fn has_waiting_form(view: Option<&RuntimeSessionView>) -> bool {
    view.and_then(|view| view.materialized.as_deref())
        .is_some_and(|view| !view.pending_elicitations.is_empty())
}

fn turn_interrupt_command(
    live: Option<&mj_core::relay::RelayOperationalState>,
) -> Result<RelayCommand> {
    let command = live.and_then(|live| live.turn_controls.as_ref()?.interrupt.as_ref());
    // A raw cancel can hit a newly started prompt after autonomous
    // goal work settles between capture and worker admission.
    ensure!(
        !live.is_some_and(|live| live.goal.running() && live.active_prompt.is_none())
            || matches!(command, Some(RelayCommand::CancelTurnFor { .. })),
        "this worker cannot target the current autonomous goal turn; open the session to use its goal controls"
    );
    if let Some(command) = command {
        return Ok(command.clone());
    }
    // When a turn-controls-capable worker has an active prompt but publishes no
    // interrupt command (e.g. cancellation is already pending or a checkpoint
    // barrier is active), never fall back to raw CancelTurn: it could outlive
    // the active prompt and cancel the next queued prompt.
    if let Some(live) = live
        && live.active_prompt.is_some()
        && let Some(controls) = live.turn_controls.as_ref()
    {
        bail!(
            "{}",
            controls
                .message
                .as_deref()
                .unwrap_or("this turn cannot be interrupted right now")
        );
    }
    Ok(RelayCommand::CancelTurn)
}

fn subagent_waiting_for_background(
    runtime: &RuntimeProjection,
    committed: Option<&mj_controller::database::CommittedState>,
    id: &str,
    live: Option<&mj_core::relay::RelayOperationalState>,
) -> bool {
    let Some(subagent) = runtime.subagents.get(id) else {
        return false;
    };
    if !subagent.handback_tool {
        return false;
    }
    let Some(committed) = committed else {
        return false;
    };
    let Some(live) = live else {
        return false;
    };
    let empty = mj_core::subagent::SubagentReport::default();
    let report = committed.subagent_reports.get(id).unwrap_or(&empty);
    let turn = committed.turns.get(id);
    let active = turn.and_then(|turn| turn.state.1.as_ref());
    let last = turn.and_then(|turn| turn.state.2.as_ref());
    let in_flight = active
        .into_iter()
        .map(|turn| turn.command_id.as_str())
        .collect::<Vec<_>>();
    let liveness = mj_core::subagent::Liveness::Known(live.work_liveness.clone());
    matches!(
        mj_core::subagent::report_state(
            subagent.handback_tool,
            report,
            last,
            &liveness,
            &in_flight,
            mj_core::clock::epoch_millis(),
        ),
        mj_core::subagent::ReportState::Held { .. }
    )
}

pub(crate) fn projected_activity(
    view: Option<&RuntimeSessionView>,
) -> mj_core::activity::ActivityState {
    if let Some(state) = view
        .filter(|view| view.connected)
        .and_then(|view| view.operational.as_deref())
    {
        return state.activity_state();
    }
    let materialized = view.and_then(|view| view.materialized.as_deref());
    mj_core::activity::while_disconnected(
        materialized.map_or(Default::default(), |view| view.execution),
        materialized.and_then(|view| view.last_activity_at_ms),
    )
}

fn own_verdict(
    runtime: &RuntimeProjection,
    committed: Option<&mj_controller::database::CommittedState>,
    id: &str,
    record: &SessionRecord,
    lifecycle: Option<RuntimeLifecycleKind>,
) -> SessionVerdict {
    use SessionOffer::*;
    let move_op = runtime.moves.get(id);
    let transition = transition_kind(record, lifecycle, move_op);
    let transition_failure = (transition.is_none()
        && record.last_error.is_some()
        && matches!(
            record.state,
            SessionState::Closing | SessionState::Destroying
        ))
    .then(|| record.state.transition_kind())
    .flatten();
    let queue_held =
        move_op.is_some_and(|m| m.queue_admission_started && !m.queue_admission_finished);
    let operation = if queue_held {
        MoveQueueHeld
    } else if transition.is_some() {
        Transitioning
    } else if transition_failure.is_some() {
        TransitionFailed
    } else if record.state == SessionState::Checkpointing {
        OperationInProgress
    } else {
        Ready
    };
    let state = record.state;
    let view = runtime.sessions.get(id);
    let live = view.and_then(|v| v.operational.as_deref());
    let form = has_waiting_form(view);
    let attention = runtime.metadata.session_attention.get(id);
    let running = live.is_some_and(|live| {
        live.active_prompt.is_some()
            || live.harness_turn.is_some()
            || live.goal.running()
            || live
                .capacity_retry
                .as_ref()
                .is_some_and(|retry| !retry.submitted)
            || live
                .continuation
                .quota_recovery
                .as_ref()
                .is_some_and(|retry| !retry.submitted)
            || runtime
                .transcripts
                .get(id)
                .is_some_and(|tail| tail.header.active_turn.is_some())
    });
    let question = !form && super::attention::semantic_question(runtime, id).is_some();
    let unfinished_turn =
        state.has_live_worker() && live.is_some() && (running || form || question);
    let active_work = state.has_live_worker()
        && !form
        && (running
            || live.is_some_and(mj_core::relay::RelayOperationalState::has_work_in_flight)
            // Retained turn facts still inform interrupt and work ownership.
            || live.is_some_and(|state| state.activity_state().has_work_in_flight()));
    let can_interrupt = !queue_held && !form && state.has_live_worker();
    let turn_targetable = !queue_held && unfinished_turn && turn_interrupt_command(live).is_ok();
    let failed_first_launch = runtime.metadata.failed_launch_retries.contains(id);
    let resume = if failed_first_launch && operation == Ready {
        Ready
    } else if operation != Ready {
        operation
    } else if state.is_active() && state != SessionState::Error {
        Active
    } else if move_op.is_some_and(|m| m.holds_source_environment()) {
        MoveEnvironmentRetained
    } else if !matches!(
        state,
        SessionState::Lost | SessionState::DestroyedWithDataLoss
    ) && (record.checkpoint.is_some()
        || (state == SessionState::Error && record.target.is_some()))
    {
        Ready
    } else {
        NoRecoveryCopy
    };
    SessionVerdict {
        transition,
        transition_failure,
        live_dashboard: !record.archived && (state.is_active() || transition.is_some()),
        needs_person: needs_person(attention),
        move_queue_held: queue_held,
        idle: transition.is_none()
            && (state == SessionState::Parked
                || state == SessionState::Running && projected_activity(view).is_idle()),
        working: state == SessionState::Running
            && view.is_some_and(|v| v.connected)
            && (live.is_some_and(|live| {
                mj_client::usage_format::SessionActivity::of(live).is_working(None, form)
            }) || subagent_waiting_for_background(runtime, committed, id, live)),
        active_work,
        unfinished: unfinished_turn || active_work,
        interrupt: can_interrupt && running && turn_targetable,
        interrupt_shell: can_interrupt
            && live.is_some_and(|live| !live.active_user_shells.is_empty()),
        interrupt_member: !queue_held && unfinished_turn && (question || turn_targetable),
        stop: if state.is_active() {
            if operation == TransitionFailed {
                Ready
            } else {
                operation
            }
        } else {
            Hidden
        },
        restart: if state.is_active() || record.checkpoint.is_some() {
            operation
        } else {
            NoRecoveryCopy
        },
        move_session: if state.is_active() { operation } else { Hidden },
        change_workspace: if runtime.metadata.workspaces.len() < 2 {
            NoOtherWorkspace
        } else if queue_held {
            MoveQueueHeld
        } else {
            Ready
        },
        container_settings: if runtime
            .metadata
            .config
            .targets
            .get(&record.target_template_id)
            .is_some_and(mj_core::config::is_container_target)
        {
            if queue_held { MoveQueueHeld } else { Ready }
        } else {
            Hidden
        },
        resume,
        ..Default::default()
    }
}

/// Revisit only changed inputs. A child can affect every ancestor's Interrupt
/// All offer, and a reparent can affect both its old and new family.
pub(crate) fn project(
    runtime: &mut RuntimeProjection,
    previous: &RuntimeProjection,
    committed: Option<&mj_controller::database::CommittedState>,
    previous_committed: Option<&mj_controller::database::CommittedState>,
) {
    let lifecycles: BTreeMap<_, _> = runtime
        .metadata
        .lifecycles
        .iter()
        .map(|view| (view.session_id.as_str(), view.kind))
        .collect();
    let mut changed: BTreeSet<_> = previous
        .records
        .changes(&runtime.records)
        .map(|(id, _)| id.as_str())
        .chain(
            previous
                .sessions
                .changes(&runtime.sessions)
                .map(|(id, _)| id.as_str()),
        )
        .chain(
            previous
                .moves
                .changes(&runtime.moves)
                .map(|(id, _)| id.as_str()),
        )
        .chain(
            previous
                .subagents
                .changes(&runtime.subagents)
                .map(|(id, _)| id.as_str()),
        )
        .chain(
            previous
                .native_agents
                .changes(&runtime.native_agents)
                .map(|(id, _)| id.as_str()),
        )
        .chain(
            previous
                .transcripts
                .changes(&runtime.transcripts)
                .map(|(id, _)| id.as_str()),
        )
        .chain(
            previous
                .metadata
                .session_attention
                .changes(&runtime.metadata.session_attention)
                .map(|(id, _)| id.as_str()),
        )
        .chain(
            previous
                .metadata
                .notification_dismissals
                .changes(&runtime.metadata.notification_dismissals)
                .map(|(id, _)| id.as_str()),
        )
        .collect();
    if previous.metadata.lifecycles != runtime.metadata.lifecycles {
        changed.extend(
            previous
                .metadata
                .lifecycles
                .iter()
                .chain(&runtime.metadata.lifecycles)
                .map(|operation| operation.session_id.as_str()),
        );
    }
    if previous.metadata.config.targets != runtime.metadata.config.targets
        || previous.metadata.workspaces.len() != runtime.metadata.workspaces.len()
    {
        changed.extend(runtime.records.keys().map(String::as_str));
    }
    match (previous_committed, committed) {
        (Some(prev), Some(curr)) => {
            changed.extend(
                prev.subagent_reports
                    .changes(&curr.subagent_reports)
                    .map(|(id, _)| id.as_str()),
            );
            changed.extend(prev.turns.changes(&curr.turns).map(|(id, _)| id.as_str()));
        }
        (None, Some(_)) | (Some(_), None) => {
            changed.extend(runtime.subagents.keys().map(String::as_str));
        }
        (None, None) => {}
    }

    let mut verdicts = previous.verdicts.clone();
    if changed.is_empty() {
        runtime.verdicts = verdicts;
        return;
    }
    let mut families = BTreeSet::new();
    for id in changed {
        let verdict = if let Some(record) = runtime.records.get(id) {
            Some(own_verdict(
                runtime,
                committed,
                id,
                record,
                lifecycles.get(id).copied(),
            ))
        } else {
            runtime.native_agents.get(id).map(|native| {
                let running = native.agent.state == NativeAgentState::Running;
                SessionVerdict {
                    working: running,
                    unfinished: running,
                    interrupt_member: running && native.agent.capabilities.cancel,
                    needs_person: needs_person(runtime.metadata.session_attention.get(id)),
                    ..Default::default()
                }
            })
        };
        let before = previous.verdicts.get(id).copied();
        let rollup_changed = match (before, verdict) {
            (Some(old), Some(new)) => {
                (
                    old.working,
                    old.unfinished,
                    old.interrupt_member,
                    old.move_queue_held,
                ) != (
                    new.working,
                    new.unfinished,
                    new.interrupt_member,
                    new.move_queue_held,
                )
            }
            (None, None) => false,
            _ => true,
        };
        let parent_changed = previous.subagents.get(id) != runtime.subagents.get(id)
            || previous.native_agents.get(id).map(|view| &view.agent)
                != runtime.native_agents.get(id).map(|view| &view.agent)
            || previous.records.get(id).map(is_active_member)
                != runtime.records.get(id).map(is_active_member);
        match verdict {
            Some(mut verdict) => {
                if let Some(old) = before {
                    verdict.working_children = old.working_children;
                    verdict.unfinished_children = old.unfinished_children;
                    verdict.interrupt_count = old.interrupt_count;
                    verdict.interrupt_all = old.interrupt_all;
                }
                if before != Some(verdict) {
                    verdicts.insert(id.to_owned(), verdict);
                }
            }
            None => {
                verdicts.remove(id);
            }
        }
        if !rollup_changed && !parent_changed {
            continue;
        }
        if before.is_some_and(|v| v.move_queue_held) != verdict.is_some_and(|v| v.move_queue_held) {
            let prefix = mj_core::native_agent::owner_view_id_prefix(id);
            families.extend(runtime.native_agents.keys_with_prefix(&prefix).cloned());
        }
        for runtime in [previous, &*runtime] {
            let mut visited = BTreeSet::new();
            let mut pending = Some(id.to_owned());
            while let Some(current) = pending.take() {
                if !visited.insert(current.clone()) {
                    break;
                }
                pending = runtime
                    .native_agents
                    .get(&current)
                    .map(|v| v.agent.parent_view_id())
                    .or_else(|| {
                        runtime
                            .subagents
                            .get(&current)
                            .map(|r| r.parent_session_id.clone())
                    });
                families.insert(current);
            }
        }
    }

    if families.is_empty() {
        runtime.verdicts = verdicts;
        return;
    }
    let children = children(runtime);
    let own = verdicts.clone();
    for id in families {
        let Some(mut verdict) = own.get(&id).copied() else {
            continue;
        };
        verdict.working_children = 0;
        verdict.unfinished_children = 0;
        let mut native_working = BTreeSet::new();
        for &(child, kind) in children.get(&id).into_iter().flatten() {
            let Some(child_verdict) = own.get(child) else {
                continue;
            };
            match kind {
                Some(kind) => {
                    verdict.unfinished_children += usize::from(child_verdict.unfinished);
                    verdict.working_children +=
                        usize::from(kind == SubagentKind::Worker && child_verdict.working);
                }
                None if child_verdict.working => {
                    let agent = &runtime.native_agents[child].agent;
                    native_working.insert(agent.stable_id.as_ref().unwrap_or(&agent.session_id));
                }
                None => {}
            }
        }
        verdict.working_children += native_working.len();
        verdict.unfinished_children += native_working.len();
        verdict.interrupt_count =
            interrupt_targets(&id, runtime, &children, |id| own.get(id).copied()).len();
        verdict.interrupt_all = verdict.interrupt_count != 0;
        if previous.verdicts.get(&id) == Some(&verdict) {
            verdicts.copy_entry(&previous.verdicts, &id, true);
        } else {
            verdicts.insert(id, verdict);
        }
    }
    runtime.verdicts = verdicts;
}

#[derive(Debug, Clone, PartialEq)]
pub(super) enum InterruptTargetKey {
    ActivePrompt {
        prompt_id: String,
    },
    HarnessTurn {
        started_at_ms: i64,
    },
    DismissQuestion {
        command: RelayCommand,
    },
    Snapshot {
        projection_ordinal: u64,
        projection_digest: String,
        command: RelayCommand,
    },
}

impl InterruptTargetKey {
    fn from_projection(
        view: Option<&RuntimeSessionView>,
        live: Option<&mj_core::relay::RelayOperationalState>,
        command: &RelayCommand,
    ) -> Self {
        if matches!(command, RelayCommand::DismissQuestion { .. }) {
            return Self::DismissQuestion {
                command: command.clone(),
            };
        }
        if let Some(prompt_id) = live
            .and_then(|live| live.active_prompt.as_ref())
            .map(|prompt| prompt.command_id.clone())
        {
            return Self::ActivePrompt { prompt_id };
        }
        if let Some(started_at_ms) = live
            .and_then(|live| live.harness_turn.as_ref())
            .map(|turn| turn.started_at_ms)
        {
            return Self::HarnessTurn { started_at_ms };
        }
        Self::Snapshot {
            projection_ordinal: view.map_or(0, |view| view.projection_ordinal),
            projection_digest: view
                .map(|view| view.projection_digest.clone())
                .unwrap_or_default(),
            command: command.clone(),
        }
    }
}

#[derive(Debug, Clone)]
pub(super) struct InterruptAdmissionEntry {
    key: InterruptTargetKey,
    state: InterruptAdmissionState,
}

#[derive(Debug, Clone)]
enum InterruptAdmissionState {
    InFlight(tokio::sync::watch::Receiver<Option<std::result::Result<(), String>>>),
    Accepted,
}

enum InterruptAdmission<'a> {
    Leader(Box<InterruptAdmissionGuard<'a>>),
    WaitInFlight(tokio::sync::watch::Receiver<Option<std::result::Result<(), String>>>),
    AlreadyAccepted,
}

struct InterruptAdmissionGuard<'a> {
    admissions: &'a Mutex<BTreeMap<String, Vec<InterruptAdmissionEntry>>>,
    session_id: String,
    key: InterruptTargetKey,
    result_tx: tokio::sync::watch::Sender<Option<std::result::Result<(), String>>>,
    settled: bool,
}

impl InterruptAdmissionGuard<'_> {
    fn finish(mut self, result: Result<()>) -> Result<()> {
        self.settled = true;
        {
            let mut guard = self
                .admissions
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if let Some(entries) = guard.get_mut(&self.session_id) {
                if result.is_ok() {
                    if let Some(entry) = entries.iter_mut().find(|entry| {
                        entry.key == self.key
                            && matches!(entry.state, InterruptAdmissionState::InFlight(_))
                    }) {
                        entry.state = InterruptAdmissionState::Accepted;
                    }
                } else {
                    entries.retain(|entry| {
                        !(entry.key == self.key
                            && matches!(entry.state, InterruptAdmissionState::InFlight(_)))
                    });
                    if entries.is_empty() {
                        guard.remove(&self.session_id);
                    }
                }
            }
        }
        let shared = match &result {
            Ok(()) => Ok(()),
            Err(error) => Err(format!("{error:#}")),
        };
        let _ = self.result_tx.send(Some(shared));
        result
    }
}

impl Drop for InterruptAdmissionGuard<'_> {
    fn drop(&mut self) {
        if self.settled {
            return;
        }
        {
            let mut guard = self
                .admissions
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if let Some(entries) = guard.get_mut(&self.session_id) {
                entries.retain(|entry| {
                    !(entry.key == self.key
                        && matches!(entry.state, InterruptAdmissionState::InFlight(_)))
                });
                if entries.is_empty() {
                    guard.remove(&self.session_id);
                }
            }
        }
        let _ = self
            .result_tx
            .send(Some(Err("interrupt submission was cancelled".into())));
    }
}

impl RuntimeState {
    fn admit_turn_interrupt(
        &self,
        runtime: &RuntimeProjection,
        id: &str,
        key: InterruptTargetKey,
    ) -> InterruptAdmission<'_> {
        let mut guard = self
            .interrupt_admissions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        guard.retain(|session_id, entries| {
            entries.retain(|entry| {
                matches!(entry.state, InterruptAdmissionState::InFlight(_))
                    || runtime.records.contains_key(session_id)
            });
            !entries.is_empty()
        });
        let entries = guard.entry(id.to_owned()).or_default();
        if let Some(existing) = entries.iter().find(|entry| entry.key == key) {
            return match &existing.state {
                InterruptAdmissionState::InFlight(result) => {
                    InterruptAdmission::WaitInFlight(result.clone())
                }
                InterruptAdmissionState::Accepted => InterruptAdmission::AlreadyAccepted,
            };
        }
        let (result_tx, result_rx) = tokio::sync::watch::channel(None);
        entries.push(InterruptAdmissionEntry {
            key: key.clone(),
            state: InterruptAdmissionState::InFlight(result_rx),
        });
        InterruptAdmission::Leader(Box::new(InterruptAdmissionGuard {
            admissions: &self.interrupt_admissions,
            session_id: id.to_owned(),
            key,
            result_tx,
            settled: false,
        }))
    }

    pub(crate) async fn interrupt_turn(&self, id: &str) -> Result<()> {
        let runtime = self.runtime_publication()?;
        ensure!(
            runtime
                .verdicts
                .get(id)
                .is_some_and(|verdict| verdict.interrupt),
            "no turn to interrupt"
        );
        self.submit_turn_interrupt(&runtime, id, false).await
    }

    async fn submit_turn_interrupt(
        &self,
        runtime: &RuntimeProjection,
        id: &str,
        include_question: bool,
    ) -> Result<()> {
        let view = runtime.sessions.get(id);
        let live = view.and_then(|view| view.operational.as_deref());
        let question = (include_question && !has_waiting_form(view))
            .then(|| super::attention::semantic_question(runtime, id))
            .flatten();
        let command = if let Some(question) = question {
            ensure!(
                live.is_some_and(|live| super::attention::supports_question_dismissal(
                    live, &question
                )),
                "this worker cannot dismiss its open question; open the session to answer it or stop the session"
            );
            RelayCommand::DismissQuestion { question }
        } else {
            turn_interrupt_command(live)?
        };
        let key = InterruptTargetKey::from_projection(view, live, &command);
        match self.admit_turn_interrupt(runtime, id, key) {
            InterruptAdmission::AlreadyAccepted => Ok(()),
            InterruptAdmission::WaitInFlight(mut rx) => match rx.wait_for(Option::is_some).await {
                Ok(outcome) => match outcome.as_ref() {
                    Some(Ok(())) => Ok(()),
                    Some(Err(message)) => Err(anyhow::anyhow!("{message}")),
                    None => unreachable!("wait_for(Option::is_some) only returns Some"),
                },
                Err(_) => Err(anyhow::anyhow!("interrupt submission was cancelled")),
            },
            InterruptAdmission::Leader(admission) => {
                let result = async {
                    self.session_manager
                        .session(id)
                        .await?
                        .submit(new_command_id("interrupt-turn")?, command)
                        .await?;
                    Ok(())
                }
                .await;
                admission.finish(result)
            }
        }
    }

    pub(crate) async fn interrupt_all(&self, root: &str) -> Result<InterruptAllResult> {
        let runtime = self.runtime_publication()?;
        ensure!(
            runtime.verdicts.contains_key(root),
            "unknown session {root}"
        );
        self.interrupt_all_in_capture(&runtime, root).await
    }

    async fn interrupt_all_in_capture(
        &self,
        runtime: &RuntimeProjection,
        root: &str,
    ) -> Result<InterruptAllResult> {
        let children = children(runtime);
        let targets = interrupt_targets(root, runtime, &children, |id| {
            runtime.verdicts.get(id).copied()
        });
        // Each worker owns its own cancellation. Dispatch all targets before
        // waiting for acknowledgements so a leased or slow worker cannot
        // delay cancellation on an unrelated worker.
        let results = futures::future::join_all(targets.iter().map(|&id| async move {
            let result = if let Some(native) = runtime.native_agents.get(id) {
                let agent = &native.agent;
                match self.session_manager.session(&agent.owner_session_id).await {
                    Ok(worker) => {
                        worker
                            .stop_background_task(format!("native-agent:{}", agent.session_id))
                            .await
                    }
                    Err(error) => Err(error),
                }
            } else {
                self.submit_turn_interrupt(runtime, id, true).await
            };
            result
                .err()
                .map(|error| (id.to_owned(), format!("{error:#}")))
        }))
        .await;
        Ok(InterruptAllResult {
            requested: targets.len(),
            failures: results.into_iter().flatten().collect(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mj_core::native_agent::{NativeAgent, NativeAgentCapabilities, NativeAgentSummary};

    fn native(id: &str, cancellable: bool, state: NativeAgentState) -> NativeAgentSummary {
        NativeAgentSummary {
            agent: NativeAgent {
                owner_session_id: "parent".into(),
                session_id: id.into(),
                state,
                capabilities: NativeAgentCapabilities {
                    cancel: cancellable,
                    ..Default::default()
                },
                parent_session_id: None,
                name: id.to_owned(),
                task: String::new(),
                stable_id: None,
                availability: Default::default(),
                availability_reason: None,
            },
            generation_ordinal: 1,
            projection_ordinal: 0,
            projection_digest: String::new(),
            pending_input: false,
            pending_input_message: None,
            pending_input_request_id: None,
            pending_input_raised_at_ms: None,
        }
    }

    #[test]
    fn retained_native_children_only_count_as_working_when_running() {
        let mut capture = RuntimeProjection::default();
        insert_running_session(&mut capture, "parent");
        for (id, state) in [
            ("active", NativeAgentState::Running),
            ("finished", NativeAgentState::Completed),
            ("failed", NativeAgentState::Failed),
        ] {
            let child = native(id, true, state);
            capture.native_agents.insert(child.agent.view_id(), child);
        }
        project(&mut capture, &Default::default(), None, None);
        assert_eq!(capture.verdicts["parent"].working_children, 1);

        let before = capture.clone();
        capture.records.get_mut("parent").unwrap().last_profile = "destination-profile".into();
        let retained = native("active", true, NativeAgentState::Disconnected);
        let id = retained.agent.view_id();
        capture.native_agents.insert(id.clone(), retained);
        project(&mut capture, &before, None, None);
        assert_eq!(capture.verdicts["parent"].working_children, 0);

        let before = capture.clone();
        let active = capture.native_agents.get_mut(&id).unwrap();
        active.agent.state = NativeAgentState::Running;
        active.agent.availability = mj_core::native_agent::NativeAgentAvailability::Available;
        project(&mut capture, &before, None, None);
        assert_eq!(capture.verdicts["parent"].working_children, 1);
    }

    #[tokio::test]
    async fn interrupt_all_offer_and_execution_cover_the_only_cancellable_native_child() {
        let mut manager = super::super::tests::TestRemoteManager::for_session("parent").await;
        let runtime = super::super::tests::test_runtime_state_with_manager(&manager);
        let parent =
            super::super::tests::runtime_test_session("parent", "default", SessionState::Running);
        let mut capture = RuntimeProjection {
            records: [("parent".into(), parent)].into_iter().collect(),
            ..Default::default()
        };
        for summary in [
            native("running", true, NativeAgentState::Running),
            native("finished", true, NativeAgentState::Cancelled),
            native("unsupported", false, NativeAgentState::Running),
        ] {
            capture
                .native_agents
                .insert(summary.agent.view_id(), summary);
        }
        project(&mut capture, &Default::default(), None, None);
        let scoped = capture.for_subagent_parents(&Default::default());
        assert_eq!(scoped.verdicts["parent"], capture.verdicts["parent"]);
        assert!(capture.verdicts["parent"].interrupt_all);
        assert_eq!(capture.verdicts["parent"].interrupt_count, 1);
        let (outcome, commands) = tokio::join!(
            runtime.interrupt_all_in_capture(&capture, "parent"),
            manager.answer_submissions(Vec::new()),
        );
        let result = outcome.unwrap();
        assert_eq!(result.requested, 1);
        assert!(result.failures.is_empty());
        assert!(commands.is_empty());
        assert_eq!(manager.stopped_tasks, ["native-agent:running"]);

        let before = capture.clone();
        let finished = native("running", true, NativeAgentState::Cancelled);
        capture
            .native_agents
            .insert(finished.agent.view_id(), finished);
        project(&mut capture, &before, None, None);
        assert!(!capture.verdicts["parent"].interrupt_all);

        let before = capture.clone();
        let mut live = mj_core::relay::RelaySnapshot::new("parent".into()).operational_state();
        live.activity = Some(mj_core::activity::ActivityState::Tool {
            tool_call_id: "standalone-tool".into(),
            started_at_ms: 1,
            last_activity_at_ms: None,
        });
        let mut view = RuntimeSessionView::from_managed("parent".into(), Default::default());
        view.operational = Some(live.into());
        capture.sessions.insert("parent".into(), view);
        project(&mut capture, &before, None, None);
        assert!(capture.verdicts["parent"].active_work);
        assert!(capture.verdicts["parent"].unfinished);
        assert!(!capture.verdicts["parent"].interrupt_all);

        for activity in [
            mj_core::activity::ActivityState::Expecting { since_ms: 1 },
            mj_core::activity::ActivityState::Goal,
        ] {
            let before = capture.clone();
            let view = capture.sessions.get_mut("parent").unwrap();
            view.operational.as_mut().unwrap().activity = Some(activity);
            project(&mut capture, &before, None, None);
            assert!(!capture.verdicts["parent"].active_work);
            assert!(!capture.verdicts["parent"].unfinished);
        }
    }

    fn insert_running_session(capture: &mut RuntimeProjection, id: &str) {
        capture.records.insert(
            id.into(),
            super::super::tests::runtime_test_session(id, "default", SessionState::Running),
        );
        let mut live = mj_core::relay::RelaySnapshot::new(id.into()).operational_state();
        live.harness_turn = Some(mj_core::relay::HarnessTurn { started_at_ms: 1 });
        let mut view = RuntimeSessionView::from_managed(id.into(), Default::default());
        view.operational = Some(live.into());
        capture.sessions.insert(id.into(), view);
    }

    fn insert_managed_child(capture: &mut RuntimeProjection) {
        insert_running_session(capture, "child");
        capture.subagents.insert(
            "child".into(),
            mj_core::subagent::SubagentMetadata {
                kind: SubagentKind::Worker,
                child_session_id: "child".into(),
                parent_session_id: "parent".into(),
                task_name: String::new(),
                profile_id: "default".into(),
                model: None,
                effort: None,
                working_directory: Default::default(),
                request_key: String::new(),
                created_at: String::new(),
                noticed_turn: None,
                reported_finish: None,
                handback_tool: false,
                supervisor: false,
            },
        );
    }

    // Hard-won: f1947384e: single-session Interrupt offered an untargetable autonomous goal.
    #[tokio::test]
    async fn single_interrupt_excludes_untargetable_autonomous_goal() {
        let mut manager =
            super::super::tests::TestRemoteManager::for_sessions(&["parent", "child"]).await;
        let runtime = super::super::tests::test_runtime_state_with_manager(&manager);
        let parent =
            super::super::tests::runtime_test_session("parent", "default", SessionState::Running);
        let mut capture = RuntimeProjection {
            records: [("parent".into(), parent)].into_iter().collect(),
            ..Default::default()
        };
        let mut live = mj_core::relay::RelaySnapshot::new("parent".into()).operational_state();
        live.goal.execution = Some(mj_core::goal::GoalExecution {
            version: 1,
            revision: None,
            status: "running".into(),
            turn_id: Some("goal".into()),
        });
        let mut view = RuntimeSessionView::from_managed("parent".into(), Default::default());
        view.operational = Some(live.into());
        capture.sessions.insert("parent".into(), view);
        project(&mut capture, &Default::default(), None, None);
        assert!(!capture.verdicts["parent"].interrupt);
        assert!(capture.verdicts["parent"].unfinished);
        assert!(!capture.verdicts["parent"].interrupt_all);
        assert_eq!(capture.verdicts["parent"].interrupt_count, 0);
        let result = runtime
            .interrupt_all_in_capture(&capture, "parent")
            .await
            .unwrap();
        assert_eq!(result.requested, 0);

        let before = capture.clone();
        insert_managed_child(&mut capture);
        project(&mut capture, &before, None, None);
        assert!(capture.verdicts["parent"].interrupt_all);
        assert_eq!(capture.verdicts["parent"].interrupt_count, 1);
        let (outcome, commands) = tokio::join!(
            runtime.interrupt_all_in_capture(&capture, "parent"),
            manager.answer_submissions(Vec::new()),
        );
        let result = outcome.unwrap();
        assert_eq!(result.requested, 1);
        assert!(result.failures.is_empty());
        assert_eq!(commands.len(), 1);
        assert_eq!(commands[0].0, "child");

        let before = capture.clone();
        let exact_interrupt = RelayCommand::CancelTurnFor {
            active_prompt_id: "goal".into(),
        };
        let view = capture.sessions.get_mut("parent").unwrap();
        view.operational.as_mut().unwrap().turn_controls = Some(mj_core::relay::TurnControls {
            status: mj_core::relay::TurnControlStatus::Ready,
            primary: None,
            interrupt: Some(exact_interrupt.clone()),
            message: None,
        });
        project(&mut capture, &before, None, None);
        assert!(capture.verdicts["parent"].interrupt);
        assert_eq!(capture.verdicts["parent"].interrupt_count, 2);
        let (outcome, commands) = tokio::join!(
            runtime.submit_turn_interrupt(&capture, "parent", false),
            manager.answer_submissions(Vec::new()),
        );
        outcome.unwrap();
        assert_eq!(commands.len(), 1);
        assert_eq!(commands[0].2, exact_interrupt);
    }

    // Hard-won: f1947384e: Interrupt All skipped independent children of a parent with a held Move.
    #[tokio::test]
    async fn interrupt_all_visits_managed_child_under_held_move() {
        let mut manager =
            super::super::tests::TestRemoteManager::for_sessions(&["parent", "child"]).await;
        let runtime = super::super::tests::test_runtime_state_with_manager(&manager);
        let mut capture = RuntimeProjection::default();
        insert_running_session(&mut capture, "parent");
        insert_managed_child(&mut capture);
        let native = native("native", true, NativeAgentState::Running);
        capture.native_agents.insert(native.agent.view_id(), native);
        project(&mut capture, &Default::default(), None, None);
        assert_eq!(capture.verdicts["parent"].interrupt_count, 3);

        let before = capture.clone();
        let mut operation =
            super::super::tests::retained_startup_test_move(&capture.records["parent"]);
        operation.phase = mj_core::state::MovePhase::Failed;
        capture.moves.insert("parent".into(), operation);
        project(&mut capture, &before, None, None);
        let parent = capture.verdicts["parent"];
        assert!(!parent.interrupt_member);
        assert!(parent.unfinished);
        assert!(parent.interrupt_all);
        assert_eq!(parent.interrupt_count, 1);

        let (outcome, commands) = tokio::join!(
            runtime.interrupt_all_in_capture(&capture, "parent"),
            manager.answer_submissions(Vec::new()),
        );
        let result = outcome.unwrap();
        assert_eq!(result.requested, 1);
        assert!(result.failures.is_empty());
        assert_eq!(commands.len(), 1);
        assert_eq!(commands[0].0, "child");
        assert!(manager.stopped_tasks.is_empty());

        for activity in [
            mj_core::activity::ActivityState::Expecting { since_ms: 1 },
            mj_core::activity::ActivityState::Goal,
        ] {
            let before = capture.clone();
            let live = capture
                .sessions
                .get_mut("child")
                .unwrap()
                .operational
                .as_mut()
                .unwrap();
            live.harness_turn = None;
            live.active_agent_terminals.clear();
            live.activity = Some(activity);
            project(&mut capture, &before, None, None);
            assert!(!capture.verdicts["child"].unfinished);
            // Only the native child still counts toward lifecycle warnings.
            assert_eq!(capture.verdicts["parent"].unfinished_children, 1);

            let before = capture.clone();
            capture
                .sessions
                .get_mut("child")
                .unwrap()
                .operational
                .as_mut()
                .unwrap()
                .active_agent_terminals
                .push(mj_core::relay::ActiveAgentTerminal {
                    terminal_id: "terminal".into(),
                    command: "sleep 10".into(),
                    started_at_ms: 1,
                });
            project(&mut capture, &before, None, None);
            assert!(capture.verdicts["child"].active_work);
            assert_eq!(capture.verdicts["parent"].unfinished_children, 2);
        }
    }

    fn insert_active_prompt_session(
        capture: &mut RuntimeProjection,
        id: &str,
        prompt_id: &str,
        turn_controls: mj_core::relay::TurnControls,
    ) {
        capture.records.insert(
            id.into(),
            super::super::tests::runtime_test_session(id, "default", SessionState::Running),
        );
        let mut live = mj_core::relay::RelaySnapshot::new(id.into()).operational_state();
        live.active_prompt = Some(mj_core::relay::ActiveRelayPrompt {
            command_id: prompt_id.into(),
            created_at_ms: 1,
            started_at_ms: 1,
        });
        live.turn_controls = Some(turn_controls);
        let mut view = RuntimeSessionView::from_managed(id.into(), Default::default());
        view.operational = Some(live.into());
        capture.sessions.insert(id.into(), view);
    }

    fn insert_subagent_link(capture: &mut RuntimeProjection, parent: &str, child: &str) {
        capture.subagents.insert(
            child.into(),
            mj_core::subagent::SubagentMetadata {
                kind: SubagentKind::Worker,
                child_session_id: child.into(),
                parent_session_id: parent.into(),
                task_name: String::new(),
                profile_id: "default".into(),
                model: None,
                effort: None,
                working_directory: Default::default(),
                request_key: String::new(),
                created_at: String::new(),
                noticed_turn: None,
                reported_finish: None,
                handback_tool: false,
                supervisor: false,
            },
        );
    }

    // Hard-won: #1624: Pending turn_controls with interrupt: None fell back to raw CancelTurn.
    #[tokio::test]
    async fn pending_cancellation_with_active_prompt_never_falls_back_to_raw_cancel() {
        let mut manager = super::super::tests::TestRemoteManager::for_session("child").await;
        let runtime = super::super::tests::test_runtime_state_with_manager(&manager);
        let mut capture = RuntimeProjection::default();
        insert_active_prompt_session(
            &mut capture,
            "child",
            "p1",
            mj_core::relay::TurnControls {
                status: mj_core::relay::TurnControlStatus::Pending,
                primary: None,
                interrupt: None,
                message: Some("Interrupting…".into()),
            },
        );
        project(&mut capture, &Default::default(), None, None);
        assert!(!capture.verdicts["child"].interrupt);
        assert!(!capture.verdicts["child"].interrupt_member);
        assert!(!capture.verdicts["child"].interrupt_all);
        assert_eq!(capture.verdicts["child"].interrupt_count, 0);

        let (outcome, commands) = tokio::join!(
            runtime.submit_turn_interrupt(&capture, "child", false),
            manager.answer_submissions(Vec::new()),
        );
        assert!(outcome.is_err());
        assert!(commands.is_empty());

        // Steering also sets status: Pending, but publishes an explicit
        // CancelTurnFor interrupt command that must remain targetable.
        let before = capture.clone();
        let exact = RelayCommand::CancelTurnFor {
            active_prompt_id: "p1".into(),
        };
        capture
            .sessions
            .get_mut("child")
            .unwrap()
            .operational
            .as_mut()
            .unwrap()
            .turn_controls = Some(mj_core::relay::TurnControls {
            status: mj_core::relay::TurnControlStatus::Pending,
            primary: None,
            interrupt: Some(exact.clone()),
            message: Some("Steering…".into()),
        });
        project(&mut capture, &before, None, None);
        assert!(capture.verdicts["child"].interrupt);
        assert!(capture.verdicts["child"].interrupt_member);
        let (outcome, commands) = tokio::join!(
            runtime.submit_turn_interrupt(&capture, "child", false),
            manager.answer_submissions(Vec::new()),
        );
        outcome.unwrap();
        assert_eq!(commands.len(), 1);
        assert_eq!(commands[0].2, exact);
    }

    // Hard-won: #1624: overlapping Interrupt All from different roots sent duplicate cancels to a shared child.
    #[tokio::test]
    async fn overlapping_interrupt_all_from_different_roots_deduplicates_shared_child() {
        let mut manager = super::super::tests::TestRemoteManager::for_sessions(&[
            "grandparent",
            "parent",
            "child",
        ])
        .await;
        let runtime = super::super::tests::test_runtime_state_with_manager(&manager);
        let mut capture = RuntimeProjection::default();
        for id in ["grandparent", "parent"] {
            capture.records.insert(
                id.into(),
                super::super::tests::runtime_test_session(id, "default", SessionState::Running),
            );
            let live = mj_core::relay::RelaySnapshot::new(id.into()).operational_state();
            let mut view = RuntimeSessionView::from_managed(id.into(), Default::default());
            view.operational = Some(live.into());
            capture.sessions.insert(id.into(), view);
        }
        let cancel_p1 = RelayCommand::CancelTurnFor {
            active_prompt_id: "p1".into(),
        };
        insert_active_prompt_session(
            &mut capture,
            "child",
            "p1",
            mj_core::relay::TurnControls {
                status: mj_core::relay::TurnControlStatus::Ready,
                primary: None,
                interrupt: Some(cancel_p1.clone()),
                message: None,
            },
        );
        insert_subagent_link(&mut capture, "grandparent", "parent");
        insert_subagent_link(&mut capture, "parent", "child");
        project(&mut capture, &Default::default(), None, None);
        assert_eq!(capture.verdicts["grandparent"].interrupt_count, 1);
        assert_eq!(capture.verdicts["parent"].interrupt_count, 1);

        // Concurrent in-flight overlap from two different roots.
        let (gp_result, parent_result, commands) = tokio::join!(
            runtime.interrupt_all_in_capture(&capture, "grandparent"),
            runtime.interrupt_all_in_capture(&capture, "parent"),
            manager.answer_submissions(Vec::new()),
        );
        let gp_result = gp_result.unwrap();
        let parent_result = parent_result.unwrap();
        assert_eq!(gp_result.requested, 1);
        assert_eq!(parent_result.requested, 1);
        assert!(gp_result.failures.is_empty());
        assert!(parent_result.failures.is_empty());
        assert_eq!(commands.len(), 1);
        assert_eq!(commands[0].0, "child");
        assert_eq!(commands[0].2, cancel_p1);

        // Back-to-back call against the same stale pre-update projection is also deduplicated.
        let (repeat_result, repeat_commands) = tokio::join!(
            runtime.interrupt_all_in_capture(&capture, "parent"),
            manager.answer_submissions(Vec::new()),
        );
        let repeat_result = repeat_result.unwrap();
        assert_eq!(repeat_result.requested, 1);
        assert!(repeat_result.failures.is_empty());
        assert!(repeat_commands.is_empty());

        // Once the child advances to a new turn (p2), a fresh Interrupt All is admitted.
        let before = capture.clone();
        let cancel_p2 = RelayCommand::CancelTurnFor {
            active_prompt_id: "p2".into(),
        };
        let child_live = capture
            .sessions
            .get_mut("child")
            .unwrap()
            .operational
            .as_mut()
            .unwrap();
        child_live.active_prompt = Some(mj_core::relay::ActiveRelayPrompt {
            command_id: "p2".into(),
            created_at_ms: 2,
            started_at_ms: 2,
        });
        child_live.turn_controls = Some(mj_core::relay::TurnControls {
            status: mj_core::relay::TurnControlStatus::Ready,
            primary: None,
            interrupt: Some(cancel_p2.clone()),
            message: None,
        });
        project(&mut capture, &before, None, None);
        let (p2_result, p2_commands) = tokio::join!(
            runtime.interrupt_all_in_capture(&capture, "parent"),
            manager.answer_submissions(Vec::new()),
        );
        let p2_result = p2_result.unwrap();
        assert_eq!(p2_result.requested, 1);
        assert!(p2_result.failures.is_empty());
        assert_eq!(p2_commands.len(), 1);
        assert_eq!(p2_commands[0].2, cancel_p2);
    }

    // Hard-won: #1624: overlapping Interrupt All during p1 cancellation must leave queued p2 running.
    #[tokio::test]
    async fn overlapping_interrupt_all_leaves_child_queued_prompt_running() {
        let mut manager = super::super::tests::TestRemoteManager::for_sessions(&[
            "grandparent",
            "parent",
            "child",
        ])
        .await;
        let runtime = super::super::tests::test_runtime_state_with_manager(&manager);
        let mut capture = RuntimeProjection::default();
        for id in ["grandparent", "parent"] {
            capture.records.insert(
                id.into(),
                super::super::tests::runtime_test_session(id, "default", SessionState::Running),
            );
            let live = mj_core::relay::RelaySnapshot::new(id.into()).operational_state();
            let mut view = RuntimeSessionView::from_managed(id.into(), Default::default());
            view.operational = Some(live.into());
            capture.sessions.insert(id.into(), view);
        }
        let cancel_p1 = RelayCommand::CancelTurnFor {
            active_prompt_id: "p1".into(),
        };
        insert_active_prompt_session(
            &mut capture,
            "child",
            "p1",
            mj_core::relay::TurnControls {
                status: mj_core::relay::TurnControlStatus::Ready,
                primary: None,
                interrupt: Some(cancel_p1.clone()),
                message: None,
            },
        );
        insert_subagent_link(&mut capture, "grandparent", "parent");
        insert_subagent_link(&mut capture, "parent", "child");
        project(&mut capture, &Default::default(), None, None);

        // First Interrupt All from grandparent cancels p1 with CancelTurnFor("p1").
        let (gp_result, commands) = tokio::join!(
            runtime.interrupt_all_in_capture(&capture, "grandparent"),
            manager.answer_submissions(Vec::new()),
        );
        assert!(gp_result.unwrap().failures.is_empty());
        assert_eq!(commands.len(), 1);
        assert_eq!(commands[0].2, cancel_p1);

        // Worker marks p1 cancellation pending (interrupt: None) before p1 finishes
        // and queued p2 starts. A second Interrupt All from parent must NOT emit raw CancelTurn.
        let before = capture.clone();
        capture
            .sessions
            .get_mut("child")
            .unwrap()
            .operational
            .as_mut()
            .unwrap()
            .turn_controls = Some(mj_core::relay::TurnControls {
            status: mj_core::relay::TurnControlStatus::Pending,
            primary: None,
            interrupt: None,
            message: Some("Interrupting…".into()),
        });
        project(&mut capture, &before, None, None);
        let (parent_result, second_commands) = tokio::join!(
            runtime.interrupt_all_in_capture(&capture, "parent"),
            manager.answer_submissions(Vec::new()),
        );
        let parent_result = parent_result.unwrap();
        assert_eq!(parent_result.requested, 0);
        assert!(parent_result.failures.is_empty());
        assert!(second_commands.is_empty());
    }

    // Protects overlapping callers from receiving a false success when the
    // leading in-flight interrupt submission fails.
    #[tokio::test]
    async fn overlapping_interrupt_all_propagates_in_flight_failure_to_duplicate_callers() {
        let mut manager = super::super::tests::TestRemoteManager::for_sessions(&[
            "grandparent",
            "parent",
            "child",
        ])
        .await;
        let runtime = super::super::tests::test_runtime_state_with_manager(&manager);
        let mut capture = RuntimeProjection::default();
        for id in ["grandparent", "parent"] {
            capture.records.insert(
                id.into(),
                super::super::tests::runtime_test_session(id, "default", SessionState::Running),
            );
            let live = mj_core::relay::RelaySnapshot::new(id.into()).operational_state();
            let mut view = RuntimeSessionView::from_managed(id.into(), Default::default());
            view.operational = Some(live.into());
            capture.sessions.insert(id.into(), view);
        }
        let cancel_p1 = RelayCommand::CancelTurnFor {
            active_prompt_id: "p1".into(),
        };
        insert_active_prompt_session(
            &mut capture,
            "child",
            "p1",
            mj_core::relay::TurnControls {
                status: mj_core::relay::TurnControlStatus::Ready,
                primary: None,
                interrupt: Some(cancel_p1.clone()),
                message: None,
            },
        );
        insert_subagent_link(&mut capture, "grandparent", "parent");
        insert_subagent_link(&mut capture, "parent", "child");
        project(&mut capture, &Default::default(), None, None);

        let (gp_result, parent_result, commands) = tokio::join!(
            runtime.interrupt_all_in_capture(&capture, "grandparent"),
            runtime.interrupt_all_in_capture(&capture, "parent"),
            manager.answer_submissions(vec![mj_client::session::SubmitFailure::from(
                "worker disconnected".to_owned(),
            )]),
        );
        let gp_result = gp_result.unwrap();
        let parent_result = parent_result.unwrap();
        assert_eq!(commands.len(), 1);
        assert_eq!(gp_result.failures.len(), 1);
        assert!(gp_result.failures[0].1.contains("worker disconnected"));
        assert_eq!(parent_result.failures.len(), 1);
        assert!(parent_result.failures[0].1.contains("worker disconnected"));

        // After a failed attempt, a retry submits fresh and succeeds.
        let (retry_result, retry_commands) = tokio::join!(
            runtime.interrupt_all_in_capture(&capture, "parent"),
            manager.answer_submissions(Vec::new()),
        );
        assert!(retry_result.unwrap().failures.is_empty());
        assert_eq!(retry_commands.len(), 1);
        assert_eq!(retry_commands[0].2, cancel_p1);

        // A delayed interrupt captured for an earlier turn (p1) must not evict
        // a newer turn's (p2) admission and let p2 be cancelled twice.
        let stale_p1_capture = capture.clone();
        let before = capture.clone();
        let cancel_p2 = RelayCommand::CancelTurnFor {
            active_prompt_id: "p2".into(),
        };
        let child_live = capture
            .sessions
            .get_mut("child")
            .unwrap()
            .operational
            .as_mut()
            .unwrap();
        child_live.active_prompt = Some(mj_core::relay::ActiveRelayPrompt {
            command_id: "p2".into(),
            created_at_ms: 2,
            started_at_ms: 2,
        });
        child_live.turn_controls = Some(mj_core::relay::TurnControls {
            status: mj_core::relay::TurnControlStatus::Ready,
            primary: None,
            interrupt: Some(cancel_p2.clone()),
            message: None,
        });
        project(&mut capture, &before, None, None);
        let (p2_first, p2_first_commands) = tokio::join!(
            runtime.interrupt_all_in_capture(&capture, "parent"),
            manager.answer_submissions(Vec::new()),
        );
        assert!(p2_first.unwrap().failures.is_empty());
        assert_eq!(p2_first_commands.len(), 1);
        // Delayed p1 snapshot is already admitted and must not disturb p2.
        let (delayed_p1, delayed_commands) = tokio::join!(
            runtime.interrupt_all_in_capture(&stale_p1_capture, "grandparent"),
            manager.answer_submissions(Vec::new()),
        );
        assert!(delayed_p1.unwrap().failures.is_empty());
        assert!(delayed_commands.is_empty());
        // Second p2 call remains deduplicated.
        let (p2_second, p2_second_commands) = tokio::join!(
            runtime.interrupt_all_in_capture(&capture, "grandparent"),
            manager.answer_submissions(Vec::new()),
        );
        assert!(p2_second.unwrap().failures.is_empty());
        assert!(p2_second_commands.is_empty());
    }

    #[test]
    fn held_subagent_waiting_for_background_processes_counts_as_working() {
        let mut capture = RuntimeProjection::default();
        insert_running_session(&mut capture, "parent");
        {
            let view = capture.sessions.get_mut("parent").unwrap();
            view.connected = true;
            let live = view.operational.as_mut().unwrap();
            live.harness_turn = None;
            live.work_liveness = Some(mj_core::work_facts::WorkLiveness {
                completed_ordinal: 12,
                alive: 1,
                checked_at_ms: 100,
            });
        }
        insert_managed_child(&mut capture);
        capture.subagents.get_mut("child").unwrap().handback_tool = true;
        {
            let view = capture.sessions.get_mut("child").unwrap();
            view.connected = true;
            let live = view.operational.as_mut().unwrap();
            live.harness_turn = None;
            live.work_liveness = Some(mj_core::work_facts::WorkLiveness {
                completed_ordinal: 12,
                alive: 1,
                checked_at_ms: 100,
            });
        }

        let reminder_id = mj_core::subagent::handback_reminder_command_id(7);
        let turn = mj_core::state::MaterializedTurnOutcome {
            work_facts: None,
            answered_through: None,
            diagnostic: None,
            usage: None,
            command_id: reminder_id.clone(),
            accepted_ordinal: Some(11),
            turn_start_position: Some(11),
            completed_ordinal: 12,
            completed_at_ms: mj_core::clock::epoch_millis(),
            outcome: mj_core::state::TurnOutcomeKind::Completed {
                stop_reason: "end_turn".into(),
            },
        };
        let mut committed = mj_controller::database::CommittedState {
            sequence: 1,
            state: Default::default(),
            moves: Default::default(),
            native_agents: Default::default(),
            startup_groups: Default::default(),
            subagent_reports: Default::default(),
            turns: Default::default(),
            wait_revisions: Default::default(),
        };
        committed.turns.insert_shared(
            "child".into(),
            mj_controller::database::CommittedTurn {
                state: (
                    mj_core::state::MaterializedExecutionState::Idle,
                    None,
                    Some(turn),
                ),
                pending_form_times: Default::default(),
                failed_message: None,
            },
        );

        project(&mut capture, &Default::default(), Some(&committed), None);
        assert!(
            !capture.verdicts["parent"].working,
            "a top-level session with leftover work_liveness is not held"
        );
        assert!(capture.verdicts["child"].working);
        assert_eq!(capture.verdicts["parent"].working_children, 1);

        // Delivering a handback ends the hold even if a background process remains alive.
        let before = capture.clone();
        let prev_committed = committed.clone();
        committed.subagent_reports.insert_shared(
            "child".into(),
            mj_core::subagent::SubagentReport {
                handback: Some(mj_core::subagent::SubagentHandback {
                    command_id: reminder_id,
                    message: "done".into(),
                    recorded_at_ms: mj_core::clock::epoch_millis(),
                }),
                ..Default::default()
            },
        );
        project(
            &mut capture,
            &before,
            Some(&committed),
            Some(&prev_committed),
        );
        assert!(!capture.verdicts["child"].working);
        assert_eq!(capture.verdicts["parent"].working_children, 0);

        // Re-entering held state and then clearing work_liveness drops the count back to 0.
        let before = capture.clone();
        let prev_committed = committed.clone();
        committed
            .subagent_reports
            .insert_shared("child".into(), mj_core::subagent::SubagentReport::default());
        project(
            &mut capture,
            &before,
            Some(&committed),
            Some(&prev_committed),
        );
        assert!(capture.verdicts["child"].working);
        assert_eq!(capture.verdicts["parent"].working_children, 1);

        let before = capture.clone();
        capture
            .sessions
            .get_mut("child")
            .unwrap()
            .operational
            .as_mut()
            .unwrap()
            .work_liveness = None;
        project(&mut capture, &before, Some(&committed), Some(&committed));
        assert!(!capture.verdicts["child"].working);
        assert_eq!(capture.verdicts["parent"].working_children, 0);
    }
}
