//! Durable completed-turn assessment and process-local activity inference.
use super::*;
use mj_core::activity::ActivityFacts;
use mj_core::activity::verdict::{Decision, TurnEvidence, TurnPhase};
use std::sync::Mutex;

type PendingRepliedVerdict = (u64, TurnEvidence, Option<(String, u64)>);

#[derive(Default)]
pub(super) struct RepliedVerdictState {
    last_generation: Option<u64>,
    inference: Mutex<Option<(u64, Decision, i64)>>,
    /// Jev's confident answer to whether the listed background commands are
    /// still needed, with the evidence generation it was judged at.
    background: Mutex<Option<(u64, bool)>>,
}

fn blocked(facts: &ActivityFacts) -> Option<&'static str> {
    if matches!(
        facts.execution,
        RelayExecutionState::Closing | RelayExecutionState::Closed
    ) {
        Some("session_closed")
    } else if facts.prompt_started_at_ms.is_some() || facts.harness_turn_started_at_ms.is_some() {
        Some("foreground_turn")
    } else if !facts.tools_in_flight.is_empty()
        || (facts.execution == RelayExecutionState::Running
            && facts.current_step_started_at_ms.is_some())
    {
        Some("foreground_tool")
    } else if facts.goal_running {
        Some("running_goal")
    } else if facts.queued_commands > 0 {
        Some("queued_prompt")
    } else {
        None
    }
}

impl RepliedVerdictState {
    /// `(expected_continuation, inferred_idle_since_ms, background_needed)`
    /// for the current generation, or nothing once the evidence has moved on.
    pub(super) fn inference(
        &self,
        generation: u64,
        facts: &ActivityFacts,
    ) -> (Option<i64>, Option<i64>, Option<bool>) {
        let mut inference = self
            .inference
            .lock()
            .expect("verdict inference lock poisoned");
        let mut background = self
            .background
            .lock()
            .expect("verdict background lock poisoned");
        let stale = blocked(facts).is_some();
        if inference
            .as_ref()
            .is_some_and(|(accepted, _, _)| *accepted != generation)
            || stale
        {
            *inference = None;
        }
        if background
            .as_ref()
            .is_some_and(|(accepted, _)| *accepted != generation)
            || stale
        {
            *background = None;
        }
        let needed = background.map(|(_, needed)| needed);
        match *inference {
            Some((_, Decision::InferIdle | Decision::AwaitingInput, since)) => {
                (None, Some(since), needed)
            }
            Some((_, Decision::ExpectContinuation, since)) => (Some(since), None, needed),
            _ => (None, None, needed),
        }
    }

    fn judge_background(&self, generation: u64, needed: Option<bool>) {
        *self
            .background
            .lock()
            .expect("verdict background lock poisoned") = needed.map(|needed| (generation, needed));
    }
}

impl DurableRelay {
    pub(super) fn publish_assessment_diagnostic(&self) {
        if let Some(a) = &self.snapshot.assessment
            && let Some(log) = self.turn_context.decision_log()
        {
            log.record_assessment(&self.snapshot.session_id, a);
        }
    }

    /// Capture immutable semantic evidence before issuing a request. Completion
    /// itself already installed the pending record, so restart can finish this.
    ///
    /// The Unix worker is the only production caller; these assessment
    /// operations stay compiled on Windows so their tests still build there.
    #[cfg_attr(not(unix), allow(dead_code))]
    pub(crate) fn prepare_pending_assessment(&mut self) -> Result<()> {
        if let Some(mut a) = self.snapshot.assessment.clone().filter(|a| {
            a.current()
                && matches!(
                    a.action,
                    Some(
                        mj_core::assessment::Action::Wait | mj_core::assessment::Action::Uncertain
                    )
                )
        }) {
            let facts = self.activity_facts();
            if a.evidence.as_ref().is_some_and(|e| {
                e.background_commands != facts.background_commands
                    || e.queued_commands != facts.queued_commands
            }) {
                a.revision = self.snapshot.latest_ordinal + 1;
                a.status = mj_core::assessment::Status::Pending;
                a.evidence = None;
                a.action = None;
                a.verdict = None;
                self.store_assessment(a)?;
            }
        }
        let Some(mut a) = self
            .snapshot
            .assessment
            .clone()
            .filter(|a| a.needs_classification(epoch_millis()) && a.evidence.is_none())
        else {
            return Ok(());
        };
        let Some(harness) = self.verdict_harness else {
            return Ok(());
        };
        let mut evidence = self.turn_context.evidence(
            harness,
            TurnPhase::Replied,
            &self.activity_facts(),
            epoch_millis(),
        );
        evidence.completion = Some(a.completion.clone());
        evidence.authorization = self.snapshot.assessment_context.clone();
        let now = epoch_millis();
        evidence.background = self
            .background_commands()
            .into_iter()
            .take(mj_core::activity::verdict::IN_FLIGHT_TOOLS)
            .map(|command| {
                let mut text = command.command;
                text.truncate(
                    text.floor_char_boundary(mj_core::activity::verdict::TOOL_TITLE_BYTES),
                );
                mj_core::activity::verdict::BackgroundEvidence {
                    id: command.id,
                    command: text,
                    started_s_ago: now.saturating_sub(command.started_at_ms).max(0) as u64 / 1000,
                }
            })
            .collect();
        if let Some(context) = &evidence.authorization
            && !context.final_reply_omitted
            && let Some(last) = context
                .messages
                .iter()
                .rev()
                .find(|m| m.role == "assistant")
        {
            evidence.assistant_text_tail = last.text.clone();
            evidence
                .assistant_text_tail
                .truncate(evidence.assistant_text_tail.floor_char_boundary(2048));
        }
        // Authorization already contains whole messages; don't send a second,
        // differently clipped interpretation of that same conversation.
        if evidence.authorization.is_some() {
            evidence.transcript_summary.clear();
        }
        if serde_json::to_vec(&evidence)?.len() > 60 * 1024 {
            evidence.authorization = None;
            evidence.transcript_summary.clear();
        }
        a.evidence = Some(evidence);
        self.store_assessment(a)
    }

    #[cfg_attr(not(unix), allow(dead_code))]
    fn store_assessment(&mut self, assessment: mj_core::assessment::TurnAssessment) -> Result<()> {
        self.append_relay_event(
            None,
            RelayObservation::TurnAssessmentUpdated {
                assessment: Box::new(assessment),
            },
        )?;
        Ok(())
    }

    #[cfg_attr(not(unix), allow(dead_code))]
    pub(crate) fn apply_turn_assessment(
        &mut self,
        ordinal: u64,
        verdict: mj_core::assessment::Verdict,
    ) -> Result<&'static str> {
        use mj_core::assessment::{Action, Status};
        let Some(mut a) = self
            .snapshot
            .assessment
            .clone()
            .filter(|a| a.revision == ordinal && a.current())
        else {
            return Ok("stale_turn");
        };
        let complete = a
            .evidence
            .as_ref()
            .and_then(|e| e.authorization.as_ref())
            .is_some_and(|c| {
                c.authorization_complete
                    && !c.final_reply_omitted
                    && c.evidence().validate().is_ok()
            });
        let action = verdict.action(complete);
        // The quiet judgment is separate from the action: it only ever says
        // whether the listed leftover processes still matter, and only when
        // the request listed some.
        let listed = a
            .evidence
            .as_ref()
            .is_some_and(|e| !e.background.is_empty());
        let needed = verdict
            .background
            .filter(|judgment| {
                listed && judgment.confidence >= mj_core::activity::verdict::ACT_CONFIDENCE
            })
            .and_then(|judgment| match judgment.choice {
                mj_core::assessment::Background::Needed => Some(true),
                mj_core::assessment::Background::Unneeded => Some(false),
                mj_core::assessment::Background::Unclear => None,
            });
        self.replied_verdict
            .judge_background(self.turn_context.generation(), needed);
        a.verdict = Some(verdict);
        a.action = Some(action);
        a.status = Status::Assessed;
        a.retry_at_ms = None;
        let suppressed = !self.snapshot.assessment_questions.is_empty()
            || self.snapshot.goal.budget_limited()
            || self
                .snapshot
                .goal
                .snapshot
                .as_ref()
                .is_some_and(|g| g.status == "paused")
            || !self.snapshot.queued_prompts.is_empty();
        let reason = if suppressed {
            a.status = Status::Superseded;
            "automatic_action_suppressed"
        } else {
            match action {
                Action::RetryProvider => {
                    a.status = Status::Scheduled;
                    "server_retry_armed"
                }
                Action::RecoverQuota => {
                    a.status = Status::Deferred;
                    "quota_resolution_required"
                }
                Action::Continue if self.snapshot.continuation.eligible() => {
                    a.status = Status::Deferred;
                    "authorized_continuation_ready"
                }
                Action::Continue => "continuation_allowance_unavailable",
                Action::AwaitInput => "user_input_required",
                Action::Finished => "work_finished",
                Action::Wait => "background_work_pending",
                Action::Uncertain => "assessment_uncertain",
            }
        };
        a.reason = reason.into();
        self.store_assessment(a)?;
        if !suppressed {
            let decision = match action {
                Action::AwaitInput => Decision::AwaitingInput,
                Action::Finished => Decision::InferIdle,
                Action::Wait => Decision::ExpectContinuation,
                _ => Decision::KeepCurrent,
            };
            // Semantic completion survives; runtime inference only applies to
            // current quiet facts, and never grants resource-replacement safety.
            self.apply_replied_decision(self.turn_context.generation(), decision, epoch_millis())?;
        }
        Ok(reason)
    }

    #[cfg_attr(not(unix), allow(dead_code))]
    pub(crate) fn fail_turn_assessment(&mut self, ordinal: u64, reason: &str) -> Result<()> {
        let Some(mut a) = self
            .snapshot
            .assessment
            .clone()
            .filter(|a| a.revision == ordinal && a.current())
        else {
            return Ok(());
        };
        a.status = if reason == "classifier_unavailable" {
            mj_core::assessment::Status::Assessed
        } else {
            mj_core::assessment::Status::Failed
        };
        a.failures = a.failures.saturating_add(1);
        a.retry_at_ms = Some(epoch_millis().saturating_add(
            (60_000_i64 * (1_i64 << a.failures.saturating_sub(1).min(3))).min(300_000),
        ));
        a.reason = reason.into();
        self.replied_verdict.last_generation = None;
        self.store_assessment(a)
    }

    pub fn pending_replied_verdict(&mut self) -> Option<PendingRepliedVerdict> {
        let a = self.snapshot.assessment.as_ref()?;
        if !a.needs_classification(epoch_millis())
            || self.replied_verdict.last_generation == Some(a.revision)
        {
            return None;
        }
        let evidence = a.evidence.clone()?;
        self.replied_verdict.last_generation = Some(a.revision);
        Some((
            a.revision,
            evidence,
            Some((a.turn_id.clone(), a.completed_ordinal)),
        ))
    }

    #[cfg(test)]
    pub(crate) fn resolve_retry_assessment(
        &mut self,
        identity: Option<(String, u64)>,
        retryable: bool,
    ) -> Result<bool> {
        let Some((id, ordinal)) = identity else {
            return Ok(false);
        };
        let Some(mut a) = self
            .snapshot
            .assessment
            .clone()
            .filter(|a| a.turn_id == id && a.completed_ordinal == ordinal && a.current())
        else {
            return Ok(false);
        };
        a.status = if retryable {
            mj_core::assessment::Status::Scheduled
        } else {
            mj_core::assessment::Status::Assessed
        };
        a.action = Some(if retryable {
            mj_core::assessment::Action::RetryProvider
        } else {
            mj_core::assessment::Action::Uncertain
        });
        self.store_assessment(a)?;
        Ok(retryable)
    }

    #[cfg(test)]
    pub(crate) fn retry_assessment_identity(&self) -> Option<(String, u64)> {
        self.snapshot
            .assessment
            .as_ref()
            .filter(|a| a.current())
            .map(|a| (a.turn_id.clone(), a.completed_ordinal))
    }

    #[cfg(any(unix, test))]
    pub(crate) fn replied_verdict_is_current(&self, generation: u64) -> bool {
        self.snapshot
            .assessment
            .as_ref()
            .is_some_and(|a| a.current() && a.revision == generation)
    }

    pub(crate) fn apply_replied_decision(
        &mut self,
        generation: u64,
        decision: Decision,
        since_ms: i64,
    ) -> Result<&'static str> {
        let facts = self.activity_facts();
        if generation != self.turn_context.generation() {
            return Ok("stale_generation");
        }
        if let Some(reason) = blocked(&facts) {
            return Ok(reason);
        }
        if decision == Decision::KeepCurrent {
            return Ok("keep_current");
        }
        // Commit the turn's decision before publishing activity. The worker owns
        // both under the relay lock; readers cannot see a half-applied verdict.
        if let Some(completion) = self.snapshot.turn_completion.as_ref() {
            let mut next = self.snapshot.clone();
            next.turn_completion = Some(mj_core::activity::verdict::TurnCompletion {
                command_id: completion.command_id.clone(),
                completed_ordinal: self.snapshot.latest_ordinal,
                decision,
            });
            self.commit_snapshot(next)?;
        }
        *self
            .replied_verdict
            .inference
            .lock()
            .expect("verdict inference lock poisoned") = Some((generation, decision, since_ms));
        self.persist_activity_transition()?;
        tracing::info!(target: "mj_jev", session = %self.snapshot.session_id, generation,
            phase = "replied", ?decision,
            previous_activity = ?mj_core::activity::classify(&facts),
            activity = ?mj_core::activity::classify(&self.activity_facts()),
            "Jev activity inference applied");
        Ok("applied")
    }

    /// Test helper: apply an expected-continuation decision directly.
    #[cfg(test)]
    pub fn expect_continuation(
        &mut self,
        since_ms: i64,
        _note: String,
        generation: u64,
    ) -> Result<()> {
        self.apply_replied_decision(generation, Decision::ExpectContinuation, since_ms)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mj_core::activity::{ActivityState, safe_to_replace};
    use mj_core::config::HarnessKind;

    fn completed(root: &Path) -> DurableRelay {
        let mut relay = DurableRelay::open(root, "jev-test", "test").unwrap();
        relay.set_turn_verdict_harness(HarnessKind::Claude);
        relay.background_work = BackgroundWorkPolicy::ClaudeTasks;
        relay
            .turn_context
            .reset("How many tickets fell out of s21?");
        relay.record_session_update(serde_json::from_value(serde_json::json!({
            "sessionUpdate":"agent_message_chunk", "content":{"type":"text", "text":"One ticket so far: #3486."}
        })).unwrap()).unwrap();
        relay.claude_background_tasks_changed(tasks()).unwrap();
        for task in relay.claude_background_tasks.values_mut() {
            task.started_at_ms = 100;
        }
        relay.activity_facts();
        relay
    }

    fn tasks() -> Vec<crate::acp::ClaudeBackgroundTask> {
        (0..4)
            .map(|id| crate::acp::ClaudeBackgroundTask {
                task_id: id.to_string(),
                description: format!("Old provisioning wait {id}"),
            })
            .collect()
    }

    #[test]
    fn finished_reply_overrides_old_tasks_without_losing_inventory_or_safety() {
        let temp = tempfile::tempdir().unwrap();
        let mut relay = completed(temp.path());
        relay
            .claude_async_task_control_changed("0".into(), true, false)
            .unwrap();
        let before = relay.operational_state().background_commands;
        assert!(
            before
                .iter()
                .any(|task| task.id == "claude:0" && task.can_stop)
        );
        let generation = relay.turn_context.generation();
        let evidence = relay.turn_context.evidence(
            HarnessKind::Claude,
            TurnPhase::Replied,
            &relay.activity_facts(),
            200,
        );
        assert_eq!(evidence.background_commands, 4);
        assert_eq!(evidence.assistant_text_tail, "One ticket so far: #3486.");
        assert_eq!(
            relay
                .apply_replied_decision(generation, Decision::InferIdle, 200)
                .unwrap(),
            "applied"
        );
        let state = relay.operational_state();
        assert_eq!(
            state.activity_state(),
            ActivityState::Idle {
                since_ms: Some(200)
            }
        );
        assert_eq!(state.background_commands, before);
        assert_eq!(
            relay.background_task_stop_target("claude:0").unwrap(),
            BackgroundTaskStopTarget::ClaudeAsyncTask {
                task_id: "0".into()
            }
        );
        assert_eq!(state.facts(), relay.activity_facts());
        assert!(!safe_to_replace(&state.facts(), HarnessKind::Claude));
        assert!(!state.is_quiet());
        assert!(relay.pending_replied_verdict().is_none());
        drop(relay);
        let reopened = DurableRelay::open(temp.path(), "jev-test", "test").unwrap();
        assert_eq!(reopened.activity_facts().inferred_idle_since_ms, None);
    }

    #[test]
    fn identical_inventory_preserves_verdict_but_same_count_replacement_invalidates_it() {
        let temp = tempfile::tempdir().unwrap();
        let mut relay = completed(temp.path());
        let generation = relay.turn_context.generation();
        relay
            .apply_replied_decision(generation, Decision::InferIdle, 200)
            .unwrap();
        let mut unchanged = tasks();
        unchanged.reverse();
        relay.claude_background_tasks_changed(unchanged).unwrap();
        assert_eq!(relay.turn_context.generation(), generation);
        assert!(relay.operational_state().activity_state().is_idle());
        assert!(relay.pending_replied_verdict().is_none());
        // A repeated informational update is not resumed foreground work.
        relay
            .record_session_update(
                serde_json::from_value(serde_json::json!({
                    "sessionUpdate":"usage_update", "used":10, "size":100
                }))
                .unwrap(),
            )
            .unwrap();
        assert!(relay.operational_state().activity_state().is_idle());
        let mut replaced = tasks();
        replaced[0].task_id = "replacement".into();
        relay.claude_background_tasks_changed(replaced).unwrap();
        assert!(matches!(
            relay.operational_state().activity_state(),
            ActivityState::Background { .. }
        ));
        let new_generation = relay.turn_context.generation();
        assert_ne!(generation, new_generation);
        assert_eq!(
            relay
                .apply_replied_decision(generation, Decision::InferIdle, 300)
                .unwrap(),
            "stale_generation"
        );
        relay
            .apply_replied_decision(new_generation, Decision::ExpectContinuation, 400)
            .unwrap();
        assert!(matches!(
            relay.operational_state().activity_state(),
            ActivityState::Background { .. }
        ));
    }

    #[test]
    fn harness_restart_invalidates_the_completed_turn_inference_and_request() {
        let temp = tempfile::tempdir().unwrap();
        let mut relay = completed(temp.path());
        let generation = relay.turn_context.generation();
        relay
            .apply_replied_decision(generation, Decision::InferIdle, 200)
            .unwrap();
        relay
            .record_observation(RelayObservation::SessionRestarted)
            .unwrap();
        assert_eq!(relay.activity_facts().inferred_idle_since_ms, None);
        assert!(!relay.replied_verdict_is_current(generation));
        assert!(relay.pending_replied_verdict().is_none());
        assert_eq!(
            relay
                .apply_replied_decision(generation, Decision::InferIdle, 300)
                .unwrap(),
            "stale_generation"
        );
    }
}

#[cfg(test)]
mod settled_task_tests {
    use super::*;
    use mj_core::config::HarnessKind;

    fn claude_relay(root: &Path) -> DurableRelay {
        let mut relay = DurableRelay::open(root, "settle-test", "test").unwrap();
        relay.set_turn_verdict_harness(HarnessKind::Claude);
        relay.background_work = BackgroundWorkPolicy::ClaudeTasks;
        relay
    }

    fn task(id: &str) -> crate::acp::ClaudeBackgroundTask {
        crate::acp::ClaudeBackgroundTask {
            task_id: id.into(),
            description: format!("cargo test ({id})"),
        }
    }

    #[test]
    fn a_completed_task_keeps_the_session_busy_until_its_turn_opens() {
        let dir = tempfile::tempdir().unwrap();
        let mut relay = claude_relay(dir.path());
        relay
            .claude_background_tasks_changed(vec![task("a"), task("b")])
            .unwrap();
        assert!(relay.activity_facts().task_settled_at_ms.is_none());

        // The adapter's edge update for a completed task.
        relay
            .claude_async_task_control_changed("a".into(), false, true)
            .unwrap();
        let facts = relay.activity_facts();
        let settled = facts.task_settled_at_ms.expect("settle recorded");
        assert!(mj_core::activity::turn_imminent(&facts, settled + 1_000));
        assert_eq!(relay.operational_state().task_settled_at_ms, Some(settled));
        assert!(
            !relay.operational_state().is_quiet(),
            "a settled task is a turn about to start"
        );

        // The notification turn opens: the settle has done its job, and the
        // turn itself is now the reason the session is busy.
        relay
            .record_observation(RelayObservation::HarnessTurnStarted {
                started_at_ms: settled + 2_000,
            })
            .unwrap();
        assert!(relay.activity_facts().task_settled_at_ms.is_none());
        assert!(relay.activity_facts().harness_turn_started_at_ms.is_some());
    }

    #[test]
    fn a_confident_unneeded_judgment_releases_the_leftover_tasks_until_the_evidence_moves() {
        use mj_core::assessment::{Background, Failure, Input, Judgment, Verdict, Work};
        let dir = tempfile::tempdir().unwrap();
        let mut relay = claude_relay(dir.path());
        // A configured harness session; otherwise nothing is ever quiet.
        relay.acp_ready = true;
        relay
            .claude_background_tasks_changed(vec![task("srv")])
            .unwrap();
        relay
            .record_observation(RelayObservation::HarnessTurnStarted { started_at_ms: 1 })
            .unwrap();
        relay
            .record_session_update(
                serde_json::from_value(serde_json::json!({
                    "sessionUpdate": "agent_message_chunk",
                    "content": {"type": "text", "text": "Done; the dev server stays up for you."}
                }))
                .unwrap(),
            )
            .unwrap();
        relay.settle_harness_turn(Some("test".into())).unwrap();
        relay.prepare_pending_assessment().unwrap();
        let (generation, evidence, _) = relay.pending_replied_verdict().unwrap();
        assert_eq!(
            evidence.background.len(),
            1,
            "the leftover task is named in the evidence"
        );
        assert!(evidence.background[0].command.contains("srv"));
        let before = relay.activity_facts();
        assert!(mj_core::activity::driver_present(&before));
        assert_eq!(
            relay.operational_state().quiet().reason(),
            "background work"
        );

        let judged = |background| Verdict {
            failure: Judgment {
                choice: Failure::None,
                confidence: 0.99,
            },
            input: Judgment {
                choice: Input::None,
                confidence: 0.99,
            },
            work: Judgment {
                choice: Work::Finished,
                confidence: 0.99,
            },
            background: Some(Judgment {
                choice: background,
                confidence: 0.95,
            }),
        };
        relay
            .apply_turn_assessment(generation, judged(Background::Unneeded))
            .unwrap();
        let after = relay.activity_facts();
        assert_eq!(after.background_needed, Some(false));
        assert_eq!(
            after.background_commands, 1,
            "the task list itself is untouched"
        );
        assert!(!mj_core::activity::driver_present(&after));
        let quiet = relay.operational_state().quiet();
        assert!(quiet.is_yes(), "{}", quiet.reason());
        assert_eq!(relay.operational_state().background_needed, Some(false));

        // New foreground work invalidates the judgment with the rest of the inference.
        relay
            .record_observation(RelayObservation::HarnessTurnStarted { started_at_ms: 2 })
            .unwrap();
        assert!(relay.activity_facts().background_needed.is_none());
    }

    #[test]
    fn only_the_edge_update_settles_and_a_stopped_task_does_not() {
        let dir = tempfile::tempdir().unwrap();
        let mut relay = claude_relay(dir.path());
        relay
            .claude_background_tasks_changed(vec![task("a"), task("b")])
            .unwrap();
        // A stop the user asked for is answered with a notice, not a turn.
        relay
            .claude_async_task_control_changed("b".into(), false, false)
            .unwrap();
        assert!(relay.activity_facts().task_settled_at_ms.is_none());
        // The level shrinking cannot tell a completed task from a stopped
        // one, so it does not count as a settle on its own.
        relay
            .claude_background_tasks_changed(vec![task("a")])
            .unwrap();
        assert!(relay.activity_facts().task_settled_at_ms.is_none());
        assert_eq!(relay.operational_state().background_commands.len(), 1);
        relay
            .claude_async_task_control_changed("a".into(), false, true)
            .unwrap();
        assert!(relay.activity_facts().task_settled_at_ms.is_some());
        // A restart forgets it with the rest of the harness's processes.
        relay.forget_harness_processes();
        assert!(relay.activity_facts().task_settled_at_ms.is_none());
    }
}
