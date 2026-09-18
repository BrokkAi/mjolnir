//! Exercise real ACP startup against a harness that forgets all selectors on load.

use super::*;
use crate::relay::{DurableRelay, RelayCommand, RelayCommandOutcome, RelayObservation};
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
        result = {'sessionId':'native','configOptions':options(),
                  'modes':{'currentModeId':'default','availableModes':[
                      {'id':'default','name':'Default'},{'id':'auto','name':'Auto'}]}}
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
        print(json.dumps({'jsonrpc':'2.0','method':'session/update','params':{'sessionId':'native','update':{'sessionUpdate':'agent_message_chunk','content':{'type':'text','text':'ok'}}}}), flush=True)
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
    elif method == 'session/prompt':
        # A real harness answers; a turn with no output at all is
        # reported as unanswered (#970).
        print(json.dumps({'jsonrpc':'2.0','method':'session/update','params':{'sessionId':'native','update':{'sessionUpdate':'agent_message_chunk','content':{'type':'text','text':'ok'}}}}), flush=True)
        result = {'stopReason':'end_turn'}
    reply = {'jsonrpc':'2.0','id':ident}
    reply['error' if error else 'result'] = error or result
    print(json.dumps(reply), flush=True)
"#,
    )
    .unwrap();
    script
}

/// A harness that answers a successful model change with the configuration it
/// held before the change, the way Codex and Kimi do, and that caps an effort
/// request to a lower value it reports straight away.
fn stale_answer_harness(root: &std::path::Path) -> PathBuf {
    let script = root.join("stale_answer.py");
    std::fs::write(
        &script,
        r#"
import json, sys
model, effort = 'default', 'low'
def options():
    return [
      {'id':'model_id','name':'Model','category':'model','type':'select',
       'currentValue':model,'options':[{'value':x,'name':x} for x in ['default','chosen']]},
      {'id':'thinking','name':'Thinking','category':'thought_level','type':'select',
       'currentValue':effort,'options':[{'value':x,'name':x} for x in ['low','high','max']]}]
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
            # Applied, but answered with the configuration from before it.
            answer = options()
            model = value
        elif key == 'thinking':
            effort = 'high' if value == 'max' else value
            answer = options()
        else: raise AssertionError(key)
        result = {'configOptions':answer}
    elif method == 'session/prompt':
        # A real harness answers; a turn with no output at all is
        # reported as unanswered (#970).
        print(json.dumps({'jsonrpc':'2.0','method':'session/update','params':{'sessionId':'native','update':{'sessionUpdate':'agent_message_chunk','content':{'type':'text','text':'ok'}}}}), flush=True)
        result = {'stopReason':'end_turn'}
    else: result = {}
    print(json.dumps({'jsonrpc':'2.0','id':ident,'result':result}), flush=True)
"#,
    )
    .unwrap();
    script
}

fn launch(root: &std::path::Path, script: PathBuf, saved: AcceptedSessionConfig) -> LaunchSpec {
    LaunchSpec {
        bridge_spec_path: None,
        subagent_mcp_socket: None,
        goal_recovery: Default::default(),
        command: "python3".into(),
        args: vec![script.to_string_lossy().into_owned()],
        environment: BTreeMap::new(),
        cwd: root.to_owned(),
        additional_directories: Vec::new(),
        project_memory: None,
        extra_mcp_servers: Vec::new(),
        resume_session: Some("native".into()),
        native_session_may_have_history: false,
        accepted_config: Arc::new(Mutex::new(saved)),
        harness: HarnessKind::Kimi,
        execution_policy: ExecutionPolicy::ConfiguredApprovals,
        acp_activity: AcpActivityClock::default(),
        step_clock: StepClock::default(),
        tools_in_flight: Default::default(),
        stall_policy: None,
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
        crate::relay::test_support::submit_relay(
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
                assert!(message.contains("Could not restore"), "{message}");
                assert!(
                    message.contains("currently reports model \"default\""),
                    "{message}"
                );
                assert!(!message.contains("harness default"), "{message}");
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

/// Send one selector change and return the configuration the worker reports
/// with the `ConfigApplied` event that answers it.
async fn set_config(
    commands: &mpsc::Sender<CommandRequest>,
    events: &mut mpsc::Receiver<RuntimeEvent>,
    key: &str,
    value: &str,
) -> Vec<SessionConfigOption> {
    commands
        .send(CommandRequest::SetConfig {
            request_id: format!("{key}-{value}"),
            key: key.into(),
            value: value.into(),
        })
        .await
        .unwrap();
    loop {
        match next(events).await {
            RuntimeEvent::ConfigApplied { config_options, .. } => return config_options,
            RuntimeEvent::CommandRejected { message, .. } => panic!("{message}"),
            RuntimeEvent::Stopped => panic!("the session must survive a selector change"),
            _ => {}
        }
    }
}

fn reported(options: &[SessionConfigOption], key: &str) -> String {
    let option = find_session_config_option(options, key).expect("the selector stays advertised");
    let SessionConfigKind::Select(select) = &option.kind else {
        panic!("{key} is a select");
    };
    select.current_value.to_string()
}

#[tokio::test]
async fn a_change_the_harness_answers_with_its_old_configuration_is_reported_as_applied() {
    let root = tempfile::tempdir().unwrap();
    let spec = launch(
        root.path(),
        stale_answer_harness(root.path()),
        AcceptedSessionConfig::default(),
    );
    let (commands, requests) = mpsc::channel(8);
    let (events_tx, mut events) = mpsc::channel(64);
    let runtime = tokio::spawn(run(spec, requests, events_tx));
    configured(&mut events).await;

    let options = set_config(&commands, &mut events, "model", "chosen").await;
    assert_eq!(
        reported(&options, "model"),
        "chosen",
        "a successful change is reported as applied, not as the value it replaced"
    );

    // The harness answered this one with a value of its own choosing. That is a
    // real mismatch and it has to stay visible.
    let options = set_config(&commands, &mut events, "effort", "max").await;
    assert_eq!(reported(&options, "effort"), "high");

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

/// Model menus under Claude setup-token auth retain the 1M alias only when
/// the native process receives the accepted model before resuming history.
#[tokio::test]
async fn claude_resume_pins_the_saved_model_before_catalogue_and_queued_prompt() {
    let root = tempfile::tempdir().unwrap();
    let script = reset_on_load_harness(root.path());
    let source = std::fs::read_to_string(&script).unwrap()
        .replace("'chosen'", "'opus[1m]'")
        .replace("model, effort = 'default', 'low'", "model, effort = 'default', 'low'\npinned = False")
        .replace("['default','opus[1m]']", "(['default','opus[1m]'] if pinned else ['default','opus'])")
        .replace("('session/new','session/load'):", "('session/new','session/load'):\n        pinned = params.get('_meta',{}).get('claudeCode',{}).get('options',{}).get('model') == 'opus[1m]'");
    std::fs::write(&script, source).unwrap();
    let mut spec = launch(
        root.path(),
        script,
        AcceptedSessionConfig {
            model: Some("opus[1m]".into()),
            effort: Some("medium".into()),
        },
    );
    spec.harness = HarnessKind::Claude;
    let (commands, requests) = mpsc::channel(8);
    let (events_tx, mut events) = mpsc::channel(64);
    prompt(&commands, "queued-before-startup").await;
    let runtime = tokio::spawn(run(spec, requests, events_tx));
    for turn in 0..3 {
        let mut ready = false;
        loop {
            let event = next(&mut events).await;
            match event {
                RuntimeEvent::SessionConfigured { config_options } => {
                    let option = find_session_config_option(&config_options, "model").unwrap();
                    let SessionConfigKind::Select(select) = &option.kind else {
                        panic!("select")
                    };
                    assert_eq!(select.current_value.to_string(), "opus[1m]");
                    ready = true;
                    if turn > 0 {
                        prompt(&commands, "after-restart").await;
                    }
                }
                RuntimeEvent::PromptFinished { stop_reason, .. } if stop_reason == "EndTurn" => {
                    assert!(ready, "prompts wait for restored model and effort");
                    break;
                }
                RuntimeEvent::ElicitationRequested { request } => {
                    panic!("unexpected recovery: {request:?}")
                }
                RuntimeEvent::Stopped => panic!("Claude startup failed"),
                _ => {}
            }
        }
        if turn < 2 {
            prompt(&commands, "restart").await;
        }
    }
    drop(commands);
    tokio::time::timeout(Duration::from_secs(10), runtime)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
}

#[test]
fn claude_session_requests_read_the_latest_accepted_model() {
    let mut spec = launch(
        std::path::Path::new("/workspace"),
        PathBuf::from("adapter"),
        AcceptedSessionConfig::default(),
    );
    spec.harness = HarnessKind::Claude;
    for model in [None, Some("opus[1m]"), Some("opus")] {
        spec.accepted_config.lock().unwrap().model = model.map(str::to_owned);
        for request in [
            serde_json::to_value(new_session_request(&spec, true)).unwrap(),
            serde_json::to_value(load_session_request(&spec, "native".into())).unwrap(),
            serde_json::to_value(resume_session_request(&spec, "native".into())).unwrap(),
        ] {
            assert_eq!(
                request
                    .pointer("/_meta/claudeCode/options/model")
                    .and_then(serde_json::Value::as_str),
                model
            );
        }
    }
}

#[tokio::test]
async fn claude_startup_errors_are_not_reported_as_model_replacement() {
    for cause in [
        "authentication expired",
        "requested startup model is unavailable",
    ] {
        let root = tempfile::tempdir().unwrap();
        let script = dropped_model_harness(root.path());
        let source = std::fs::read_to_string(&script).unwrap().replace(
            "result = {'sessionId':'native','configOptions':options()}",
            &format!("error = {{'code':-32603,'message':{cause:?}}}"),
        );
        std::fs::write(&script, source).unwrap();
        let mut spec = launch(
            root.path(),
            script,
            AcceptedSessionConfig {
                model: Some("opus[1m]".into()),
                effort: None,
            },
        );
        spec.harness = HarnessKind::Claude;
        let (commands, requests) = mpsc::channel(8);
        let (events_tx, mut events) = mpsc::channel(64);
        prompt(&commands, "must-not-run").await;
        let error = tokio::time::timeout(Duration::from_secs(10), run(spec, requests, events_tx))
            .await
            .unwrap()
            .unwrap_err();
        assert!(format!("{error:#}").contains(cause), "{error:#}");
        while let Some(event) = events.recv().await {
            assert!(!matches!(
                event,
                RuntimeEvent::SessionConfigured { .. }
                    | RuntimeEvent::ElicitationRequested { .. }
                    | RuntimeEvent::PromptFinished { .. }
            ));
        }
    }
}

/// Codex reads its model out of the bridge environment at startup, so the pin
/// has to carry the value this session holds at *this* launch. A session that
/// accepts its model after the worker started -- which every session created
/// with an explicit model does -- resumed every later bridge on the profile
/// default while the supervisor spec was written once and never updated.
#[cfg(unix)]
#[tokio::test]
async fn a_codex_bridge_restart_starts_on_a_model_accepted_after_the_worker_did() {
    use crate::worker_runtime::AcpSupervisorSpec;

    let root = tempfile::tempdir().unwrap();
    let spec_path = root.path().join("acp-supervisor.json");
    let launches = root.path().join("launched-on.txt");
    let script = root.path().join("codex_like.py");
    std::fs::write(
        &script,
        r#"
import json, os, sys
spec = json.load(open(sys.argv[1]))
pinned = spec['environment'].get('CODEX_CONFIG')
with open(sys.argv[2], 'a') as log:
    log.write((json.loads(pinned)['model'] if pinned else 'unpinned') + '\n')
model = 'default'
def options():
    return [{'id':'model','name':'Model','category':'model','type':'select',
             'currentValue':model,
             'options':[{'value':x,'name':x} for x in ['default','chosen']]}]
for line in sys.stdin:
    request = json.loads(line)
    method, ident = request.get('method'), request.get('id')
    if ident is None: continue
    params = request.get('params', {})
    if method == 'initialize': result = {'protocolVersion':1}
    elif method in ('session/new','session/load'):
        result = {'sessionId':'native','configOptions':options(),
                  'modes':{'currentModeId':'agent',
                           'availableModes':[{'id':'agent','name':'Agent'}]}}
    elif method == 'session/set_config_option':
        model = params['value']
        result = {'configOptions':options()}
    elif method == 'session/prompt':
        if params['prompt'][0]['text'] == 'restart': os._exit(0)
        print(json.dumps({'jsonrpc':'2.0','method':'session/update','params':{'sessionId':'native','update':{'sessionUpdate':'agent_message_chunk','content':{'type':'text','text':'ok'}}}}), flush=True)
        result = {'stopReason':'end_turn'}
    else: result = {}
    print(json.dumps({'jsonrpc':'2.0','id':ident,'result':result}), flush=True)
"#,
    )
    .unwrap();
    AcpSupervisorSpec {
        command: "python3".into(),
        args: vec![script.to_string_lossy().into_owned()],
        environment: BTreeMap::new(),
        cwd: root.path().to_owned(),
        harness_lease: None,
    }
    .write_spec(&spec_path)
    .unwrap();

    let mut spec = launch(
        root.path(),
        script.clone(),
        AcceptedSessionConfig::default(),
    );
    spec.harness = HarnessKind::Codex;
    spec.bridge_spec_path = Some(spec_path.clone());
    spec.args = vec![
        script.to_string_lossy().into_owned(),
        spec_path.to_string_lossy().into_owned(),
        launches.to_string_lossy().into_owned(),
    ];
    let (commands, requests) = mpsc::channel(8);
    let (events_tx, mut events) = mpsc::channel(64);
    let runtime = tokio::spawn(run(spec, requests, events_tx));
    configured(&mut events).await;

    commands
        .send(CommandRequest::SetConfig {
            request_id: "choose".into(),
            key: "model".into(),
            value: "chosen".into(),
        })
        .await
        .unwrap();
    loop {
        match next(&mut events).await {
            RuntimeEvent::ConfigApplied { request_id, .. } if request_id == "choose" => break,
            RuntimeEvent::CommandRejected { message, .. } => panic!("{message}"),
            _ => {}
        }
    }

    prompt(&commands, "restart").await;
    configured(&mut events).await;
    drop(commands);
    tokio::time::timeout(Duration::from_secs(10), runtime)
        .await
        .unwrap()
        .unwrap()
        .unwrap();

    assert_eq!(
        std::fs::read_to_string(&launches).unwrap(),
        "unpinned\nchosen\n",
        "the replacement bridge must start on the accepted model"
    );
}
