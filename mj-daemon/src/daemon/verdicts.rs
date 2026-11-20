use super::*;
use mj_client::runtime_feed::{RuntimeProjection, SessionOffer, SessionVerdict};
use mj_core::attention::SessionAttention;
use mj_core::native_agent::NativeAgentState;
use mj_core::state::SessionTransitionKind;
use mj_core::subagent::SubagentKind;

type Children<'a> = BTreeMap<String, Vec<(&'a str, Option<SubagentKind>)>>;

fn children(runtime: &RuntimeProjection) -> Children<'_> {
    let mut children = Children::new();
    for (id, child) in &runtime.subagents {
        if runtime
            .records
            .get(id)
            .is_some_and(|r| r.state.has_live_worker() && !r.archived)
        {
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
        if verdict(id).is_some_and(|v| v.interrupt_member) {
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
        RuntimeLifecycleKind::Suspend | RuntimeLifecycleKind::ForceStop => Suspending,
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

fn own_verdict(
    runtime: &RuntimeProjection,
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
    } else {
        Ready
    };
    let state = record.state;
    let view = runtime.sessions.get(id);
    let live = view.and_then(|v| v.operational.as_deref());
    let form = view
        .and_then(|v| v.materialized.as_deref())
        .is_some_and(|v| !v.pending_elicitations.is_empty());
    let attention = runtime.metadata.session_attention.get(id);
    let activity = live.map(|v| v.activity_state());
    let running = live.is_some_and(|live| {
        live.active_prompt.is_some()
            || live.harness_turn.is_some() && !live.goal.active()
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
    let unfinished = state.has_live_worker()
        && live.is_some()
        && (running || form || super::attention::semantic_question(runtime, id).is_some());
    let failed_first_launch = runtime.metadata.failed_launch_retries.contains(id);
    let resume = if operation != Ready {
        operation
    } else if state.is_active() && state != SessionState::Error {
        Active
    } else if move_op.is_some_and(|m| m.holds_source_environment()) {
        MoveEnvironmentRetained
    } else if !matches!(
        state,
        SessionState::Lost | SessionState::DestroyedWithDataLoss
    ) && (record.checkpoint.is_some()
        || (state == SessionState::Error && record.target.is_some())
        || failed_first_launch)
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
                || state == SessionState::Running
                    && activity.as_ref().is_some_and(|a| a.is_idle())),
        working: state == SessionState::Running
            && view.is_some_and(|v| v.connected)
            && live.is_some_and(|live| {
                mj_client::usage_format::SessionActivity::of(live).is_working(None, form)
            }),
        unfinished,
        interrupt: running && !form && state.has_live_worker(),
        interrupt_member: unfinished,
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
pub(crate) fn project(runtime: &mut RuntimeProjection, previous: &RuntimeProjection) {
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
        match verdict {
            Some(verdict) => {
                verdicts.insert(id.to_owned(), verdict);
            }
            None => {
                verdicts.remove(id);
            }
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
            interrupt_targets(&id, &children, |id| own.get(id).copied()).len();
        verdict.interrupt_all = verdict.interrupt_count != 0;
        if previous.verdicts.get(&id) == Some(&verdict) {
            verdicts.copy_entry(&previous.verdicts, &id, true);
        } else {
            verdicts.insert(id, verdict);
        }
    }
    runtime.verdicts = verdicts;
}

impl RuntimeState {
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
        let command = include_question
            .then(|| super::attention::semantic_question(runtime, id))
            .flatten()
            .map(|question| RelayCommand::DismissQuestion { question })
            .or_else(|| {
                runtime
                    .sessions
                    .get(id)?
                    .operational
                    .as_ref()?
                    .turn_controls
                    .as_ref()?
                    .interrupt
                    .clone()
            })
            .unwrap_or(RelayCommand::CancelTurn);
        self.session_manager
            .session(id)
            .await?
            .submit(new_command_id("interrupt-turn")?, command)
            .await?;
        Ok(())
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
        let targets = interrupt_targets(root, &children, |id| runtime.verdicts.get(id).copied());
        let mut failures = Vec::new();
        for id in &targets {
            let result = async {
                if let Some(native) = runtime.native_agents.get(*id) {
                    let agent = &native.agent;
                    self.session_manager
                        .session(&agent.owner_session_id)
                        .await?
                        .stop_background_task(format!("native-agent:{}", agent.session_id))
                        .await?;
                } else {
                    self.submit_turn_interrupt(runtime, id, true).await?;
                }
                Ok::<_, anyhow::Error>(())
            }
            .await;
            if let Err(error) = result {
                failures.push(((*id).to_owned(), format!("{error:#}")));
            }
        }
        Ok(InterruptAllResult {
            requested: targets.len(),
            failures,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mj_core::native_agent::{NativeAgent, NativeAgentCapabilities, NativeAgentSummary};

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
        let native = |id: &str, cancellable, state| NativeAgentSummary {
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
        project(&mut capture, &Default::default());
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
        project(&mut capture, &before);
        assert!(!capture.verdicts["parent"].interrupt_all);
    }
}
