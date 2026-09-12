//! Exercise real ACP startup against a harness that forgets all selectors on load.

use super::*;
use crate::hel_worker::{DurableRelay, RelayCommand, RelayCommandOutcome, RelayObservation};
use agent_client_protocol::schema::v1::TextContent;

fn reset_on_load_harness(root: &std::path::Path) -> PathBuf {
    let script = root.join("reset_config.py");
    std::fs::write(
        &script,
        r#"
import json, os, sys
model, effort = 'default', 'low'
def options():
    efforts = ['medium', 'high'] if model == 'chosen' else ['low']
    return [
      {'id':'model_id','name':'Model','category':'model','type':'select',
       'currentValue':model,'options':[{'value':x,'name':x} for x in ['default','chosen']]},
      {'id':'thinking','name':'Thinking','category':'thought_level','type':'select',
       'currentValue':effort,'options':[{'value':x,'name':x} for x in efforts]}]
for line in sys.stdin:
    request = json.loads(line)
    method, ident = request.get('method'), request.get('id')
    if ident is None: continue
    params = request.get('params', {})
    if method == 'initialize': result = {'protocolVersion':1}
    elif method in ('session/new','session/load'):
        result = {'sessionId':'native','configOptions':options()}
    elif method == 'session/set_config_option':
        key, value = params['configId'], params['value']
        if key == 'model_id':
            model = value
            effort = 'high' if model == 'chosen' else 'low'
        elif key == 'thinking':
            assert model == 'chosen', 'effort was applied before model'
            assert value in ['medium','high']
            effort = value
        else: raise AssertionError(key)
        result = {'configOptions':options()}
    elif method == 'session/prompt':
        text = params['prompt'][0]['text']
        if text == 'restart': os._exit(0)
        assert (model,effort) == ('chosen','medium'), (model,effort)
        result = {'stopReason':'end_turn'}
    else: result = {}
    print(json.dumps({'jsonrpc':'2.0','id':ident,'result':result}), flush=True)
"#,
    )
    .unwrap();
    script
}

/// A harness whose model catalogue never contains the value this session
/// saved, and which rejects one model it does list.
fn dropped_model_harness(root: &std::path::Path) -> PathBuf {
    let script = root.join("dropped_model.py");
    std::fs::write(
        &script,
        r#"
import json, sys
MODELS = ['default', 'chosen', 'broken']
model, effort = 'default', 'low'
def efforts():
    return ['medium', 'high'] if model == 'chosen' else ['low']
def options():
    return [
      {'id':'model_id','name':'Model','category':'model','type':'select',
       'currentValue':model,'options':[{'value':x,'name':x.title()} for x in MODELS]},
      {'id':'thinking','name':'Thinking','category':'thought_level','type':'select',
       'currentValue':effort,'options':[{'value':x,'name':x} for x in efforts()]}]
for line in sys.stdin:
    request = json.loads(line)
    method, ident = request.get('method'), request.get('id')
    if ident is None: continue
    params, error, result = request.get('params', {}), None, {}
    if method == 'initialize': result = {'protocolVersion':1}
    elif method in ('session/new','session/load'):
        result = {'sessionId':'native','configOptions':options()}
    elif method == 'session/set_config_option':
        key, value = params['configId'], params['value']
        if key == 'model_id' and value == 'broken':
            error = {'code':-32603,'message':'this model is listed but unusable'}
        elif key == 'model_id':
            model = value
            effort = efforts()[0]
            result = {'configOptions':options()}
        elif key == 'thinking':
            assert value in efforts(), value
            effort = value
            result = {'configOptions':options()}
        else: raise AssertionError(key)
    elif method == 'session/prompt': result = {'stopReason':'end_turn'}
    reply = {'jsonrpc':'2.0','id':ident}
    reply['error' if error else 'result'] = error or result
    print(json.dumps(reply), flush=True)
"#,
    )
    .unwrap();
    script
}

fn launch(root: &std::path::Path, script: PathBuf, saved: AcceptedSessionConfig) -> LaunchSpec {
    LaunchSpec {
        command: "python3".into(),
        args: vec![script.to_string_lossy().into_owned()],
        environment: BTreeMap::new(),
        cwd: root.to_owned(),
        additional_directories: Vec::new(),
        project_memory: None,
        extra_mcp_servers: Vec::new(),
        resume_session: Some("native".into()),
        accepted_config: Arc::new(Mutex::new(saved)),
        harness: HarnessKind::Kimi,
        execution_policy: ExecutionPolicy::ConfiguredApprovals,
        acp_activity: AcpActivityClock::default(),
        step_clock: StepClock::default(),
    }
}

async fn next(events: &mut mpsc::Receiver<RuntimeEvent>) -> RuntimeEvent {
    tokio::time::timeout(Duration::from_secs(10), events.recv())
        .await
        .expect("the fake adapter must make progress")
        .expect("runtime remains alive")
}

async fn configured(events: &mut mpsc::Receiver<RuntimeEvent>) {
    loop {
        match next(events).await {
            RuntimeEvent::SessionConfigured { .. } => return,
            RuntimeEvent::Stopped => panic!("adapter stopped before configuration completed"),
            _ => {}
        }
    }
}

async fn prompt(commands: &mpsc::Sender<CommandRequest>, text: &str) {
    commands
        .send(CommandRequest::Prompt {
            request_id: text.into(),
            prompt: vec![ContentBlock::Text(TextContent::new(text))],
        })
        .await
        .unwrap();
}

#[tokio::test]
async fn accepted_selectors_survive_bridge_and_worker_restarts_before_the_next_prompt() {
    let root = tempfile::tempdir().unwrap();
    let script = reset_on_load_harness(root.path());
    let journal = root.path().join("relay");
    let mut relay =
        DurableRelay::open(&journal, "0123456789abcdef0123456789abcdef", "test").unwrap();
    let (commands, requests) = mpsc::channel(8);
    let (events_tx, mut events) = mpsc::channel(64);
    let spec = launch(
        root.path(),
        script.clone(),
        AcceptedSessionConfig::default(),
    );
    let saved = spec.accepted_config.clone();
    let runtime = tokio::spawn(run(spec, requests, events_tx));
    configured(&mut events).await;
    for (index, (key, value)) in [
        ("model", "default"),
        ("model_id", "chosen"),
        ("effort", "high"),
        ("thinking", "medium"),
    ]
    .into_iter()
    .enumerate()
    {
        let request_id = format!("config-{index}");
        crate::hel_worker::test_support::submit_relay(
            &mut relay,
            &request_id,
            RelayCommand::SetConfig {
                key: key.into(),
                value: value.into(),
            },
        );
        assert_eq!(relay.claim_pending_commands(true).unwrap().len(), 1);
        commands
            .send(CommandRequest::SetConfig {
                request_id,
                key: key.into(),
                value: value.into(),
            })
            .await
            .unwrap();
        loop {
            match next(&mut events).await {
                RuntimeEvent::ConfigApplied {
                    request_id,
                    key,
                    value,
                    config_options,
                } => {
                    // These are the same accepted observations the worker
                    // records, including adapter-specific selector ids.
                    relay
                        .record_observation(RelayObservation::ConfigurationUpdated { key, value })
                        .unwrap();
                    relay
                        .record_observation(RelayObservation::SessionConfigured { config_options })
                        .unwrap();
                    relay
                        .record_command_completed(&request_id, RelayCommandOutcome::Configured)
                        .unwrap();
                    break;
                }
                RuntimeEvent::CommandRejected { message, .. } => panic!("{message}"),
                _ => {}
            }
        }
    }
    let accepted = saved.lock().unwrap().clone();
    commands
        .send(CommandRequest::SetConfig {
            request_id: "rejected".into(),
            key: "model".into(),
            value: "unavailable".into(),
        })
        .await
        .unwrap();
    loop {
        if matches!(next(&mut events).await, RuntimeEvent::CommandRejected { request_id, .. } if request_id == "rejected")
        {
            break;
        }
    }
    assert_eq!(
        *saved.lock().unwrap(),
        accepted,
        "a rejected change must not replace accepted choices"
    );

    prompt(&commands, "restart").await;
    configured(&mut events).await;
    prompt(&commands, "check-after-bridge").await;
    loop {
        if matches!(next(&mut events).await, RuntimeEvent::PromptFinished { request_id, .. } if request_id == "check-after-bridge")
        {
            break;
        }
    }
    drop(commands);
    tokio::time::timeout(Duration::from_secs(10), runtime)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    drop(relay);

    let relay = DurableRelay::open(&journal, "0123456789abcdef0123456789abcdef", "test").unwrap();
    let state = relay.operational_state();
    let restored = AcceptedSessionConfig::from_configuration(&state.config, &state.config_options);
    assert_eq!(
        restored, accepted,
        "worker startup recovers accepted selectors from its journal"
    );
    let (commands, requests) = mpsc::channel(8);
    let (events_tx, mut events) = mpsc::channel(64);
    // Queue before startup. The fake fails if this prompt sees profile defaults.
    prompt(&commands, "check-after-worker").await;
    let runtime = tokio::spawn(run(
        launch(root.path(), script, restored),
        requests,
        events_tx,
    ));
    let mut ready = false;
    loop {
        match next(&mut events).await {
            RuntimeEvent::SessionConfigured { .. } => ready = true,
            RuntimeEvent::PromptFinished { .. } => {
                assert!(ready);
                break;
            }
            RuntimeEvent::Stopped => panic!("restored worker must run the queued prompt"),
            _ => {}
        }
    }
    drop(commands);
    tokio::time::timeout(Duration::from_secs(10), runtime)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn a_saved_selector_the_harness_refuses_fails_before_ready_or_prompt_delivery() {
    let root = tempfile::tempdir().unwrap();
    let spec = launch(
        root.path(),
        dropped_model_harness(root.path()),
        AcceptedSessionConfig {
            // Listed by the harness, and rejected when it is selected. That
            // is a real failure, not a withdrawn model, so startup keeps it.
            model: Some("broken".into()),
            effort: None,
        },
    );
    let (commands, requests) = mpsc::channel(8);
    let (events_tx, mut events) = mpsc::channel(64);
    prompt(&commands, "must-not-run").await;
    let error = tokio::time::timeout(Duration::from_secs(10), run(spec, requests, events_tx))
        .await
        .unwrap()
        .unwrap_err();
    assert!(
        format!("{error:#}").contains("restore this session's accepted model"),
        "{error:#}"
    );
    while let Some(event) = events.recv().await {
        assert!(!matches!(
            event,
            RuntimeEvent::SessionConfigured { .. } | RuntimeEvent::PromptFinished { .. }
        ));
    }
}

/// Drive startup until it raises the recovery question, checking on the way
/// that it warned about the dropped value and reached a configured session.
async fn recovery_question(events: &mut mpsc::Receiver<RuntimeEvent>) -> ElicitationRequest {
    let mut warned = false;
    let mut configured = false;
    loop {
        match next(events).await {
            RuntimeEvent::Warning { message } => {
                assert!(message.contains("withdrawn-model"), "{message}");
                warned = true;
            }
            RuntimeEvent::SessionConfigured { .. } => configured = true,
            RuntimeEvent::ElicitationRequested { request } => {
                assert!(warned, "the dropped value must reach the transcript");
                assert!(configured, "the session must be usable before it is asked");
                return request;
            }
            RuntimeEvent::Stopped => panic!("startup must survive a withdrawn model"),
            _ => {}
        }
    }
}

async fn answer_recovery(
    commands: &mpsc::Sender<CommandRequest>,
    elicitation_id: &str,
    response: ElicitationResponse,
) {
    let (resolved, resolution) = oneshot::channel();
    commands
        .send(CommandRequest::ResolveElicitation {
            elicitation_id: elicitation_id.to_owned(),
            response,
            resolved,
        })
        .await
        .unwrap();
    resolution.await.unwrap().unwrap();
}

#[tokio::test]
async fn a_withdrawn_saved_model_starts_on_the_default_and_its_replacement_survives_a_restart() {
    let root = tempfile::tempdir().unwrap();
    let script = dropped_model_harness(root.path());
    let journal = root.path().join("relay");
    let mut relay =
        DurableRelay::open(&journal, "0123456789abcdef0123456789abcdef", "test").unwrap();
    let spec = launch(
        root.path(),
        script.clone(),
        AcceptedSessionConfig {
            model: Some("withdrawn-model".into()),
            effort: None,
        },
    );
    let saved = spec.accepted_config.clone();
    let (commands, requests) = mpsc::channel(8);
    let (events_tx, mut events) = mpsc::channel(64);
    let runtime = tokio::spawn(run(spec, requests, events_tx));

    let request = recovery_question(&mut events).await;
    assert_eq!(request.id, SESSION_CONFIG_RECOVERY_ID);
    let [field] = request.fields.as_slice() else {
        panic!(
            "one dropped selector asks one question: {:?}",
            request.fields
        );
    };
    assert_eq!(field.id, "model");
    let ElicitationFieldKind::SingleSelect { options, default } = &field.kind else {
        panic!("a model choice is a single select");
    };
    assert_eq!(
        options
            .iter()
            .map(|option| option.value.as_str())
            .collect::<Vec<_>>(),
        ["default", "chosen", "broken"],
        "the question carries what the harness offers now"
    );
    assert_eq!(default.as_deref(), Some("default"));

    answer_recovery(
        &commands,
        &request.id,
        ElicitationResponse::Accept {
            content: BTreeMap::from([(
                "model".to_owned(),
                ElicitationValue::String("chosen".into()),
            )]),
        },
    )
    .await;
    let mut resolved = false;
    loop {
        match next(&mut events).await {
            RuntimeEvent::ElicitationResolved {
                elicitation_id,
                action,
            } => {
                assert_eq!(elicitation_id, SESSION_CONFIG_RECOVERY_ID);
                assert_eq!(action, "accept");
                resolved = true;
            }
            RuntimeEvent::ConfigApplied {
                request_id,
                key,
                value,
                config_options,
            } => {
                assert!(
                    request_id.is_empty(),
                    "a recovered selector completes no relay command"
                );
                assert_eq!((key.as_str(), value.as_str()), ("model", "chosen"));
                // The same observations the worker records for a change no
                // relay command asked for.
                relay
                    .record_observation(RelayObservation::SessionConfigured { config_options })
                    .unwrap();
                relay
                    .record_observation(RelayObservation::ConfigurationUpdated { key, value })
                    .unwrap();
                break;
            }
            RuntimeEvent::Stopped => panic!("answering must not stop the session"),
            _ => {}
        }
    }
    assert!(resolved);
    let accepted = saved.lock().unwrap().clone();
    assert_eq!(
        accepted,
        AcceptedSessionConfig {
            model: Some("chosen".into()),
            effort: Some("medium".into())
        },
        "the chosen model brings its own effort catalogue with it"
    );
    drop(commands);
    tokio::time::timeout(Duration::from_secs(10), runtime)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    drop(relay);

    let relay = DurableRelay::open(&journal, "0123456789abcdef0123456789abcdef", "test").unwrap();
    let state = relay.operational_state();
    let restored = AcceptedSessionConfig::from_configuration(&state.config, &state.config_options);
    assert_eq!(restored, accepted, "the answer must outlive the worker");

    let (commands, requests) = mpsc::channel(8);
    let (events_tx, mut events) = mpsc::channel(64);
    prompt(&commands, "after-recovery").await;
    let runtime = tokio::spawn(run(
        launch(root.path(), script, restored),
        requests,
        events_tx,
    ));
    loop {
        match next(&mut events).await {
            RuntimeEvent::ElicitationRequested { .. } => {
                panic!("the withdrawn model must not be replayed and asked about again")
            }
            RuntimeEvent::PromptFinished { .. } => break,
            RuntimeEvent::Stopped => panic!("the recovered session must run its prompt"),
            _ => {}
        }
    }
    drop(commands);
    tokio::time::timeout(Duration::from_secs(10), runtime)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn declining_the_recovery_question_keeps_the_harness_default_and_clears_the_question() {
    let root = tempfile::tempdir().unwrap();
    let spec = launch(
        root.path(),
        dropped_model_harness(root.path()),
        AcceptedSessionConfig {
            model: Some("withdrawn-model".into()),
            effort: None,
        },
    );
    let saved = spec.accepted_config.clone();
    let (commands, requests) = mpsc::channel(8);
    let (events_tx, mut events) = mpsc::channel(64);
    let runtime = tokio::spawn(run(spec, requests, events_tx));

    let request = recovery_question(&mut events).await;
    answer_recovery(&commands, &request.id, ElicitationResponse::Decline).await;
    let mut resolved = false;
    prompt(&commands, "after-decline").await;
    loop {
        match next(&mut events).await {
            RuntimeEvent::ElicitationResolved {
                elicitation_id,
                action,
            } => {
                assert_eq!(elicitation_id, SESSION_CONFIG_RECOVERY_ID);
                assert_eq!(action, "decline");
                resolved = true;
            }
            RuntimeEvent::ConfigApplied { .. } => panic!("declining changes nothing"),
            RuntimeEvent::PromptFinished { .. } => break,
            RuntimeEvent::Stopped => panic!("declining must leave the session usable"),
            _ => {}
        }
    }
    assert!(
        resolved,
        "the question must stop asking once it is answered"
    );
    assert_eq!(
        saved.lock().unwrap().model.as_deref(),
        Some("withdrawn-model"),
        "declining defers the choice rather than discarding what was stored"
    );
    drop(commands);
    tokio::time::timeout(Duration::from_secs(10), runtime)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
}

#[test]
fn permission_and_plan_configuration_are_not_restored_as_session_selectors() {
    let saved = AcceptedSessionConfig::from_configuration(
        &BTreeMap::from([
            ("mode".into(), "bypassPermissions".into()),
            ("collaboration_mode".into(), "plan".into()),
        ]),
        &[],
    );
    assert_eq!(saved, AcceptedSessionConfig::default());
}

#[test]
fn a_completed_model_change_keeps_its_new_effort_and_clears_an_absent_selector() {
    let mut values = BTreeMap::from([
        ("model".into(), "old".into()),
        ("effort".into(), "xhigh".into()),
    ]);
    let mut options: Vec<SessionConfigOption> = serde_json::from_value(serde_json::json!([
        {"id":"model_id", "name":"Model", "category":"model", "type":"select",
         "currentValue":"new", "options":[{"value":"new", "name":"New"}]},
        {"id":"thinking", "name":"Effort", "category":"thought_level", "type":"select",
         "currentValue":"low", "options":[{"value":"low", "name":"Low"}]}
    ]))
    .unwrap();
    AcceptedSessionConfig::record_completed(&mut values, "model_id", "new", &options);
    assert_eq!(
        AcceptedSessionConfig::from_configuration(&values, &options),
        AcceptedSessionConfig {
            model: Some("new".into()),
            effort: Some("low".into())
        }
    );
    options.pop();
    AcceptedSessionConfig::record_completed(&mut values, "model_id", "new", &options);
    assert_eq!(
        AcceptedSessionConfig::from_configuration(&values, &options),
        AcceptedSessionConfig {
            model: Some("new".into()),
            effort: None
        }
    );
}
