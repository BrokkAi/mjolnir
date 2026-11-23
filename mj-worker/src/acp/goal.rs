//! Native goal controls and recovery through the saved-session question surface.
use super::*;
use mj_core::goal::{GoalSnapshot, RECOVERY_ID};

async fn record_resume_sent(
    owner: Option<&std::sync::Weak<crate::relay::RelayCell>>,
    events: &mpsc::Sender<RuntimeEvent>,
    goal: Option<GoalSnapshot>,
) -> Result<Option<u64>> {
    if let Some(owner) = owner {
        owner
            .upgrade()
            .context("native resume relay owner disappeared")?;
        static NEXT_RESUME: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        let resume_id = NEXT_RESUME.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let (applied, applied_rx) = mj_core::acp::NativeGoalResumeAck::new_pair();
        // Send through the worker FIFO so every earlier queued observation
        // applies first (cannot prematurely clear this reservation), then await
        // the relay apply acknowledgement before dispatching `_session/goal`.
        emit_runtime_event(
            events,
            RuntimeEvent::NativeGoalResumeSent {
                resume_id,
                goal,
                applied,
            },
        )
        .await?;
        applied_rx
            .await
            .context("native resume relay event loop closed before recording reservation")?;
        return Ok(Some(resume_id));
    }
    Ok(None)
}

async fn record_resume_rejection(
    events: &mpsc::Sender<RuntimeEvent>,
    resume_id: Option<u64>,
    error: &agent_client_protocol::Error,
) -> Result<()> {
    // SDK outgoing _session/goal publication and lost response failures use
    // INTERNAL_ERROR (5.0.0 jsonrpc PreparedRequest/block_task). Its local
    // INVALID_REQUEST constructors handle initialize rewriting/incoming batches,
    // not this object-valued goal request; these rejections came from the peer.
    if matches!(
        error.code,
        agent_client_protocol::ErrorCode::INVALID_REQUEST
            | agent_client_protocol::ErrorCode::INVALID_PARAMS
            | agent_client_protocol::ErrorCode::METHOD_NOT_FOUND
    ) && let Some(resume_id) = resume_id
    {
        emit_runtime_event(events, RuntimeEvent::NativeGoalResumeRejected { resume_id }).await?;
    }
    Ok(())
}

type PendingControl = Pin<Box<dyn Future<Output = RuntimeEvent> + Send>>;

/// Polled by both command loops. Dropping the session drops all request waits;
/// the relay reports their commands interrupted rather than replaying them.
#[derive(Default)]
pub(super) struct PendingControls(Vec<(Arc<std::sync::atomic::AtomicBool>, PendingControl)>);

impl PendingControls {
    pub(super) fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub(super) fn start(
        &mut self,
        spec: &LaunchSpec,
        events: &mpsc::Sender<RuntimeEvent>,
        connection: &ConnectionTo<Agent>,
        session: &SessionId,
        request: (String, mj_core::goal::GoalControlAction),
        expected: Option<GoalSnapshot>,
    ) {
        let (request_id, action) = request;
        let owner = spec.terminal_owner.clone();
        let events = events.clone();
        let resume_goal = spec
            .goal_recovery
            .lock()
            .expect("goal lock poisoned")
            .state
            .snapshot
            .clone();
        let connection = connection.clone();
        let session = session.clone();
        let sent = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let sending = sent.clone();
        self.0.push((
            sent,
            Box::pin(async move {
                // Explicit native resume may answer only when its turn finishes.
                // Do not impose recovery's short acknowledgement timeout here.
                let admission = if action == mj_core::goal::GoalControlAction::Resume {
                    record_resume_sent(owner.as_ref(), &events, resume_goal).await
                } else {
                    Ok(None)
                };
                let response = match admission {
                    Ok(resume_id) => {
                        let request = connection.send_request(control_request(
                            &session,
                            action,
                            expected.as_ref(),
                        ));
                        sending.store(true, std::sync::atomic::Ordering::Release);
                        match request.block_task().await {
                            Ok(value) => Ok(value),
                            Err(error) => {
                                match record_resume_rejection(&events, resume_id, &error).await {
                                    Ok(()) => Err(anyhow::anyhow!("{error}")),
                                    Err(recording) => Err(recording
                                        .context(format!("goal request rejected: {error}"))),
                                }
                            }
                        }
                    }
                    Err(error) => Err(error),
                };
                match response {
                    Ok(_) => RuntimeEvent::GoalControlApplied { request_id },
                    Err(error) => RuntimeEvent::CommandRejected {
                        reason: mj_core::event_outcome::OutcomeReason::CommandFailed,
                        request_id,
                        message: format!("/goal {} failed: {error}", action.as_str()),
                    },
                }
            }),
        ));
    }

    pub(super) async fn next(&mut self) -> RuntimeEvent {
        std::future::poll_fn(|cx| {
            for index in 0..self.0.len() {
                if let std::task::Poll::Ready(event) = self.0[index].1.as_mut().poll(cx) {
                    // Keep requests ordered until their first poll sends them.
                    drop(self.0.remove(index));
                    return std::task::Poll::Ready(event);
                }
                // A full event FIFO must not let a later control overtake
                // the resume whose ordered marker is still being enqueued.
                if !self.0[index].0.load(std::sync::atomic::Ordering::Acquire) {
                    return std::task::Poll::Pending;
                }
            }
            std::task::Poll::Pending
        })
        .await
    }
}

/// The identity an automatic quota goal resume sends, so the adapter restarts
/// only the goal the usage limit stopped and never one whose budget is spent.
/// A user's own `/goal resume` carries none.
pub(super) fn control_identity(spec: &LaunchSpec, request_id: &str) -> Option<GoalSnapshot> {
    mj_core::continuation::is_quota_goal_resume(request_id)
        .then(|| {
            spec.goal_recovery
                .lock()
                .expect("goal lock poisoned")
                .state
                .snapshot
                .clone()
        })
        .flatten()
}

fn control_request(
    session: &SessionId,
    action: mj_core::goal::GoalControlAction,
    expected: Option<&GoalSnapshot>,
) -> UntypedMessage {
    let mut params = serde_json::json!({"sessionId":session, "action":action.as_str()});
    if let Some(goal) = expected {
        params["expectedGoal"] =
            serde_json::json!({"objective":goal.objective, "createdAt":goal.created_at});
    }
    UntypedMessage {
        method: "_session/goal".into(),
        params,
    }
}

pub(super) async fn publish(
    spec: &LaunchSpec,
    events: &mpsc::Sender<RuntimeEvent>,
    meta: serde_json::Value,
) -> Result<()> {
    let update: SessionUpdate = serde_json::from_value(
        serde_json::json!({"sessionUpdate":"session_info_update", "_meta":meta}),
    )?;
    spec.goal_recovery
        .lock()
        .expect("goal lock poisoned")
        .state
        .apply(&update)?;
    emit_runtime_event(
        events,
        RuntimeEvent::SessionUpdate {
            update: serde_json::to_value(update)?,
        },
    )
    .await
}

async fn decision_update(
    spec: &LaunchSpec,
    events: &mpsc::Sender<RuntimeEvent>,
    meta: serde_json::Value,
) -> Result<()> {
    let journal = spec
        .goal_recovery
        .lock()
        .expect("goal lock poisoned")
        .journal
        .clone();
    if let Some(journal) = journal {
        let update: SessionUpdate = serde_json::from_value(
            serde_json::json!({"sessionUpdate":"session_info_update", "_meta":meta}),
        )?;
        let persisted = update.clone();
        mj_core::runtime::spawn_blocking(move || (journal.0)(persisted))
            .await
            .context("persist goal decision task")??;
        spec.goal_recovery
            .lock()
            .expect("goal lock poisoned")
            .state
            .apply(&update)?;
        Ok(())
    } else {
        publish(spec, events, meta).await
    }
}

/// Explicit controls supersede any saved restart answer before native dispatch.
pub(super) async fn prepare_control(
    spec: &LaunchSpec,
    events: &mpsc::Sender<RuntimeEvent>,
    pending: &mut Option<Question>,
    model_pending: bool,
    request_id: &str,
    action: mj_core::goal::GoalControlAction,
) -> Result<bool> {
    let context = spec
        .goal_recovery
        .lock()
        .expect("goal lock poisoned")
        .clone();
    let error = if !context.state.supports(action) {
        Some(format!(
            "/goal {} is not supported by this adapter",
            action.as_str()
        ))
    } else if mj_core::continuation::is_quota_goal_resume(request_id)
        && !context.state.resumable_after_quota()
    {
        Some("the goal is no longer stopped by the usage limit".into())
    } else if model_pending && action == mj_core::goal::GoalControlAction::Resume {
        Some("Choose the replacement model before resuming the goal".into())
    } else {
        None
    };
    if let Some(message) = error {
        emit_runtime_event(
            events,
            RuntimeEvent::CommandRejected {
                reason: mj_core::event_outcome::OutcomeReason::AdmissionRejected,
                request_id: request_id.into(),
                message,
            },
        )
        .await?;
        return Ok(false);
    }
    if pending.is_some() || context.state.decision.is_some() || context.asking() {
        let mut meta = serde_json::json!({"mjGoalDecision":null});
        if let Some(intent) = context.request_id() {
            meta["mjGoalResumeAnswered"] = intent.into();
        }
        if let Err(error) = decision_update(spec, events, meta).await {
            emit_runtime_event(
                events,
                RuntimeEvent::CommandRejected {
                    reason: mj_core::event_outcome::OutcomeReason::CommandFailed,
                    request_id: request_id.into(),
                    message: format!("Could not save goal control intent: {error:#}"),
                },
            )
            .await?;
            return Ok(false);
        }
        if pending.take().is_some() {
            emit_runtime_event(
                events,
                RuntimeEvent::ElicitationResolved {
                    elicitation_id: RECOVERY_ID.into(),
                    action: "cancel".into(),
                    reply: None,
                },
            )
            .await?;
        }
    }
    Ok(true)
}

pub(super) struct Question {
    request: CommandLoopQuestion,
    goal: GoalSnapshot,
    intent: Option<String>,
}

async fn control(
    spec: &LaunchSpec,
    events: &mpsc::Sender<RuntimeEvent>,
    connection: &ConnectionTo<Agent>,
    session: &SessionId,
    goal: &GoalSnapshot,
    resume: bool,
) -> Result<()> {
    ensure!(
        goal.control_method.as_deref() == Some("_session/goal"),
        "goal resume is not supported by this harness"
    );
    goal.created_at.context("goal identity is unavailable")?;
    let action = if resume {
        mj_core::goal::GoalControlAction::Resume
    } else {
        mj_core::goal::GoalControlAction::Pause
    };
    let resume_id = if resume {
        record_resume_sent(spec.terminal_owner.as_ref(), events, Some(goal.clone())).await?
    } else {
        None
    };
    // Timeout and SDK internal errors cannot prove rejection.
    let response = tokio::time::timeout(Duration::from_secs(30), connection.send_request(control_request(session, action, Some(goal))).block_task())
        .await.context("goal resume acknowledgement timed out; current execution must be reconciled before retrying")?;
    if let Err(error) = response {
        record_resume_rejection(events, resume_id, &error).await?;
        return Err(error.into());
    }
    Ok(())
}

pub(super) async fn recover(
    recovery_supported: bool,
    connection: &ConnectionTo<Agent>,
    session: &SessionId,
    spec: &LaunchSpec,
    events: &mpsc::Sender<RuntimeEvent>,
    pending_elicitations: &PendingElicitations,
    model_question: bool,
) -> Result<Option<Question>> {
    let context = spec
        .goal_recovery
        .lock()
        .expect("goal lock poisoned")
        .clone();
    if !recovery_supported || spec.resume_session.is_none() {
        return Ok(None);
    }
    // A crash can leave the prior question in the durable projection after
    // its decision was saved. Rebuild this session-owned question from intent.
    emit_runtime_event(
        events,
        RuntimeEvent::ElicitationResolved {
            elicitation_id: RECOVERY_ID.into(),
            action: "cancel".into(),
            reply: None,
        },
    )
    .await?;
    if !context.state.synchronized() {
        emit_runtime_event(events, RuntimeEvent::Warning {
            message: "The adapter did not synchronize goal execution state; goal recovery and worker replacement are deferred".into(),
        }).await?;
        return Ok(None);
    }
    if let Some(decision) = &context.state.decision
        && !context.asking()
        && !model_question
    {
        if context.state.snapshot.as_ref().is_some_and(|g| {
            g.same_goal(&decision.goal) && matches!(g.status.as_str(), "active" | "paused")
        }) {
            control(
                spec,
                events,
                connection,
                session,
                &decision.goal,
                decision.resume,
            )
            .await?;
        }
        decision_update(spec, events, serde_json::json!({"mjGoalDecision":null})).await?;
        return Ok(None);
    }
    if context.state.decision.is_some() {
        // A new explicit open or model choice supersedes an older answer.
        decision_update(spec, events, serde_json::json!({"mjGoalDecision":null})).await?;
    }
    if !context.asking() && !model_question {
        return Ok(None);
    }
    let Some(goal) = context
        .state
        .snapshot
        .clone()
        .filter(|g| matches!(g.status.as_str(), "active" | "paused"))
    else {
        if context.asking()
            && let Some(id) = context.request_id()
        {
            publish(spec, events, serde_json::json!({"mjGoalResumeAnswered":id})).await?;
        }
        return Ok(None);
    };
    if model_question && goal.active() {
        control(spec, events, connection, session, &goal, false).await?;
    }
    let request = ElicitationRequest {
        id: RECOVERY_ID.into(),
        title: Some("Resume this goal?".into()),
        message: goal.objective.clone(),
        description: Some(
            "Resume the existing goal with its remaining budget, or keep it paused.".into(),
        ),
        fields: vec![ElicitationField {
            id: "action".into(),
            title: "Goal".into(),
            description: None,
            required: true,
            secret: false,
            custom_answer_for: None,
            custom_answer_option: None,
            kind: ElicitationFieldKind::SingleSelect {
                options: vec![
                    ElicitationOption {
                        value: "resume".into(),
                        title: "Resume goal".into(),
                        description: None,
                        preview: None,
                    },
                    ElicitationOption {
                        value: "pause".into(),
                        title: "Keep paused".into(),
                        description: None,
                        preview: None,
                    },
                ],
                default: Some("resume".into()),
            },
        }],
        tool_permission: None,
    };
    let request = pending_elicitations.register_command_loop(request);
    emit_runtime_event(
        events,
        RuntimeEvent::ElicitationRequested {
            request: (*request).clone(),
        },
    )
    .await?;
    Ok(Some(Question {
        request,
        goal,
        intent: context.request_id().map(str::to_owned),
    }))
}

pub(super) async fn resolve(
    connection: &ConnectionTo<Agent>,
    session: &SessionId,
    spec: &LaunchSpec,
    events: &mpsc::Sender<RuntimeEvent>,
    pending: &mut Option<Question>,
    model_pending: bool,
    answer: (&str, &ElicitationResponse),
) -> Result<Option<std::result::Result<(), String>>> {
    let (id, response) = answer;
    let Some(question) = pending.as_ref().filter(|q| q.request.id == id) else {
        return Ok(None);
    };
    if let Err(error) = question.request.validate_response(response) {
        return Ok(Some(Err(error)));
    }
    let resume = matches!(response, ElicitationResponse::Accept { content } if matches!(content.get("action"), Some(ElicitationValue::String(action)) if action == "resume"));
    if resume && model_pending {
        return Ok(Some(Err(
            "Choose the replacement model before resuming the goal".into(),
        )));
    }
    let state = spec
        .goal_recovery
        .lock()
        .expect("goal lock poisoned")
        .state
        .clone();
    let current = state.snapshot.as_ref().is_some_and(|g| {
        g.same_goal(&question.goal) && matches!(g.status.as_str(), "active" | "paused")
    });
    let decision = current.then(|| mj_core::goal::GoalDecision {
        goal: question.goal.clone(),
        resume,
    });
    let mut meta = serde_json::json!({"mjGoalDecision":decision});
    if let Some(intent) = &question.intent {
        meta["mjGoalResumeAnswered"] = intent.clone().into();
    }
    decision_update(spec, events, meta).await?;
    if current {
        if let Err(error) = control(spec, events, connection, session, &question.goal, resume).await
        {
            return Ok(Some(Err(format!("{error:#}"))));
        }
        decision_update(spec, events, serde_json::json!({"mjGoalDecision":null})).await?;
    }
    let reply = Some(question.request.reply_text(response));
    *pending = None;
    emit_runtime_event(
        events,
        RuntimeEvent::ElicitationResolved {
            elicitation_id: id.into(),
            action: response.action_name().into(),
            reply,
        },
    )
    .await?;
    Ok(Some(Ok(())))
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    // Hard-won: 5c9e20e: guarded Codex identity rejection uses invalidRequest.
    #[tokio::test]
    async fn stored_goals_pause_on_reopen_and_only_resume_on_explicit_intent() {
        let _ = mj_harness_install::login_environment::initialize_from_parent();
        enum Reopen {
            QueuedWork(&'static str, &'static str),
            Running,
            Form(Option<bool>, &'static str),
            SavedAnswer,
            ResumeTimedOut,
            ResumeRejected(i32),
            PauseUnsupported,
        }
        use Reopen::*;
        for case in [
            QueuedWork("resume", "active"),
            QueuedWork("load", "active"),
            Running,
            Form(Some(true), "active"),
            Form(Some(false), "active"),
            // Hard-won: ba25f84: an unanswered command-loop goal form must survive adoption until bridge exit.
            Form(None, "active"),
            QueuedWork("resume", "paused"),
            QueuedWork("resume", "blocked"),
            QueuedWork("resume", "limited"),
            QueuedWork("resume", "complete"),
            Form(None, "complete"),
            SavedAnswer,
            ResumeTimedOut,
            // Hard-won: 655be2b: only proven peer rejection releases its own resume.
            ResumeRejected(-32600),
            ResumeRejected(-32602),
            ResumeRejected(-32601),
            ResumeRejected(-32603),
            PauseUnsupported,
        ] {
            let (method, status) = match case {
                QueuedWork(method, status) => (method, status),
                Form(_, status) => ("resume", status),
                SavedAnswer => ("resume", "paused"),
                _ => ("resume", "active"),
            };
            let timed_out = matches!(case, ResumeTimedOut);
            let reject_code = match case {
                ResumeRejected(code) => Some(code),
                _ => None,
            };
            let answer = match case {
                Form(answer, _) => answer,
                ResumeTimedOut | ResumeRejected(_) => Some(true),
                _ => None,
            };
            let asking = matches!(case, Form(..) | ResumeTimedOut | ResumeRejected(_));
            let running = matches!(case, Running);
            let decision = matches!(case, SavedAnswer);
            let queued_prompt = matches!(case, QueuedWork(..));
            let pause_supported = !matches!(case, PauseUnsupported);
            let root = tempfile::tempdir().unwrap();
            let script = root.path().join("goal.py");
            std::fs::write(&script, r#"
import json, os, sys, time
asking = os.environ['ASKING'] == 'true'
running = os.environ['RUNNING'] == 'true'
open_method = 'session/' + os.environ['OPEN_METHOD']
pause_supported = os.environ['PAUSE_SUPPORTED'] == 'true'
def emit(x): print(json.dumps(x), flush=True)
def update(meta): emit({'jsonrpc':'2.0','method':'session/update','params':{'sessionId':'native','update':{'sessionUpdate':'session_info_update','_meta':meta}}})
goal = {'objective':'finish campaign','status':'active','createdAt':123,'tokenBudget':900,'tokensUsed':100,'controlMethod':'_session/goal'}
for line in sys.stdin:
    r = json.loads(line); method=r.get('method'); ident=r.get('id'); p=r.get('params',{})
    if ident is None: continue
    if method == 'initialize':
        result={'protocolVersion':1,'agentCapabilities':{'loadSession':True,'sessionCapabilities':{'resume':{}} if open_method == 'session/resume' else {}},'_meta':{'jetbrains':{'air':{'goal':{'version':1,'controlMethod':'_session/goal','actions':['resume','pause'],'resumePolicies':['pause','preserve'] if pause_supported else ['preserve']}}},'execution':{'version':1}}}
    elif method in ('session/load', 'session/resume'):
        assert method == open_method and pause_supported
        assert p['_meta']['jetbrains']['air']['goal']['resumePolicy'] == 'pause'
        # For ordinary reopen, deliberately report the pre-pause status. Even
        # with an active snapshot Mjolnir must not issue a second resume.
        goal['status']='paused' if asking and os.environ['GOAL_STATUS']=='active' else os.environ['GOAL_STATUS']
        result={'modes':{'currentModeId':'agent','availableModes':[{'id':'agent','name':'Guardian'},{'id':'auto','name':'Auto'}]},'_meta':{'jetbrains':{'air':{'goal':goal}},'execution':{'version':1,'status':'running' if running else 'idle','turnId':'native-turn' if running else None}}}
        # Live output is legal before the resume response and exceeds a pipe buffer.
        if running:
            emit({'jsonrpc':'2.0','method':'session/update','params':{'sessionId':'native','update':{'sessionUpdate':'agent_message_chunk','content':{'type':'text','text':'x'*80000}}}})
    elif method == 'session/prompt':
        assert p['prompt'][0]['text'] == 'queued work'
        assert not os.path.exists('resumes'), 'queued prompt was overtaken by a resume'
        emit({'jsonrpc':'2.0','method':'session/update','params':{'sessionId':'native','update':{'sessionUpdate':'agent_message_chunk','content':{'type':'text','text':'queued work done'}}}})
        result={'stopReason':'end_turn'}
    elif method == '_session/goal':
        if p['action']=='resume':
            assert p['expectedGoal']=={'objective':'finish campaign','createdAt':123}
            with open('resumes','a') as f: f.write('resume\n')
            if os.environ.get('WAIT_FOR_CUT') == 'true':
                while not os.path.exists('release-resume'): time.sleep(0.01)
            if os.environ.get('REJECT_CODE'):
                emit({'jsonrpc':'2.0','id':ident,'error':{'code':int(os.environ['REJECT_CODE']),'message':'recorded rejection'}})
                continue
            goal['status']='active'; update({'jetbrains':{'air':{'goal':goal}},'execution':{'version':1,'status':'running','turnId':'resumed'}})
        else:
            goal['status']='paused'; update({'jetbrains':{'air':{'goal':goal}}})
        result={}
    else: result={}
    emit({'jsonrpc':'2.0','id':ident,'result':result})
"#).unwrap();
            let context = Arc::new(Mutex::new(mj_core::goal::GoalRecoveryContext {
                request: asking.then(|| "explicit-1".into()),
                state: mj_core::goal::GoalState {
                    decision: decision.then(|| mj_core::goal::GoalDecision {
                        goal: serde_json::from_value(serde_json::json!({"objective":"finish campaign","status":"paused","createdAt":123,"controlMethod":"_session/goal"})).unwrap(), resume: true,
                    }),
                    ..Default::default()
                },
                ..Default::default()
            }));
            let track_cut = answer == Some(true);
            let mut durable = crate::relay::DurableRelay::open(
                root.path().join("relay"),
                crate::relay::test_support::SESSION,
                "test",
            )
            .unwrap();
            durable.set_harness_turn_policy(crate::relay::HarnessTurnPolicy::CodexAdapter);
            let relay = Arc::new(crate::relay::RelayCell::new(durable));
            let spec = LaunchSpec {
                operator_away: relay.observe().unwrap().operator_away.clone(),
                imported_logins: None,
                login_identity: Default::default(),
                bridge_spec_path: None,
                subagent_policy: mj_core::subagent::SubagentPolicy::Native,
                managed_subagent: false,
                instruction_role: None,
                subagent_mcp_socket: None,
                clear_context_request: None,
                context_restore: None,
                goal_recovery: context.clone(),
                command: "python3".into(),
                args: vec![script.to_string_lossy().into_owned()],
                environment: BTreeMap::from([
                    ("ASKING".into(), asking.to_string()),
                    ("OPEN_METHOD".into(), method.into()),
                    ("PAUSE_SUPPORTED".into(), pause_supported.to_string()),
                    ("WAIT_FOR_CUT".into(), track_cut.to_string()),
                    (
                        "REJECT_CODE".into(),
                        reject_code
                            .map(|code: i32| code.to_string())
                            .unwrap_or_default(),
                    ),
                    ("RUNNING".into(), running.to_string()),
                    ("GOAL_STATUS".into(), status.into()),
                ]),
                cwd: root.path().into(),
                additional_directories: vec![],
                project_memory: None,
                extra_mcp_servers: vec![],
                resume_session: Some("native".into()),
                native_session_history: NativeSessionHistory::MayHaveHistory,
                accepted_config: Default::default(),
                initial_model: None,
                harness: HarnessKind::Codex,
                execution_policy: ExecutionPolicy::ConfiguredApprovals,
                approval_choices: Default::default(),
                acp_activity: Default::default(),
                step_clock: Default::default(),
                tools_in_flight: Default::default(),
                terminal_owner: track_cut.then(|| Arc::downgrade(&relay)),
                turn_context: Default::default(),
                verdict: Some(crate::acp::VerdictSource::Disabled),
                stall_policy: None,
            };
            let (tx, rx) = mpsc::channel(8);
            let (events, mut receive) = mpsc::channel(8);
            if queued_prompt {
                tx.send(CommandRequest::Prompt {
                    request_id: "queued-work".into(),
                    prompt: vec![ContentBlock::from("queued work".to_owned())],
                })
                .await
                .unwrap();
            }
            let task = mj_core::runtime::spawn(run(spec, rx, events));
            if !pause_supported {
                tokio::time::timeout(Duration::from_secs(10), async {
                    let mut refused = false;
                    while let Some(event) = receive.recv().await {
                        assert!(!matches!(event, RuntimeEvent::SessionStarted { .. }));
                        if let RuntimeEvent::SessionFault { message } = event {
                            assert!(message.contains("cannot pause a stored goal"), "{message}");
                            refused = true;
                        }
                    }
                    assert!(refused);
                    assert!(task.await.unwrap().is_err());
                })
                .await
                .unwrap();
                continue;
            }
            let mut in_flight = BTreeMap::new();
            let mut bytes = 0;
            if asking && status == "active" {
                let request = tokio::time::timeout(Duration::from_secs(10), async {
                    loop {
                        let event = receive.recv().await.unwrap();
                        if let RuntimeEvent::ElicitationRequested { request } = event {
                            break request;
                        }
                        crate::worker_runtime::record_runtime_event(&relay, &mut in_flight, event)
                            .unwrap();
                    }
                })
                .await
                .unwrap();
                assert_eq!(request.id, RECOVERY_ID);
                assert!(!root.path().join("resumes").exists());
                {
                    let mut state = relay.lock().unwrap();
                    state.operator_away.set(mj_core::state::OperatorPresence {
                        away: true,
                        revision: 1,
                    });
                    state
                        .record_observation(
                            mj_core::relay::RelayObservation::ElicitationRequested {
                                request: request.clone(),
                            },
                        )
                        .unwrap();
                    let cut = state.live_session_baseline().unwrap();
                    assert_eq!(cut.questions, vec![request.clone()]);
                    let adopted =
                        mj_transcript::projection::materialized_session_from_live_baseline(&cut)
                            .unwrap();
                    assert_eq!(adopted.pending_elicitations, vec![request]);
                }
                if let Some(accept) = answer {
                    let response = if accept {
                        ElicitationResponse::Accept {
                            content: BTreeMap::from([(
                                "action".into(),
                                ElicitationValue::String("resume".into()),
                            )]),
                        }
                    } else {
                        ElicitationResponse::Decline
                    };
                    let (resolved, mut ack) = oneshot::channel();
                    tx.send(CommandRequest::ResolveElicitation {
                        elicitation_id: RECOVERY_ID.into(),
                        response,
                        resolved,
                    })
                    .await
                    .unwrap();

                    let mut cut = if track_cut {
                        // Even an explicit form answer cannot reach ACP until
                        // the relay has applied its reservation in FIFO order.
                        let reservation = tokio::time::timeout(Duration::from_secs(10), async {
                            loop {
                                let event = receive.recv().await.unwrap();
                                if matches!(event, RuntimeEvent::NativeGoalResumeSent { .. }) {
                                    break event;
                                }
                                crate::worker_runtime::record_runtime_event(
                                    &relay,
                                    &mut in_flight,
                                    event,
                                )
                                .unwrap();
                            }
                        })
                        .await
                        .unwrap();
                        tokio::time::sleep(Duration::from_millis(50)).await;
                        assert!(
                            !root.path().join("resumes").exists(),
                            "_session/goal must wait for the relay reservation"
                        );
                        crate::worker_runtime::record_runtime_event(
                            &relay,
                            &mut in_flight,
                            reservation,
                        )
                        .unwrap();
                        let mut cut = relay.lock().unwrap().reserve_live_baseline();
                        assert!(cut.try_recv().is_err());
                        assert!(
                            relay
                                .observe()
                                .unwrap()
                                .adoption_blockers()
                                .iter()
                                .any(|blocker| blocker.contains("goal resume"))
                        );
                        tokio::time::timeout(Duration::from_secs(10), async {
                            while !root.path().join("resumes").exists() {
                                tokio::time::sleep(Duration::from_millis(10)).await;
                            }
                        })
                        .await
                        .unwrap();
                        if timed_out {
                            tokio::time::pause();
                            tokio::time::advance(Duration::from_secs(31)).await;
                            tokio::time::resume();
                        } else {
                            std::fs::write(root.path().join("release-resume"), "").unwrap();
                        }
                        Some(cut)
                    } else {
                        None
                    };

                    let result = tokio::time::timeout(Duration::from_secs(10), async {
                        loop {
                            tokio::select! {
                                result = &mut ack => break result.unwrap(),
                                event = receive.recv() => {
                                    crate::worker_runtime::record_runtime_event(
                                        &relay, &mut in_flight, event.unwrap(),
                                    )
                                    .unwrap();
                                }
                            }
                        }
                    })
                    .await
                    .unwrap();
                    if let Some(cut) = &mut cut {
                        if timed_out || reject_code == Some(-32603) {
                            let error = result.unwrap_err();
                            let expected = if timed_out {
                                "acknowledgement timed out"
                            } else {
                                "recorded rejection"
                            };
                            assert!(error.contains(expected), "{error}");
                            assert!(
                                cut.try_recv().is_err(),
                                "an ambiguous resume failure must retain its reservation"
                            );
                            if timed_out {
                                std::fs::write(root.path().join("release-resume"), "").unwrap();
                            }
                        } else if reject_code.is_some() {
                            assert!(result.unwrap_err().contains("recorded rejection"));
                            tokio::time::timeout(Duration::from_secs(10), async {
                                loop {
                                    tokio::select! {
                                        cut = &mut *cut => {
                                            assert!(cut.unwrap().is_ok());
                                            break;
                                        }
                                        event = receive.recv() => {
                                            crate::worker_runtime::record_runtime_event(
                                                &relay, &mut in_flight, event.unwrap(),
                                            )
                                            .unwrap();
                                        }
                                    }
                                }
                            })
                            .await
                            .unwrap();
                        } else {
                            result.unwrap();
                            tokio::time::timeout(Duration::from_secs(10), async {
                                while !relay.observe().unwrap().turn_in_progress() {
                                    let event = receive.recv().await.unwrap();
                                    crate::worker_runtime::record_runtime_event(
                                        &relay,
                                        &mut in_flight,
                                        event,
                                    )
                                    .unwrap();
                                }
                            })
                            .await
                            .unwrap();
                            assert!(cut.try_recv().is_err());
                            relay
                                .lock()
                                .unwrap()
                                .record_observation(
                                    mj_core::relay::RelayObservation::HarnessTurnSettled {
                                        autonomous: None,
                                        origin: None,
                                        prompt_in_flight: false,
                                    },
                                )
                                .unwrap();
                            assert!(cut.try_recv().unwrap().is_ok());
                        }
                    } else {
                        result.unwrap();
                    }
                    assert_eq!(
                        context.lock().unwrap().state.answered_resume.as_deref(),
                        Some("explicit-1")
                    );
                }
            }
            if queued_prompt {
                tokio::time::timeout(Duration::from_secs(10), async {
                    loop {
                        match receive.recv().await.unwrap() {
                            RuntimeEvent::PromptFinished {
                                request_id,
                                stop_reason,
                                ..
                            } => {
                                assert_eq!(request_id, "queued-work");
                                assert_eq!(
                                    mj_core::state::classify_prompt_completion(&stop_reason),
                                    mj_core::state::PromptCompletion::Finished,
                                );
                                break;
                            }
                            RuntimeEvent::ElicitationRequested { .. } => {
                                panic!("ordinary reopen must not ask to resume the goal")
                            }
                            _ => {}
                        }
                    }
                })
                .await
                .unwrap();
            }
            tx.send(CommandRequest::Close {
                request_id: "close".into(),
            })
            .await
            .unwrap();
            loop {
                match tokio::time::timeout(Duration::from_secs(10), receive.recv())
                    .await
                    .unwrap()
                {
                    Some(RuntimeEvent::SessionUpdate { update }) => {
                        bytes += update
                            .pointer("/content/text")
                            .and_then(serde_json::Value::as_str)
                            .map_or(0, str::len);
                    }
                    Some(RuntimeEvent::CloseApplied { .. }) | None => break,
                    Some(RuntimeEvent::ElicitationRequested { .. }) => {
                        panic!("automatic recovery must not ask")
                    }
                    _ => {}
                }
            }
            drop(tx);
            while receive.recv().await.is_some() {}
            tokio::time::timeout(Duration::from_secs(10), task)
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            assert!(
                relay
                    .observe()
                    .unwrap()
                    .operator_away
                    .live_questions()
                    .is_empty()
            );
            let count = std::fs::read_to_string(root.path().join("resumes"))
                .unwrap_or_default()
                .lines()
                .count();
            assert_eq!(count, usize::from(decision || answer == Some(true)));
            if running {
                assert_eq!(bytes, 80000);
            }
        }
    }
    #[tokio::test]
    async fn goal_controls_work_during_prompts_and_pending_resume_with_large_output() {
        let _ = mj_harness_install::login_environment::initialize_from_parent();
        use mj_core::goal::GoalControlAction;
        for harness in [HarnessKind::Codex, HarnessKind::Claude] {
            for foreground in [false, true] {
                let root = tempfile::tempdir().unwrap();
                let script = root.path().join("controls.py");
                std::fs::write(&script, r#"
import json, os, sys
def emit(x): print(json.dumps(x), flush=True)
def result(i): emit({'jsonrpc':'2.0','id':i,'result':{}})
def update(x): emit({'jsonrpc':'2.0','method':'session/update','params':{'sessionId':'native','update':x}})
def goal(status): update({'sessionUpdate':'session_info_update','_meta':{'jetbrains':{'air':{'goal':None if status is None else {'objective':'finish','status':status,'createdAt':123,'tokensUsed':42,'tokenBudget':900}}}}})
def text(s): update({'sessionUpdate':'agent_message_chunk','content':{'type':'text','text':s}})
codex = os.environ['CODEX'] == 'true'
held = None
clears = 0
for line in sys.stdin:
    r=json.loads(line); m=r.get('method'); i=r.get('id'); p=r.get('params',{})
    if i is None: continue
    if m=='initialize':
        emit({'jsonrpc':'2.0','id':i,'result':{'protocolVersion':1,'agentCapabilities':{},'_meta':{'jetbrains':{'air':{'goal':{'version':1,'controlMethod':'_session/goal','actions':['pause','resume','clear'] if codex else ['set','clear']}}}}}})
    elif m=='session/new':
        emit({'jsonrpc':'2.0','id':i,'result':{'sessionId':'native','modes':{'currentModeId':'agent','availableModes':[{'id':'agent','name':'Guardian'},{'id':'auto','name':'Auto'}]},'_meta':{'jetbrains':{'air':{'goal':{'objective':'finish','status':'active','createdAt':123,'tokensUsed':42,'tokenBudget':900}}}}}})
    elif m=='session/prompt':
        assert p['prompt'][0]['text']=='work'
        text('prompt-started')
        # Leave the foreground prompt open until close.
    elif m=='_session/goal':
        assert 'expectedGoal' not in p
        action=p['action']
        if action=='resume':
            assert codex
            held=i
            goal('active')
            text('resume-pending')
        elif action=='pause':
            assert codex
            goal('paused')
            text('x'*100000)
            result(i)
        elif action=='clear':
            clears += 1
            if clears > 1:
                emit({'jsonrpc':'2.0','id':i,'error':{'code':-32603,'message':'control denied by test adapter'}})
                continue
            goal(None)
            text('x'*100000)
            result(i)
            if held is not None: result(held); held=None
        else: raise AssertionError(action)
    else: result(i)
"#).unwrap();
                let context = Arc::new(Mutex::new(mj_core::goal::GoalRecoveryContext::default()));
                let spec = LaunchSpec {
                    operator_away: Default::default(),
                    imported_logins: None,
                    login_identity: Default::default(),
                    bridge_spec_path: None,
                    subagent_policy: mj_core::subagent::SubagentPolicy::Native,
                    managed_subagent: false,
                    instruction_role: None,
                    subagent_mcp_socket: None,
                    clear_context_request: None,
                    context_restore: None,
                    goal_recovery: context.clone(),
                    command: "python3".into(),
                    args: vec![script.to_string_lossy().into_owned()],
                    environment: BTreeMap::from([(
                        "CODEX".into(),
                        (harness == HarnessKind::Codex).to_string(),
                    )]),
                    cwd: root.path().into(),
                    additional_directories: vec![],
                    project_memory: None,
                    extra_mcp_servers: vec![],
                    resume_session: None,
                    native_session_history: NativeSessionHistory::LoadBeforeReplace,
                    accepted_config: Default::default(),
                    initial_model: None,
                    harness,
                    execution_policy: ExecutionPolicy::ConfiguredApprovals,
                    approval_choices: Default::default(),
                    acp_activity: Default::default(),
                    step_clock: Default::default(),
                    tools_in_flight: Default::default(),
                    terminal_owner: None,
                    turn_context: Default::default(),
                    verdict: Some(crate::acp::VerdictSource::Disabled),
                    stall_policy: None,
                };
                let (tx, rx) = mpsc::channel(8);
                let (events, mut receive) = mpsc::channel(2);
                let task = mj_core::runtime::spawn(run(spec, rx, events));
                async fn next(rx: &mut mpsc::Receiver<RuntimeEvent>) -> RuntimeEvent {
                    tokio::time::timeout(Duration::from_secs(10), rx.recv())
                        .await
                        .unwrap()
                        .unwrap()
                }
                while !matches!(
                    next(&mut receive).await,
                    RuntimeEvent::SessionStarted { .. }
                ) {}
                assert!(
                    context
                        .lock()
                        .unwrap()
                        .state
                        .supports(GoalControlAction::Clear)
                );
                assert!(
                    context.lock().unwrap().state.active(),
                    "both adapters' session-open AIR goal snapshot is ingested"
                );
                if foreground {
                    tx.send(CommandRequest::Prompt {
                        request_id: "work".into(),
                        prompt: vec![ContentBlock::from("work")],
                    })
                    .await
                    .unwrap();
                    loop {
                        if let RuntimeEvent::SessionUpdate { update } = next(&mut receive).await
                            && update
                                .pointer("/content/text")
                                .and_then(serde_json::Value::as_str)
                                == Some("prompt-started")
                        {
                            break;
                        }
                    }
                }
                if harness == HarnessKind::Codex {
                    tx.send(CommandRequest::GoalControl {
                        request_id: "resume".into(),
                        action: GoalControlAction::Resume,
                    })
                    .await
                    .unwrap();
                    loop {
                        if let RuntimeEvent::SessionUpdate { update } = next(&mut receive).await
                            && update
                                .pointer("/content/text")
                                .and_then(serde_json::Value::as_str)
                                == Some("resume-pending")
                        {
                            break;
                        }
                    }
                    tx.send(CommandRequest::GoalControl {
                        request_id: "pause".into(),
                        action: GoalControlAction::Pause,
                    })
                    .await
                    .unwrap();
                } else {
                    tx.send(CommandRequest::GoalControl {
                        request_id: "unsupported".into(),
                        action: GoalControlAction::Pause,
                    })
                    .await
                    .unwrap();
                }
                let mut bytes = 0;
                loop {
                    match next(&mut receive).await {
                        RuntimeEvent::GoalControlApplied { request_id } => {
                            assert_eq!(request_id, "pause");
                            break;
                        }
                        RuntimeEvent::CommandRejected {
                            request_id,
                            message,
                            ..
                        } => {
                            assert_eq!(harness, HarnessKind::Claude);
                            assert_eq!(request_id, "unsupported");
                            assert!(message.contains("not supported"));
                            break;
                        }
                        RuntimeEvent::SessionUpdate { update } => {
                            bytes += update
                                .pointer("/content/text")
                                .and_then(serde_json::Value::as_str)
                                .map_or(0, str::len);
                        }
                        _ => {}
                    }
                }
                tx.send(CommandRequest::GoalControl {
                    request_id: "clear".into(),
                    action: GoalControlAction::Clear,
                })
                .await
                .unwrap();
                let mut completed = 0;
                while completed < if harness == HarnessKind::Codex { 2 } else { 1 } {
                    match next(&mut receive).await {
                        RuntimeEvent::GoalControlApplied { request_id } => {
                            assert!(matches!(request_id.as_str(), "clear" | "resume"));
                            completed += 1;
                        }
                        RuntimeEvent::SessionUpdate { update } => {
                            bytes += update
                                .pointer("/content/text")
                                .and_then(serde_json::Value::as_str)
                                .map_or(0, str::len);
                        }
                        RuntimeEvent::CommandRejected { message, .. } => panic!("{message}"),
                        _ => {}
                    }
                }
                assert_eq!(
                    bytes,
                    if harness == HarnessKind::Codex {
                        200000
                    } else {
                        100000
                    }
                );
                assert!(
                    context.lock().unwrap().state.snapshot.is_none(),
                    "late resume acknowledgement must not reactivate a cleared goal"
                );
                tx.send(CommandRequest::GoalControl {
                    request_id: "denied".into(),
                    action: GoalControlAction::Clear,
                })
                .await
                .unwrap();
                loop {
                    if let RuntimeEvent::CommandRejected {
                        request_id,
                        message,
                        ..
                    } = next(&mut receive).await
                    {
                        assert_eq!(request_id, "denied");
                        assert!(message.contains("control denied by test adapter"));
                        break;
                    }
                }
                if harness == HarnessKind::Codex {
                    tx.send(CommandRequest::GoalControl {
                        request_id: "unfinished-resume".into(),
                        action: GoalControlAction::Resume,
                    })
                    .await
                    .unwrap();
                    loop {
                        if let RuntimeEvent::SessionUpdate { update } = next(&mut receive).await
                            && update
                                .pointer("/content/text")
                                .and_then(serde_json::Value::as_str)
                                == Some("resume-pending")
                        {
                            break;
                        }
                    }
                    tx.send(CommandRequest::Cancel {
                        request_id: "cancel".into(),
                        steering_prompt: None,
                    })
                    .await
                    .unwrap();
                    while !matches!(next(&mut receive).await, RuntimeEvent::CancelApplied { .. }) {}
                }
                tx.send(CommandRequest::Close {
                    request_id: "close".into(),
                })
                .await
                .unwrap();
                while !matches!(next(&mut receive).await, RuntimeEvent::CloseApplied { .. }) {}
                drop(tx);
                while receive.recv().await.is_some() {}
                tokio::time::timeout(Duration::from_secs(10), task)
                    .await
                    .unwrap()
                    .unwrap()
                    .unwrap();
            }
        }
    }
}
