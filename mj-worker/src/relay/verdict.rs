//! Process-local completed-turn inference and bounded retry scheduling.
use super::*;
use mj_core::activity::ActivityFacts;
use mj_core::activity::verdict::{Decision, TurnEvidence, TurnPhase};
use std::sync::Mutex;
use std::time::{Duration, Instant};

pub(super) struct RepliedVerdictState {
    last_generation: Option<u64>,
    retry_at: Option<Instant>,
    retry_delay: Duration,
    skip_reason: Option<&'static str>,
    inference: Mutex<Option<(u64, Decision, i64)>>,
}

impl Default for RepliedVerdictState {
    fn default() -> Self {
        Self {
            last_generation: None,
            retry_at: None,
            retry_delay: Duration::from_secs(60),
            skip_reason: None,
            inference: Mutex::new(None),
        }
    }
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
    pub(super) fn inference(
        &self,
        generation: u64,
        facts: &ActivityFacts,
    ) -> (Option<i64>, Option<i64>) {
        let mut inference = self
            .inference
            .lock()
            .expect("verdict inference lock poisoned");
        if inference
            .as_ref()
            .is_some_and(|(accepted, _, _)| *accepted != generation)
            || blocked(facts).is_some()
        {
            *inference = None;
        }
        match *inference {
            Some((_, Decision::InferIdle, since)) => (None, Some(since)),
            Some((_, Decision::ExpectContinuation, since)) => (Some(since), None),
            _ => (None, None),
        }
    }
}

impl DurableRelay {
    pub fn pending_replied_verdict(&mut self) -> Option<(u64, TurnEvidence)> {
        self.pending_replied_verdict_at(Instant::now())
    }

    fn pending_replied_verdict_at(&mut self, now: Instant) -> Option<(u64, TurnEvidence)> {
        if !self.replied_verdict_pending {
            return None;
        }
        let facts = self.activity_facts();
        let generation = self.turn_context.generation();
        let reason = blocked(&facts).or(self.verdict_harness.is_none().then_some("no_harness"));
        if let Some(reason) = reason {
            if self.replied_verdict.skip_reason != Some(reason) {
                tracing::info!(target: "mj_jev", session = %self.snapshot.session_id,
                    phase = "replied", generation, reason, outcome = "skipped", "Jev classification skipped");
            }
            self.replied_verdict.skip_reason = Some(reason);
            return None;
        }
        self.replied_verdict.skip_reason = None;
        if self.replied_verdict.last_generation == Some(generation) {
            if self
                .replied_verdict
                .retry_at
                .is_none_or(|deadline| now < deadline)
            {
                return None;
            }
        } else {
            self.replied_verdict.retry_delay = Duration::from_secs(60);
        }
        self.replied_verdict.last_generation = Some(generation);
        self.replied_verdict.retry_at = None;
        Some((
            generation,
            self.turn_context.evidence(
                self.verdict_harness.expect("checked harness"),
                TurnPhase::Replied,
                &facts,
                epoch_millis(),
            ),
        ))
    }

    pub(crate) fn replied_verdict_is_current(&self, generation: u64) -> bool {
        let facts = self.activity_facts();
        self.replied_verdict_pending
            && generation == self.turn_context.generation()
            && blocked(&facts).is_none()
    }

    pub(crate) fn retry_replied_verdict(&mut self, generation: u64) {
        self.retry_replied_verdict_at(generation, Instant::now());
    }

    fn retry_replied_verdict_at(&mut self, generation: u64, now: Instant) {
        if self.replied_verdict_is_current(generation) {
            let delay = self.replied_verdict.retry_delay;
            self.replied_verdict.retry_at = Some(now + delay);
            self.replied_verdict.retry_delay = (delay * 2).min(Duration::from_secs(300));
            tracing::info!(target: "mj_jev", session = %self.snapshot.session_id,
                phase = "replied", generation, retry_after_s = delay.as_secs(), "Jev retry scheduled");
        }
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
        relay.replied_verdict_pending = true;
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
            .claude_async_task_control_changed("0".into(), true)
            .unwrap();
        let before = relay.operational_state().background_commands;
        assert!(
            before
                .iter()
                .any(|task| task.id == "claude:0" && task.can_stop)
        );
        let (generation, evidence) = relay.pending_replied_verdict().unwrap();
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
        let (generation, _) = relay.pending_replied_verdict().unwrap();
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
        let (new_generation, _) = relay.pending_replied_verdict().unwrap();
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
    fn inconclusive_replies_retry_with_bounded_backoff_and_stop_after_acceptance() {
        let temp = tempfile::tempdir().unwrap();
        let mut relay = completed(temp.path());
        let mut now = Instant::now();
        let (generation, _) = relay.pending_replied_verdict_at(now).unwrap();
        for seconds in [60, 120, 240, 300, 300] {
            relay.retry_replied_verdict_at(generation, now);
            now += Duration::from_secs(seconds);
            assert!(
                relay
                    .pending_replied_verdict_at(now - Duration::from_millis(1))
                    .is_none()
            );
            assert!(relay.pending_replied_verdict_at(now).is_some());
        }
        relay
            .apply_replied_decision(generation, Decision::InferIdle, 200)
            .unwrap();
        assert!(
            relay
                .pending_replied_verdict_at(now + Duration::from_secs(1_000))
                .is_none()
        );
    }
    #[test]
    fn harness_restart_invalidates_the_completed_turn_inference_and_request() {
        let temp = tempfile::tempdir().unwrap();
        let mut relay = completed(temp.path());
        let (generation, _) = relay.pending_replied_verdict().unwrap();
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
