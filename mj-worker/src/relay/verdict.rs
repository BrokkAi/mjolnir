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
    /// The turn (`turn_id`, completed ordinal) this process last asked Jev
    /// about. The background judgment dies with the process, so a restarted
    /// worker asks once more about a turn it has not asked about itself.
    asked_turn: Option<(String, u64)>,
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
    pub(crate) fn classifies_turns(&self) -> bool {
        !self.managed_subagent
    }

    /// Persist suppression so replaying controllers and older assessment readers
    /// never wait for a child classification. Keep the shared event format.
    pub(super) fn suppress_child_assessment(&mut self) -> Result<()> {
        if self.managed_subagent {
            self.replied_verdict = RepliedVerdictState::default();
            if let Some(mut assessment) = self
                .snapshot
                .assessment
                .as_deref()
                .cloned()
                .filter(|a| a.current())
            {
                assessment.status = mj_core::assessment::Status::Superseded;
                assessment.reason = "managed_subagent".into();
                assessment.retry_at_ms = None;
                self.store_assessment(assessment)?;
            }
        }
        Ok(())
    }

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
        if !self.classifies_turns() {
            return self.suppress_child_assessment();
        }
        self.reclassify_assessment_with_generated_authorization()?;
        if let Some(mut a) = self.snapshot.assessment.as_deref().cloned().filter(|a| {
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
                a.classify_again(self.snapshot.latest_ordinal + 1);
                self.store_assessment(a)?;
            }
        }
        self.reask_background_after_restart()?;
        if let Some(mut assessment) =
            self.snapshot
                .assessment
                .as_deref()
                .cloned()
                .filter(|assessment| {
                    assessment.status == mj_core::assessment::Status::Failed
                        && assessment.needs_classification(epoch_millis())
                })
        {
            // A failed request stays failed during backoff. Requeueing it is
            // the durable transition that makes the next check pending again.
            assessment.status = mj_core::assessment::Status::Pending;
            assessment.reason = "awaiting_classification".into();
            assessment.retry_at_ms = None;
            self.store_assessment(assessment)?;
        }
        let Some(mut a) = self
            .snapshot
            .assessment
            .as_deref()
            .cloned()
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
        let now = epoch_millis();
        evidence.background = self
            .background_commands()
            .into_iter()
            .take(mj_core::activity::verdict::IN_FLIGHT_TOOLS)
            .map(|command| mj_core::activity::verdict::BackgroundEvidence {
                command: mj_core::activity::verdict::tool_title(&command.command),
                id: command.id,
                started_s_ago: now.saturating_sub(command.started_at_ms).max(0) as u64 / 1000,
            })
            .collect();
        evidence.authorization = self.snapshot.authorization_context();
        a.evidence = Some(evidence);
        self.store_assessment(a)
    }

    /// Jev's judgment of leftover processes lives only in this process
    /// (#1202). After a worker restart, an assessed turn whose processes are
    /// reported again has none, and the session would stay busy for good.
    /// Ask again, once per process per turn: `asked_turn` records the ask, so
    /// an answer without a confident judgment is not asked a second time.
    #[cfg_attr(not(unix), allow(dead_code))]
    fn reask_background_after_restart(&mut self) -> Result<()> {
        if self.verdict_harness.is_none() || self.background_commands().is_empty() {
            return Ok(());
        }
        let Some(mut a) = self.snapshot.assessment.as_deref().cloned().filter(|a| {
            a.current()
                && a.status == mj_core::assessment::Status::Assessed
                && self.replied_verdict.asked_turn.as_ref()
                    != Some(&(a.turn_id.clone(), a.completed_ordinal))
        }) else {
            return Ok(());
        };
        if let Some(glide) = &a.glide {
            let answers = serde_json::to_value(glide)?;
            self.replied_verdict.judge_background(
                self.turn_context.generation(),
                mj_core::activity::eot_glide::background_needed(&answers, true),
            );
            self.replied_verdict.asked_turn = Some((a.turn_id.clone(), a.completed_ordinal));
            return Ok(());
        }
        let facts = self.activity_facts();
        if blocked(&facts).is_some() || facts.background_needed.is_some() {
            return Ok(());
        }
        tracing::info!(target: "mj_jev", session = %self.snapshot.session_id,
            revision = a.revision, phase = "replied",
            "asking Jev again about leftover processes after a worker restart");
        a.classify_again(self.snapshot.latest_ordinal + 1);
        self.store_assessment(a)
    }

    /// An assessment classified by an earlier worker can rest on a user
    /// message that today's rules attribute to Mjolnir, such as another
    /// session's message. Its verdict and action are not the person's to rely
    /// on, so the turn is classified again on the corrected history, the same
    /// way a seeded history does it.
    #[cfg_attr(not(unix), allow(dead_code))]
    fn reclassify_assessment_with_generated_authorization(&mut self) -> Result<()> {
        let Some(mut a) = self.snapshot.assessment.as_deref().cloned().filter(|a| {
            a.current()
                && !matches!(
                    a.action,
                    Some(
                        mj_core::assessment::Action::RetryProvider
                            | mj_core::assessment::Action::RecoverQuota
                    )
                )
                && a.evidence
                    .as_ref()
                    .and_then(|e| e.authorization.as_ref())
                    .is_some_and(mj_core::assessment::ContextHistory::has_generated_prompts)
        }) else {
            return Ok(());
        };
        a.classify_again(self.snapshot.latest_ordinal + 1);
        a.failures = 0;
        a.retry_at_ms = None;
        a.reason = "authorization_evidence_changed".into();
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
    pub(crate) fn question_locator_request(
        &self,
        revision: u64,
    ) -> Option<mj_core::activity::question_locator::Request> {
        let assessment = self.snapshot.assessment.as_ref()?;
        if assessment.revision != revision
            || !assessment.current()
            || assessment.action != Some(mj_core::assessment::Action::AwaitInput)
            || !(assessment.glide.as_ref().is_some_and(|g| {
                g.input.probabilities.required >= mj_core::activity::eot_glide::REQUIRED_INPUT
            }) || assessment
                .verdict
                .as_ref()
                .is_some_and(|v| v.requires_input()))
        {
            return None;
        }
        let context = assessment.evidence.as_ref()?.authorization.as_ref()?;
        let request = mj_core::activity::question_locator::build(context);
        (!request.message_ids.is_empty()).then_some(request)
    }

    #[cfg_attr(not(unix), allow(dead_code))]
    pub(crate) fn store_question_target(&mut self, revision: u64, stable_id: &str) -> Result<bool> {
        let Some(mut assessment) = self.snapshot.assessment.as_deref().cloned().filter(|a| {
            a.revision == revision
                && a.current()
                && a.action == Some(mj_core::assessment::Action::AwaitInput)
                && a.question_message.is_none()
        }) else {
            return Ok(false);
        };
        assessment.question_message = Some(stable_id.to_owned());
        self.store_assessment(assessment)?;
        Ok(true)
    }

    #[cfg_attr(not(unix), allow(dead_code))]
    #[cfg(test)]
    pub(crate) fn apply_turn_assessment(
        &mut self,
        ordinal: u64,
        verdict: mj_core::assessment::Verdict,
    ) -> Result<&'static str> {
        self.apply_assessment_result(ordinal, Some(verdict), None)
    }

    pub(crate) fn glide_runtime(&self) -> (ActivityFacts, usize) {
        (self.activity_facts(), self.native_agent_count())
    }

    pub(crate) fn apply_glide_assessment(
        &mut self,
        ordinal: u64,
        glide: mj_core::activity::eot_glide::types::Probabilities,
    ) -> Result<&'static str> {
        self.apply_assessment_result(ordinal, None, Some(glide))
    }

    fn apply_assessment_result(
        &mut self,
        ordinal: u64,
        verdict: Option<mj_core::assessment::Verdict>,
        glide: Option<mj_core::activity::eot_glide::types::Probabilities>,
    ) -> Result<&'static str> {
        use mj_core::assessment::{Action, Status};
        let Some(mut a) = self
            .snapshot
            .assessment
            .as_deref()
            .cloned()
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
        let answers = glide
            .as_ref()
            .map(|g| serde_json::to_value(g).expect("typed probabilities serialize"));
        let action = if let Some(answers) = &answers {
            mj_core::activity::eot_glide::action(answers, complete)
        } else {
            verdict
                .as_ref()
                .expect("assessment result")
                .action(complete)
        };
        // The quiet judgment is separate from the action: it only ever says
        // whether the listed leftover processes still matter, and only when
        // the request listed some.
        let listed = a
            .evidence
            .as_ref()
            .is_some_and(|e| !e.background.is_empty());
        let needed = verdict.as_ref().and_then(|v| {
            v.background
                .as_ref()
                .filter(|judgment| {
                    listed && judgment.confidence >= mj_core::activity::verdict::ACT_CONFIDENCE
                })
                .and_then(|judgment| match judgment.choice {
                    mj_core::assessment::Background::Needed => Some(true),
                    mj_core::assessment::Background::Unneeded => Some(false),
                    mj_core::assessment::Background::Unclear => None,
                })
        });
        let needed = answers.as_ref().map_or(needed, |answers| {
            mj_core::activity::eot_glide::background_needed(answers, listed)
        });
        self.replied_verdict
            .judge_background(self.turn_context.generation(), needed);
        a.verdict = verdict;
        a.glide = glide;
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
                Action::Continue if !self.continuation_enabled => "continuation_disabled",
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
            .as_deref()
            .cloned()
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
        if !self.classifies_turns() {
            return None;
        }
        let a = self.snapshot.assessment.as_ref()?;
        if !a.needs_classification(epoch_millis())
            || self.replied_verdict.last_generation == Some(a.revision)
        {
            return None;
        }
        let evidence = a.evidence.clone()?;
        self.replied_verdict.last_generation = Some(a.revision);
        self.replied_verdict.asked_turn = Some((a.turn_id.clone(), a.completed_ordinal));
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
            .as_deref()
            .cloned()
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
        // Jev's verdict on the turn is its assessment record, or the stop
        // reason a classifier handoff ended it with. This sets only the
        // process-local inference that the activity state reads.
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

    #[tokio::test]
    async fn glide_completed_turn_transport_and_replay_preserve_action_without_continue() {
        use crate::acp::verdict_client::{VerdictClient, VerdictSource};
        use crate::relay::test_support::{finish_prompt, prompt, submit_relay};
        use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt};
        let samples: serde_json::Value = serde_json::from_str(include_str!(
            "../../../mj-core/src/activity/eot_glide_fresh_sample.json"
        ))
        .unwrap();
        for sample in samples.as_array().unwrap() {
            let temp = tempfile::tempdir().unwrap();
            let mut relay = DurableRelay::open(temp.path(), "glide-replay", "test").unwrap();
            relay.set_acp_ready(true);
            relay.set_turn_verdict_harness(HarnessKind::Claude);
            submit_relay(&mut relay, "user-turn", prompt("Complete the task"));
            relay.claim_pending_commands(true).unwrap();
            finish_prompt(&mut relay, "user-turn");
            relay.prepare_pending_assessment().unwrap();
            let (revision, evidence, _) = relay.pending_replied_verdict().unwrap();
            let (facts, native) = relay.glide_runtime();
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let endpoint = format!("http://{}/v7/turn-verdict", listener.local_addr().unwrap());
            let response = serde_json::json!({"answers":sample["answers"]}).to_string();
            let server = tokio::spawn(async move {
                let (socket, _) = listener.accept().await.unwrap();
                let mut reader = tokio::io::BufReader::new(socket);
                let mut line = String::new();
                reader.read_line(&mut line).await.unwrap();
                assert!(line.starts_with("POST /v1/glide-turn-verdict "));
                let mut length = 0;
                loop {
                    line.clear();
                    reader.read_line(&mut line).await.unwrap();
                    if line == "\r\n" {
                        break;
                    }
                    if let Some(value) = line.to_lowercase().strip_prefix("content-length:") {
                        length = value.trim().parse().unwrap();
                    }
                }
                let mut body = vec![0; length];
                reader.read_exact(&mut body).await.unwrap();
                let request: serde_json::Value = serde_json::from_slice(&body).unwrap();
                assert_eq!(request["latest_user_message"], "Complete the task");
                assert!(request.get("questions").is_none());
                reader
                    .get_mut()
                    .write_all(
                        format!(
                            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                            response.len(),
                            response
                        )
                        .as_bytes(),
                    )
                    .await
                    .unwrap();
            });
            let client = VerdictClient::new(VerdictSource::Hosted { endpoint }).unwrap();
            let answer = client
                .ask_glide_turn(
                    &evidence,
                    evidence.authorization.clone().unwrap(),
                    &facts,
                    native,
                )
                .await
                .unwrap();
            server.await.unwrap();
            relay.apply_glide_assessment(revision, answer).unwrap();
            let assessment = relay.snapshot.assessment.clone().unwrap();
            assert_ne!(
                assessment.action,
                Some(mj_core::assessment::Action::Continue)
            );
            assert!(assessment.glide.is_some());
            drop(relay);
            let replay = DurableRelay::open(temp.path(), "glide-replay", "test").unwrap();
            assert_eq!(
                replay.snapshot.assessment.as_ref().unwrap().action,
                assessment.action
            );
            assert_eq!(
                replay.snapshot.assessment.as_ref().unwrap().glide,
                assessment.glide
            );
            let mut old = serde_json::to_value(&*assessment).unwrap();
            old.as_object_mut().unwrap().remove("glide");
            let old: mj_core::assessment::TurnAssessment = serde_json::from_value(old).unwrap();
            assert_eq!(old.action, assessment.action);
            assert!(old.glide.is_none());
        }
    }

    #[test]
    fn glide_worker_projection_capture_and_large_history_bound() {
        use crate::acp::verdict_client::glide_turn::project;
        use crate::relay::test_support::{finish_prompt, prompt, submit_relay};
        let temp = tempfile::tempdir().unwrap();
        let mut relay = DurableRelay::open(temp.path(), "glide-projection-test", "test").unwrap();
        relay.set_acp_ready(true);
        relay.set_turn_verdict_harness(HarnessKind::Claude);
        submit_relay(
            &mut relay,
            "user-turn",
            prompt("Run the workspace checks and report failures."),
        );
        relay.claim_pending_commands(true).unwrap();
        relay.record_session_update(serde_json::from_value(serde_json::json!({
            "sessionUpdate": "agent_message_chunk",
            "content": {"type": "text", "text": "The workspace checks passed. All requested checks are complete."}
        })).unwrap()).unwrap();
        finish_prompt(&mut relay, "user-turn");
        let facts = relay.activity_facts();
        let mut evidence =
            relay
                .turn_context
                .evidence(HarnessKind::Claude, TurnPhase::Replied, &facts, 200);
        evidence.completion = Some(mj_core::activity::verdict::CompletionEvidence::bounded(
            "end_turn", None,
        ));
        let history = relay.snapshot.authorization_context().unwrap();
        let body = project(
            &evidence,
            history.clone(),
            &facts,
            relay.native_agent_count(),
        )
        .unwrap();
        let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../services/jev-proxy/tests/fixtures/eot-glide-worker.json");
        if std::env::var_os("MJ_UPDATE_GLIDE_FIXTURE").is_some() {
            std::fs::write(
                &fixture,
                serde_json::to_string_pretty(&body).unwrap() + "\n",
            )
            .unwrap();
        }
        let captured: serde_json::Value =
            serde_json::from_slice(&std::fs::read(fixture).unwrap()).unwrap();
        assert_eq!(body, captured);
        let mut large = history;
        for i in 0..500 {
            large.messages.insert(
                0,
                mj_core::continuation::EvidenceMessage {
                    id: format!("old:{i}"),
                    role: "assistant".into(),
                    text: "historical context ".repeat(100),
                },
            );
        }
        let bounded = project(&evidence, large, &facts, relay.native_agent_count()).unwrap();
        let bytes = serde_json::to_vec(&bounded).unwrap().len();
        assert!((64 * 1024..=80 * 1024).contains(&bytes));
        assert_eq!(bounded["latest_user_message"], body["latest_user_message"]);
        assert_eq!(
            bounded["runtime_observations"],
            body["runtime_observations"]
        );
        assert!(bounded.get("questions").is_none());
        assert!(
            !bounded["runtime_observations"]
                .to_string()
                .contains("historical context")
        );
    }

    #[test]
    fn managed_child_completion_and_silent_turn_never_request_classification() {
        use crate::relay::test_support::{finish_prompt, prompt, submit_relay};
        for managed in [false, true] {
            let temp = tempfile::tempdir().unwrap();
            let mut relay = DurableRelay::open(temp.path(), "child-verdict-test", "test").unwrap();
            relay.set_acp_ready(true);
            relay.set_managed_subagent(managed).unwrap();
            relay.set_turn_verdict_harness(HarnessKind::Claude);
            submit_relay(&mut relay, "user-turn", prompt("Do the assigned work."));
            relay.claim_pending_commands(true).unwrap();
            relay.prepare_pending_assessment().unwrap();
            assert!(relay.pending_replied_verdict().is_none());
            // Silence alone never ends an observed open turn.
            assert!(relay.activity_facts().prompt_started_at_ms.is_some());
            finish_prompt(&mut relay, "user-turn");
            relay.prepare_pending_assessment().unwrap();
            assert_eq!(relay.pending_replied_verdict().is_some(), !managed);
            assert_eq!(relay.operational_state().retry_assessment_pending, !managed);
            assert!(relay.activity_facts().prompt_started_at_ms.is_none());
            if managed {
                assert!(relay.operational_state().is_quiet());
                assert!(!relay.snapshot.assessment.as_ref().unwrap().current());
            }
        }
        // An old worker's durable pending assessment is superseded on launch.
        let temp = tempfile::tempdir().unwrap();
        let mut relay = DurableRelay::open(temp.path(), "legacy-child", "test").unwrap();
        submit_relay(&mut relay, "old-turn", prompt("Work"));
        relay.claim_pending_commands(true).unwrap();
        finish_prompt(&mut relay, "old-turn");
        drop(relay);
        let mut relay = DurableRelay::open(temp.path(), "legacy-child", "test").unwrap();
        relay.set_managed_subagent(true).unwrap();
        assert!(!relay.operational_state().retry_assessment_pending);
        assert!(!relay.snapshot.assessment.as_ref().unwrap().current());
    }

    // Hard-won: 97731d30a: an assessment classified with a peer message as user authorization kept its action.
    #[test]
    fn an_assessment_resting_on_a_now_generated_prompt_is_classified_again() {
        use crate::relay::test_support::{finish_prompt, prompt, submit_relay};
        use mj_core::assessment::{Action, Status};
        use mj_core::continuation::EvidenceMessage;
        let temp = tempfile::tempdir().unwrap();
        let mut relay = DurableRelay::open(temp.path(), "reclassify-test", "test").unwrap();
        relay.set_turn_verdict_harness(HarnessKind::Claude);
        submit_relay(&mut relay, "user-turn", prompt("Fix the build."));
        relay.claim_pending_commands(true).unwrap();
        finish_prompt(&mut relay, "user-turn");
        relay.prepare_pending_assessment().unwrap();
        let assessed = |relay: &DurableRelay, peer: bool| {
            let mut assessment = relay.snapshot.assessment.as_deref().cloned().unwrap();
            let mut evidence = relay.turn_context.evidence(
                HarnessKind::Claude,
                TurnPhase::Replied,
                &relay.activity_facts(),
                0,
            );
            let mut messages = vec![EvidenceMessage {
                id: "user:user-turn".into(),
                role: "user".into(),
                text: "Fix the build.".into(),
            }];
            if peer {
                messages.push(EvidenceMessage {
                    id: "user:session-message-turn-peer-1".into(),
                    role: "user".into(),
                    text: "Please also push it.".into(),
                });
            }
            messages.push(EvidenceMessage {
                id: "agent:reply".into(),
                role: "assistant".into(),
                text: "Fixed; pushing next.".into(),
            });
            evidence.authorization = Some(mj_core::assessment::ContextHistory {
                messages,
                ..Default::default()
            });
            assessment.evidence = Some(evidence);
            assessment.action = Some(Action::Continue);
            assessment.status = Status::Assessed;
            assessment
        };
        let clean = assessed(&relay, false);
        relay.store_assessment(clean).unwrap();
        relay.prepare_pending_assessment().unwrap();
        assert_eq!(
            relay.snapshot.assessment.as_ref().unwrap().status,
            Status::Assessed
        );

        let mut stale = assessed(&relay, true);
        stale.question_message = Some("agent:reply".into());
        relay.store_assessment(stale).unwrap();
        relay.prepare_pending_assessment().unwrap();
        let a = relay.snapshot.assessment.as_ref().unwrap();
        assert_eq!(a.status, Status::Pending);
        assert_eq!(a.action, None);
        assert_eq!(a.reason, "authorization_evidence_changed");
        assert_eq!(a.question_message, None);
    }

    #[test]
    fn question_target_is_stored_only_for_current_input_gate_assessment() {
        use crate::relay::test_support::{finish_prompt, prompt, submit_relay};
        use mj_core::assessment::{Failure, Input, Judgment, Status, Verdict, Work};
        use mj_core::continuation::EvidenceMessage;
        let temp = tempfile::tempdir().unwrap();
        let mut relay = DurableRelay::open(temp.path(), "locator-test", "test").unwrap();
        relay.set_turn_verdict_harness(HarnessKind::Claude);
        submit_relay(&mut relay, "question-turn", prompt("Choose."));
        relay.claim_pending_commands(true).unwrap();
        finish_prompt(&mut relay, "question-turn");
        relay.prepare_pending_assessment().unwrap();
        let (revision, _, _) = relay.pending_replied_verdict().unwrap();
        let mut assessment = relay.snapshot.assessment.as_deref().cloned().unwrap();
        let mut evidence = relay.turn_context.evidence(
            HarnessKind::Claude,
            TurnPhase::Replied,
            &relay.activity_facts(),
            0,
        );
        let history = mj_core::assessment::ContextHistory {
            messages: vec![
                EvidenceMessage {
                    id: "user:1".into(),
                    role: "user".into(),
                    text: "task".into(),
                },
                EvidenceMessage {
                    id: "agent:one".into(),
                    role: "assistant".into(),
                    text: "Choose?".into(),
                },
                EvidenceMessage {
                    id: "agent:two".into(),
                    role: "assistant".into(),
                    text: "Waiting.".into(),
                },
            ],
            ..Default::default()
        };
        evidence.authorization = Some(history);
        assessment.evidence = Some(evidence);
        assessment.verdict = Some(Verdict {
            failure: Judgment {
                choice: Failure::None,
                confidence: 1.0,
                probabilities: Default::default(),
            },
            input: Judgment {
                choice: Input::Required,
                confidence: 1.0,
                probabilities: [("required".into(), 1.0)].into(),
            },
            work: Judgment {
                choice: Work::Waiting,
                confidence: 1.0,
                probabilities: Default::default(),
            },
            background: None,
            reply: None,
        });
        assessment.action = Some(mj_core::assessment::Action::AwaitInput);
        assessment.status = Status::Assessed;
        relay.store_assessment(assessment).unwrap();
        let request = relay.question_locator_request(revision).unwrap();
        assert_eq!(request.message_ids, ["agent:one", "agent:two"]);
        assert!(
            !relay
                .store_question_target(revision + 1, "agent:one")
                .unwrap()
        );
        assert!(relay.store_question_target(revision, "agent:one").unwrap());
        assert_eq!(
            relay
                .snapshot
                .assessment
                .as_ref()
                .unwrap()
                .question_message
                .as_deref(),
            Some("agent:one")
        );
        assert!(!relay.store_question_target(revision, "agent:two").unwrap());

        let mut one_reply = relay.snapshot.assessment.as_deref().cloned().unwrap();
        one_reply.question_message = None;
        one_reply
            .evidence
            .as_mut()
            .unwrap()
            .authorization
            .as_mut()
            .unwrap()
            .messages
            .pop();
        relay.store_assessment(one_reply).unwrap();
        assert_eq!(
            relay
                .question_locator_request(revision)
                .unwrap()
                .message_ids,
            ["agent:one"]
        );

        for action in [
            mj_core::assessment::Action::Finished,
            mj_core::assessment::Action::Wait,
        ] {
            let mut non_input = relay.snapshot.assessment.as_deref().cloned().unwrap();
            non_input.action = Some(action);
            relay.store_assessment(non_input).unwrap();
            assert!(relay.question_locator_request(revision).is_none());
        }
        let mut other_failure = relay.snapshot.assessment.as_deref().cloned().unwrap();
        other_failure.action = Some(mj_core::assessment::Action::AwaitInput);
        other_failure.verdict.as_mut().unwrap().input.choice = Input::None;
        other_failure.verdict.as_mut().unwrap().failure.choice = Failure::Other;
        relay.store_assessment(other_failure).unwrap();
        assert!(relay.question_locator_request(revision).is_none());
    }

    fn completed(root: &Path) -> DurableRelay {
        let mut relay = DurableRelay::open(root, "jev-test", "test").unwrap();
        relay.set_turn_verdict_harness(HarnessKind::Claude);
        relay.background_work = BackgroundWorkPolicy::AirTasks;
        relay
            .turn_context
            .reset(None, "How many tickets fell out of s21?");
        relay.record_session_update(serde_json::from_value(serde_json::json!({
            "sessionUpdate":"agent_message_chunk", "content":{"type":"text", "text":"One ticket so far: #3486."}
        })).unwrap()).unwrap();
        relay.set_air_tasks(tasks()).unwrap();
        for task in relay.air_tasks.values_mut() {
            if let super::super::background::AirTaskState::Background(command) = &mut task.state {
                command.started_at_ms = 100;
            }
        }
        relay.activity_facts();
        relay
    }

    fn tasks() -> Vec<crate::relay::AirTaskFixture> {
        (0..4)
            .map(|id| crate::relay::AirTaskFixture {
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
            .air_task_changed(
                "root".into(),
                crate::acp::AirTaskUpdate::Changed {
                    task_id: "0".into(),
                    description: None,
                    can_stop: Some(true),
                },
            )
            .unwrap();
        let before = relay.operational_state().background_commands;
        assert!(
            before
                .iter()
                .any(|task| task.id == "air:4:root:0" && task.can_stop)
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
            relay.background_task_stop_target("air:4:root:0").unwrap(),
            BackgroundTaskStopTarget::AirAsyncTask {
                session_id: "root".into(),
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
        relay.set_air_tasks(unchanged).unwrap();
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
        relay.set_air_tasks(replaced).unwrap();
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
        relay.set_harness_turn_policy(HarnessTurnPolicy::ClaudeAdapter);
        relay.background_work = BackgroundWorkPolicy::AirTasks;
        relay
    }

    #[test]
    fn dismissed_questions_survive_journal_replay_and_reject_stale_requests() {
        use crate::relay::test_support::{finish_prompt, prompt, relay_request, submit_relay};
        use mj_core::assessment::{Failure, Input, Judgment, Verdict, Work};

        let required = || Verdict {
            failure: Judgment {
                choice: Failure::None,
                confidence: 0.99,
                probabilities: Default::default(),
            },
            input: Judgment {
                choice: Input::Required,
                confidence: 0.99,
                probabilities: [("required".into(), 1.0)].into(),
            },
            work: Judgment {
                choice: Work::Waiting,
                confidence: 0.99,
                probabilities: Default::default(),
            },
            background: None,
            reply: None,
        };
        let dir = tempfile::tempdir().unwrap();
        let mut relay = claude_relay(dir.path());
        relay.set_acp_ready(true);
        relay
            .record_observation(RelayObservation::SessionConfigured {
                config_options: vec![],
            })
            .unwrap();
        submit_relay(&mut relay, "first-turn", prompt("Choose a target."));
        relay.claim_pending_commands(true).unwrap();
        finish_prompt(&mut relay, "first-turn");
        relay.prepare_pending_assessment().unwrap();
        let (revision, _, _) = relay.pending_replied_verdict().unwrap();
        relay.apply_turn_assessment(revision, required()).unwrap();
        let question = relay
            .snapshot
            .assessment
            .as_ref()
            .unwrap()
            .inferred_question()
            .unwrap();
        let context = serde_json::to_value(&relay.snapshot.assessment_context).unwrap();
        let command = RelayCommand::DismissQuestion {
            question: question.clone(),
        };
        let accepted = submit_relay(&mut relay, "dismiss-first", command.clone());
        assert!(
            relay
                .snapshot
                .assessment
                .as_ref()
                .unwrap()
                .question_dismissed()
        );
        assert_eq!(
            submit_relay(&mut relay, "dismiss-first", command.clone()),
            accepted
        );
        assert!(
            relay.claim_pending_commands(true).unwrap().is_empty(),
            "dismissal never reaches the harness"
        );
        assert_eq!(
            serde_json::to_value(&relay.snapshot.assessment_context).unwrap(),
            context,
            "dismissal grants no new authorization"
        );
        assert_eq!(
            relay.apply_turn_assessment(revision, required()).unwrap(),
            "stale_turn"
        );

        // Force reconstruction from the journal, rather than merely reading
        // the latest snapshot, to exercise serialization and replay together.
        drop(relay);
        std::fs::remove_file(dir.path().join(RELAY_STATE_FILE)).unwrap();
        let mut relay = claude_relay(dir.path());
        relay.set_acp_ready(true);
        assert!(
            relay
                .snapshot
                .assessment
                .as_ref()
                .unwrap()
                .question_dismissed()
        );
        relay.prepare_pending_assessment().unwrap();
        assert!(relay.pending_replied_verdict().is_none());
        assert_eq!(
            submit_relay(&mut relay, "dismiss-first", command.clone()),
            accepted
        );

        submit_relay(
            &mut relay,
            "second-turn",
            prompt("What about the other target?"),
        );
        relay.claim_pending_commands(true).unwrap();
        finish_prompt(&mut relay, "second-turn");
        relay.prepare_pending_assessment().unwrap();
        let (revision, _, _) = relay.pending_replied_verdict().unwrap();
        relay.apply_turn_assessment(revision, required()).unwrap();
        let next = relay
            .snapshot
            .assessment
            .as_ref()
            .unwrap()
            .inferred_question()
            .unwrap();
        assert_ne!(question, next);
        let response = relay.handle(relay_request(
            "stale-dismissal",
            RelayRequest::Submit {
                command_id: "delayed-dismissal".into(),
                command,
            },
        ));
        assert!(
            matches!(response.body, RelayResponseBody::Error { .. }),
            "{response:?}"
        );
        assert_eq!(
            relay
                .snapshot
                .assessment
                .as_ref()
                .unwrap()
                .inferred_question(),
            Some(next.clone())
        );
        relay
            .record_observation(RelayObservation::ElicitationRequested {
                request: mj_core::elicitation::ElicitationRequest {
                    id: "permission".into(),
                    message: "May I deploy?".into(),
                    title: None,
                    description: None,
                    fields: Vec::new(),
                    tool_permission: None,
                },
            })
            .unwrap();
        let response = relay.handle(relay_request(
            "form-owns-input",
            RelayRequest::Submit {
                command_id: "dismiss-form".into(),
                command: RelayCommand::DismissQuestion { question: next },
            },
        ));
        assert!(
            matches!(response.body, RelayResponseBody::Error { .. }),
            "{response:?}"
        );
        assert!(relay.snapshot.assessment_questions.contains("permission"));
    }

    fn task(id: &str) -> crate::relay::AirTaskFixture {
        crate::relay::AirTaskFixture {
            task_id: id.into(),
            description: format!("cargo test ({id})"),
        }
    }

    // Hard-won: 295a3f85: a completed Claude task looked idle during the gap before its task-notification turn.
    #[test]
    fn a_completed_task_keeps_the_session_busy_until_its_turn_opens() {
        let dir = tempfile::tempdir().unwrap();
        let mut relay = claude_relay(dir.path());
        relay.set_air_tasks(vec![task("a"), task("b")]).unwrap();
        assert!(relay.activity_facts().task_settled_at_ms.is_none());

        // The adapter's edge update for a completed task.
        relay
            .air_task_changed(
                "root".into(),
                crate::acp::AirTaskUpdate::Finished {
                    task_id: "a".into(),
                    settled: true,
                    failed: false,
                },
            )
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

    // Hard-won: agentclientprotocol/claude-agent-acp#1390: an autonomous turn's task end predicted a notification turn for 60 s.
    #[test]
    fn an_autonomous_turn_task_does_not_settle_or_offer_stop() {
        let dir = tempfile::tempdir().unwrap();
        let mut relay = claude_relay(dir.path());
        relay.set_harness_turn_policy(HarnessTurnPolicy::ClaudeAdapter);
        relay.set_acp_ready(true);
        relay.snapshot.native_session_id = Some("root".into());

        // claude-agent-acp 0.97.0's autonomousTurnStarted payload canStop=true although
        // this synthetic turn is not background work.
        relay
            .apply_claude_task_fixture("autonomous-turn-uuid".into(), true, true, false, false)
            .unwrap();
        assert!(relay.operational_state().background_commands.is_empty());
        assert!(
            relay
                .background_task_stop_target("air:4:root:autonomous-turn-uuid")
                .is_err()
        );

        // autonomousTurnEnded is an ordinary terminal update without taskType.
        relay
            .apply_claude_task_fixture("autonomous-turn-uuid".into(), false, false, true, false)
            .unwrap();
        let facts = relay.activity_facts();
        assert!(facts.task_settled_at_ms.is_none());
        assert!(relay.operational_state().is_quiet());
        assert!(
            relay
                .background_task_stop_target("air:4:root:autonomous-turn-uuid")
                .is_err()
        );
    }

    #[test]
    fn a_confident_unneeded_judgment_releases_the_leftover_tasks_until_the_evidence_moves() {
        use mj_core::assessment::{Background, Failure, Input, Judgment, Verdict, Work};
        let dir = tempfile::tempdir().unwrap();
        let mut relay = claude_relay(dir.path());
        // A configured harness session; otherwise nothing is ever quiet.
        relay.set_acp_ready(true);
        relay.set_air_tasks(vec![task("srv")]).unwrap();
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
        relay
            .settle_harness_turn(Some("test".into()), "end_turn")
            .unwrap();
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
                probabilities: Default::default(),
            },
            input: Judgment {
                choice: Input::None,
                confidence: 0.99,
                probabilities: Default::default(),
            },
            work: Judgment {
                choice: Work::Finished,
                confidence: 0.99,
                probabilities: [("finished".into(), 1.0)].into(),
            },
            background: Some(Judgment {
                choice: background,
                confidence: 0.95,
                probabilities: Default::default(),
            }),
            reply: None,
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

    /// #1202: the background judgment is process-local, so a restarted
    /// worker holds none. When the new harness reports leftover processes
    /// again (Kimi re-reads its task journal on attach), the worker asks Jev
    /// once more instead of leaving the session busy for good.
    // Hard-won: 9cb9d651: after restart, leftover processes kept `mj wait` blocked because Jev was never asked again.
    #[test]
    fn a_restarted_worker_asks_again_about_leftover_processes_once() {
        use mj_core::assessment::{Background, Failure, Input, Judgment, Verdict, Work};
        let judged = |background| Verdict {
            failure: Judgment {
                choice: Failure::None,
                confidence: 0.99,
                probabilities: Default::default(),
            },
            input: Judgment {
                choice: Input::None,
                confidence: 0.99,
                probabilities: Default::default(),
            },
            work: Judgment {
                choice: Work::Finished,
                confidence: 0.99,
                probabilities: [("finished".into(), 1.0)].into(),
            },
            background: Some(Judgment {
                choice: background,
                confidence: 0.95,
                probabilities: Default::default(),
            }),
            reply: None,
        };
        let restart = |root: &Path| {
            // What the worker records when it starts on an existing root.
            let mut relay = claude_relay(root);
            relay
                .record_observation(RelayObservation::SessionRestarted)
                .unwrap();
            relay.set_acp_ready(true);
            relay
        };
        let dir = tempfile::tempdir().unwrap();
        let mut relay = claude_relay(dir.path());
        relay.set_acp_ready(true);
        relay.set_air_tasks(vec![task("srv")]).unwrap();
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
        relay
            .settle_harness_turn(Some("test".into()), "end_turn")
            .unwrap();
        relay.prepare_pending_assessment().unwrap();
        let (revision, _, _) = relay.pending_replied_verdict().unwrap();
        relay
            .apply_turn_assessment(revision, judged(Background::Unneeded))
            .unwrap();
        assert!(relay.operational_state().quiet().is_yes());
        // The answer was applied by this process: nothing more to ask.
        relay.prepare_pending_assessment().unwrap();
        assert!(relay.pending_replied_verdict().is_none());
        drop(relay);

        let mut relay = restart(dir.path());
        relay.set_air_tasks(vec![task("srv")]).unwrap();
        assert!(relay.activity_facts().background_needed.is_none());
        relay.prepare_pending_assessment().unwrap();
        let (revision, evidence, _) = relay
            .pending_replied_verdict()
            .expect("the restarted worker asks about the leftover process");
        assert_eq!(evidence.background.len(), 1);
        assert!(evidence.background[0].command.contains("srv"));
        // One request per restart: an unclear answer is not asked again.
        let mut unclear = judged(Background::Unclear);
        unclear.background.as_mut().unwrap().confidence = 0.5;
        relay.apply_turn_assessment(revision, unclear).unwrap();
        assert!(relay.activity_facts().background_needed.is_none());
        for _ in 0..3 {
            relay.prepare_pending_assessment().unwrap();
            assert!(relay.pending_replied_verdict().is_none());
        }
        drop(relay);

        // The next restart asks once more, and a confident answer applies.
        let mut relay = restart(dir.path());
        relay.set_air_tasks(vec![task("srv")]).unwrap();
        relay.prepare_pending_assessment().unwrap();
        let (revision, _, _) = relay.pending_replied_verdict().unwrap();
        relay
            .apply_turn_assessment(revision, judged(Background::Unneeded))
            .unwrap();
        assert_eq!(relay.activity_facts().background_needed, Some(false));
        let quiet = relay.operational_state().quiet();
        assert!(quiet.is_yes(), "{}", quiet.reason());
        relay.prepare_pending_assessment().unwrap();
        assert!(relay.pending_replied_verdict().is_none());
    }

    /// Session 979dc, through the worker: prompt turn 20 ends while a build
    /// runs in the background and Jev says Wait; the build finishes, Claude
    /// Code runs and ends a turn of its own, and Jev says Uncertain about it.
    /// The assessment record holds each verdict for its own turn. The worker
    /// publishes no other copy of a verdict: daemons older than #1304 still
    /// read `turn_completion.decision`, and a Wait left there for turn 20 kept
    /// their waits open for good.
    // Hard-won: #1304: the worker kept turn 20's Wait verdict after the session went idle.
    #[test]
    fn a_verdict_stays_with_its_own_turn_in_the_published_state() {
        use crate::relay::test_support::{prompt, submit_relay};
        use agent_client_protocol::v1::schema::{ContentBlock, ContentChunk, TextContent};
        use mj_core::activity::verdict::Decision;
        use mj_core::assessment::{Action, Failure, Input, Judgment, Verdict, Work};
        use mj_core::relay::RelayCommandOutcome;
        let verdict = |work| Verdict {
            failure: Judgment {
                choice: Failure::None,
                confidence: 0.99,
                probabilities: Default::default(),
            },
            input: Judgment {
                choice: Input::None,
                confidence: 0.99,
                probabilities: Default::default(),
            },
            work: Judgment {
                choice: work,
                confidence: 0.99,
                probabilities: Default::default(),
            },
            background: None,
            reply: None,
        };
        let say = |relay: &mut DurableRelay, text: &str| {
            relay
                .record_session_update(SessionUpdate::AgentMessageChunk(ContentChunk::new(
                    ContentBlock::Text(TextContent::new(text)),
                )))
                .unwrap();
        };
        let dir = tempfile::tempdir().unwrap();
        let mut relay = claude_relay(dir.path());
        relay.set_acp_ready(true);
        relay
            .record_observation(RelayObservation::SessionConfigured {
                config_options: vec![],
            })
            .unwrap();
        submit_relay(&mut relay, "api-turn-20", prompt("Build it and report."));
        relay.claim_pending_commands(true).unwrap();
        relay.set_air_tasks(vec![task("build")]).unwrap();
        say(&mut relay, "The build is running in the background.");
        relay
            .record_command_completed(
                "api-turn-20",
                RelayCommandOutcome::Prompt {
                    work_facts: None,
                    diagnostic: None,
                    stop_reason: "end_turn".into(),
                    usage: None,
                },
            )
            .unwrap();
        relay.prepare_pending_assessment().unwrap();
        let (revision, _, _) = relay.pending_replied_verdict().unwrap();
        assert_eq!(
            relay
                .apply_turn_assessment(revision, verdict(Work::Waiting))
                .unwrap(),
            "background_work_pending"
        );

        // The build finishes and Claude Code answers it with its own turn.
        relay
            .air_task_changed(
                "root".into(),
                crate::acp::AirTaskUpdate::Finished {
                    task_id: "build".into(),
                    settled: true,
                    failed: false,
                },
            )
            .unwrap();
        relay.set_air_tasks(Vec::new()).unwrap();
        relay
            .record_observation(RelayObservation::HarnessTurnStarted {
                started_at_ms: epoch_millis(),
            })
            .unwrap();
        say(&mut relay, "The build passed.");
        relay
            .settle_harness_turn(Some("end_turn".into()), "end_turn")
            .unwrap();
        let completion = relay.operational_state().turn_completion;
        relay.prepare_pending_assessment().unwrap();
        let (revision, _, identity) = relay.pending_replied_verdict().unwrap();
        assert!(
            identity.is_some_and(|(turn_id, _)| turn_id.starts_with("harness-turn-")),
            "Jev is asked about the harness's own turn"
        );
        assert_eq!(
            relay
                .apply_turn_assessment(revision, verdict(Work::Unclear))
                .unwrap(),
            "assessment_uncertain"
        );

        let state = relay.operational_state();
        assert!(
            state.activity_state().is_idle(),
            "{:?}",
            state.activity_state()
        );
        let assessment = state.assessment.as_deref().unwrap();
        assert!(assessment.turn_id.starts_with("harness-turn-"));
        assert_eq!(assessment.action, Some(Action::Uncertain));
        assert_eq!(
            state.turn_completion, completion,
            "assessment keeps the harness settlement identity"
        );
        let completion = state.turn_completion.unwrap();
        assert_eq!(completion.command_id, assessment.turn_id);
        assert_eq!(completion.decision, Decision::KeepCurrent);
    }

    // Hard-won: 1ee54bfa: a disabled continuation left its unconsumed Continue verdict holding `mj wait` open.
    #[test]
    fn a_continue_verdict_is_not_parked_when_nobody_will_act_on_it() {
        use crate::relay::test_support::{prompt, submit_relay};
        use agent_client_protocol::v1::schema::{ContentBlock, ContentChunk, TextContent};
        use mj_core::assessment::{Failure, Input, Judgment, Status, Verdict, Work};
        use mj_core::relay::RelayCommandOutcome;
        let dir = tempfile::tempdir().unwrap();
        let mut relay = claude_relay(dir.path());
        relay
            .record_observation(RelayObservation::SessionConfigured {
                config_options: vec![],
            })
            .unwrap();
        relay.set_continuation_enabled(false);
        // A real user prompt, so the authorization history is complete and
        // the verdict can reach `Continue` at all.
        submit_relay(
            &mut relay,
            "original-prompt",
            prompt("Fix the failing test and push when done."),
        );
        assert_eq!(
            relay.claim_pending_commands(true).unwrap()[0].command_id,
            "original-prompt"
        );
        relay
            .record_session_update(SessionUpdate::AgentMessageChunk(ContentChunk::new(
                ContentBlock::Text(TextContent::new("Fixed the test. I'll push next.")),
            )))
            .unwrap();
        relay
            .record_command_completed(
                "original-prompt",
                RelayCommandOutcome::Prompt {
                    work_facts: None,
                    diagnostic: None,
                    stop_reason: "EndTurn".into(),
                    usage: None,
                },
            )
            .unwrap();
        relay.prepare_pending_assessment().unwrap();
        let (generation, evidence, _) = relay.pending_replied_verdict().unwrap();
        assert!(evidence.authorization.is_some());
        let verdict = Verdict {
            failure: Judgment {
                choice: Failure::None,
                confidence: 0.99,
                probabilities: Default::default(),
            },
            input: Judgment {
                choice: Input::None,
                confidence: 0.99,
                probabilities: Default::default(),
            },
            work: Judgment {
                choice: Work::AuthorizedUnfinished,
                confidence: 0.99,
                probabilities: Default::default(),
            },
            background: None,
            reply: None,
        };
        let reason = relay.apply_turn_assessment(generation, verdict).unwrap();
        assert_eq!(reason, "continuation_disabled");
        let assessment = relay.snapshot.assessment.as_ref().unwrap();
        assert_eq!(
            assessment.status,
            Status::Assessed,
            "nothing would consume a deferred one"
        );
        assert!(!relay.operational_state().retry_assessment_pending);
    }

    #[test]
    fn stopped_tasks_do_not_anticipate_a_notification_turn() {
        let dir = tempfile::tempdir().unwrap();
        let mut relay = claude_relay(dir.path());
        relay.set_air_tasks(vec![task("a"), task("b")]).unwrap();
        // A stop the user asked for is answered with a notice, not a turn.
        relay
            .air_task_changed(
                "root".into(),
                crate::acp::AirTaskUpdate::Finished {
                    task_id: "b".into(),
                    settled: false,
                    failed: false,
                },
            )
            .unwrap();
        assert!(relay.activity_facts().task_settled_at_ms.is_none());
        assert_eq!(relay.operational_state().background_commands.len(), 1);
        relay
            .air_task_changed(
                "root".into(),
                crate::acp::AirTaskUpdate::Finished {
                    task_id: "a".into(),
                    settled: true,
                    failed: false,
                },
            )
            .unwrap();
        assert!(relay.activity_facts().task_settled_at_ms.is_some());
        // A restart forgets it with the rest of the harness's processes.
        relay.forget_harness_processes();
        assert!(relay.activity_facts().task_settled_at_ms.is_none());
    }
}

impl DurableRelay {
    /// The delivery events own assignment identity. Reminders and provider retries
    /// do not replace it, so all their handbacks consume the original assignment.
    #[cfg_attr(not(unix), allow(dead_code))]
    pub(crate) fn prepare_handback_check(
        &mut self,
        report: &str,
    ) -> Result<Option<(String, u64, TurnEvidence)>> {
        if !self.continuation_enabled || mj_core::jev::continuation_disabled_by_environment() {
            return Ok(None);
        }
        let Some(harness) = self.verdict_harness else {
            return Ok(None);
        };
        let Some(assignment_id) = self.snapshot.handback_checks.assignment_id.clone() else {
            return Ok(None);
        };
        if self
            .snapshot
            .handback_checks
            .checked
            .contains(&assignment_id)
        {
            return Ok(None);
        }
        let Some(mut context) = self.snapshot.handback_checks.context.clone() else {
            return Ok(None);
        };
        context.open_assistant_id = None;
        context.assistant(
            Some("handback-report"),
            self.snapshot.latest_ordinal,
            report,
        );
        let mut evidence = self.turn_context.evidence(
            harness,
            TurnPhase::Replied,
            &self.activity_facts(),
            epoch_millis(),
        );
        // The report goes whole into the authorization history above; the
        // tail field keeps the bound every classifier route enforces.
        evidence.assistant_text_tail = mj_transcript::turn_context::tail(
            report,
            mj_core::activity::verdict::ASSISTANT_TEXT_BYTES,
        );
        evidence.final_tool_calls.clear();
        evidence.tools_in_flight.clear();
        evidence.completion = Some(mj_core::activity::verdict::CompletionEvidence::bounded(
            "end_turn", None,
        ));
        crate::acp::verdict_client::bound_authorization(&mut evidence, Some(context));
        // GLiDE judges the report against the parent's instructions. When they
        // did not survive whole (a non-text block, or a request too large to
        // carry them), there is nothing to judge against: deliver unchecked and
        // leave the check unspent.
        if !evidence
            .authorization
            .as_ref()
            .is_some_and(|context| context.authorization_complete)
        {
            return Ok(None);
        }
        // Journal before HTTP: a crash or cancelled socket consumes the attempt.
        self.record_observation(RelayObservation::HandbackCheckStarted { assignment_id })?;
        Ok(Some((
            self.snapshot.session_id.clone(),
            self.snapshot.latest_ordinal,
            evidence,
        )))
    }
}
