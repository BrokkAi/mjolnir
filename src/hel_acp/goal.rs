//! Goal recovery uses the same question surface as saved model recovery.
use super::*;
use crate::hel_goal::{GoalSnapshot, RECOVERY_ID};

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
        tokio::task::spawn_blocking(move || (journal.0)(persisted))
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

pub(super) struct Question {
    request: ElicitationRequest,
    goal: GoalSnapshot,
    intent: Option<String>,
}

async fn control(
    connection: &ConnectionTo<Agent>,
    session: &SessionId,
    goal: &GoalSnapshot,
    resume: bool,
) -> Result<()> {
    ensure!(
        goal.control_method.as_deref() == Some("_session/goal"),
        "goal resume is not supported by this harness"
    );
    let created_at = goal.created_at.context("goal identity is unavailable")?;
    tokio::time::timeout(Duration::from_secs(30), connection.send_request(UntypedMessage {
        method: "_session/goal".into(),
        params: serde_json::json!({"sessionId":session,"action":if resume { "resume" } else { "pause" }, "expectedGoal":{"objective":goal.objective,"createdAt":created_at}}),
    }).block_task()).await.context("goal resume acknowledgement timed out; current execution must be reconciled before retrying")??;
    Ok(())
}

pub(super) async fn recover(
    connection: &ConnectionTo<Agent>,
    session: &SessionId,
    spec: &LaunchSpec,
    events: &mpsc::Sender<RuntimeEvent>,
    model_question: bool,
) -> Result<Option<Question>> {
    let context = spec
        .goal_recovery
        .lock()
        .expect("goal lock poisoned")
        .clone();
    if spec.harness != HarnessKind::Codex || spec.resume_session.is_none() {
        return Ok(None);
    }
    // A crash can leave the prior question in the durable projection after
    // its decision was saved. Rebuild this session-owned question from intent.
    emit_runtime_event(
        events,
        RuntimeEvent::ElicitationResolved {
            elicitation_id: RECOVERY_ID.into(),
            action: "cancel".into(),
        },
    )
    .await?;
    if !context.state.synchronized() {
        emit_runtime_event(events, RuntimeEvent::Warning {
            message: "The adapter did not synchronize goal execution state; automatic goal recovery and worker replacement are deferred".into(),
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
            control(connection, session, &decision.goal, decision.resume).await?;
        }
        decision_update(spec, events, serde_json::json!({"mjGoalDecision":null})).await?;
        return Ok(None);
    }
    if context.state.decision.is_some() {
        // A new explicit open or model choice supersedes an older answer.
        decision_update(spec, events, serde_json::json!({"mjGoalDecision":null})).await?;
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
        control(connection, session, &goal, false).await?;
    }
    let mut explanation = None;
    if !context.asking() && !model_question {
        if !goal.active() || context.state.running() {
            return Ok(None);
        }
        match control(connection, session, &goal, true).await {
            Ok(()) => return Ok(None),
            Err(error) => {
                let message = format!("Goal recovery needs attention: {error:#}");
                emit_runtime_event(
                    events,
                    RuntimeEvent::Warning {
                        message: message.clone(),
                    },
                )
                .await?;
                explanation = Some(message);
            }
        }
    }
    let request = ElicitationRequest {
        id: RECOVERY_ID.into(),
        title: Some("Resume this goal?".into()),
        message: goal.objective.clone(),
        description: Some(explanation.unwrap_or_else(|| {
            "Resume the existing goal with its remaining budget, or keep it paused.".into()
        })),
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
    };
    emit_runtime_event(
        events,
        RuntimeEvent::ElicitationRequested {
            request: request.clone(),
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
    let decision = current.then(|| crate::hel_goal::GoalDecision {
        goal: question.goal.clone(),
        resume,
    });
    let mut meta = serde_json::json!({"mjGoalDecision":decision});
    if let Some(intent) = &question.intent {
        meta["mjGoalResumeAnswered"] = intent.clone().into();
    }
    decision_update(spec, events, meta).await?;
    if current {
        if let Err(error) = control(connection, session, &question.goal, resume).await {
            return Ok(Some(Err(format!("{error:#}"))));
        }
        decision_update(spec, events, serde_json::json!({"mjGoalDecision":null})).await?;
    }
    *pending = None;
    emit_runtime_event(
        events,
        RuntimeEvent::ElicitationResolved {
            elicitation_id: id.into(),
            action: response.action_name().into(),
        },
    )
    .await?;
    Ok(Some(Ok(())))
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[tokio::test]
    async fn goal_recovery_resumes_only_when_needed_and_explicit_open_asks() {
        for (asking, running, answer, expected_resumes, status, decision) in [
            (false, false, None, 1, "active", false),
            (false, true, None, 0, "active", false),
            (true, false, Some(true), 1, "active", false),
            (true, false, Some(false), 0, "active", false),
            (false, false, None, 0, "paused", false),
            (false, false, None, 0, "blocked", false),
            (false, false, None, 0, "limited", false),
            (false, false, None, 0, "complete", false),
            (true, false, None, 0, "complete", false),
            (false, false, None, 1, "paused", true),
        ] {
            let root = tempfile::tempdir().unwrap();
            let script = root.path().join("goal.py");
            std::fs::write(&script, r#"
import json, os, sys
asking = os.environ['ASKING'] == 'true'
running = os.environ['RUNNING'] == 'true'
def emit(x): print(json.dumps(x), flush=True)
def update(meta): emit({'jsonrpc':'2.0','method':'session/update','params':{'sessionId':'native','update':{'sessionUpdate':'session_info_update','_meta':meta}}})
goal = {'objective':'finish campaign','status':'active','createdAt':123,'tokenBudget':900,'tokensUsed':100,'controlMethod':'_session/goal'}
for line in sys.stdin:
    r = json.loads(line); method=r.get('method'); ident=r.get('id'); p=r.get('params',{})
    if ident is None: continue
    if method == 'initialize':
        result={'protocolVersion':1,'agentCapabilities':{'sessionCapabilities':{'resume':{}}},'_meta':{'goal':{'version':1,'controlMethod':'_session/goal','actions':['resume','pause'],'resumePolicies':['pause','preserve']},'execution':{'version':1}}}
    elif method == 'session/resume':
        assert p['_meta']['goal']['resumePolicy'] == ('pause' if asking else 'preserve')
        goal['status']='paused' if asking and os.environ['GOAL_STATUS']=='active' else os.environ['GOAL_STATUS']
        result={'modes':{'currentModeId':'agent','availableModes':[{'id':'agent','name':'Guardian'}]},'_meta':{'goal':goal,'execution':{'version':1,'status':'running' if running else 'idle','turnId':'native-turn' if running else None}}}
        # Live output is legal before the resume response and exceeds a pipe buffer.
        if running:
            emit({'jsonrpc':'2.0','method':'session/update','params':{'sessionId':'native','update':{'sessionUpdate':'agent_message_chunk','content':{'type':'text','text':'x'*80000}}}})
    elif method == '_session/goal':
        if p['action']=='resume':
            assert p['expectedGoal']=={'objective':'finish campaign','createdAt':123}
            with open('resumes','a') as f: f.write('resume\n')
            goal['status']='active'; update({'goal':goal,'execution':{'version':1,'status':'running','turnId':'resumed'}})
        else:
            goal['status']='paused'; update({'goal':goal})
        result={}
    else: result={}
    emit({'jsonrpc':'2.0','id':ident,'result':result})
"#).unwrap();
            let context = Arc::new(Mutex::new(crate::hel_goal::GoalRecoveryContext {
                request: asking.then(|| "explicit-1".into()),
                state: crate::hel_goal::GoalState {
                    decision: decision.then(|| crate::hel_goal::GoalDecision {
                        goal: serde_json::from_value(serde_json::json!({"objective":"finish campaign","status":"paused","createdAt":123,"controlMethod":"_session/goal"})).unwrap(), resume: true,
                    }),
                    ..Default::default()
                },
                ..Default::default()
            }));
            let spec = LaunchSpec {
                goal_recovery: context.clone(),
                command: "python3".into(),
                args: vec![script.to_string_lossy().into_owned()],
                environment: BTreeMap::from([
                    ("ASKING".into(), asking.to_string()),
                    ("RUNNING".into(), running.to_string()),
                    ("GOAL_STATUS".into(), status.into()),
                ]),
                cwd: root.path().into(),
                additional_directories: vec![],
                project_memory: None,
                extra_mcp_servers: vec![],
                resume_session: Some("native".into()),
                accepted_config: Default::default(),
                harness: HarnessKind::Codex,
                execution_policy: ExecutionPolicy::ConfiguredApprovals,
                acp_activity: Default::default(),
                step_clock: Default::default(),
            };
            let (tx, rx) = mpsc::channel(8);
            let (events, mut receive) = mpsc::channel(8);
            let task = tokio::spawn(run(spec, rx, events));
            let mut bytes = 0;
            if let Some(accept) = answer {
                loop {
                    let event = tokio::time::timeout(Duration::from_secs(10), receive.recv())
                        .await
                        .unwrap()
                        .unwrap();
                    if let RuntimeEvent::ElicitationRequested { request } = event {
                        assert_eq!(request.id, RECOVERY_ID);
                        assert!(!root.path().join("resumes").exists());
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
                        let (resolved, ack) = oneshot::channel();
                        tx.send(CommandRequest::ResolveElicitation {
                            elicitation_id: RECOVERY_ID.into(),
                            response,
                            resolved,
                        })
                        .await
                        .unwrap();
                        // Continue draining while the control response is in flight.
                        let ack = async { ack.await.unwrap().unwrap() };
                        tokio::pin!(ack);
                        loop {
                            tokio::select! { _ = &mut ack => break, event = receive.recv() => { assert!(event.is_some()); } }
                        }
                        assert_eq!(
                            context.lock().unwrap().state.answered_resume.as_deref(),
                            Some("explicit-1")
                        );
                        break;
                    }
                }
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
            let count = std::fs::read_to_string(root.path().join("resumes"))
                .unwrap_or_default()
                .lines()
                .count();
            assert_eq!(count, expected_resumes);
            if running {
                assert_eq!(bytes, 80000);
            }
        }
    }
}
