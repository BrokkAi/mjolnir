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
async fn an_unavailable_saved_selector_fails_before_ready_or_prompt_delivery() {
    let root = tempfile::tempdir().unwrap();
    let spec = launch(
        root.path(),
        reset_on_load_harness(root.path()),
        AcceptedSessionConfig {
            model: Some("removed-model".into()),
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
