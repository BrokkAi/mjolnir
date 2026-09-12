//! A short-lived harness that discovers configuration without a prompt.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use hel::hel_acp::{self, AcceptedSessionConfig, CommandRequest, LaunchSpec, RuntimeEvent};
use hel::hel_config::ExecutionPolicy;
use hel::hel_worker_launch::{HarnessRuntimePolicy, ProfileConfig, ProfileProbeSpec};
use tokio::sync::mpsc;

use super::AcpSupervisorSpec;

pub async fn discover_profile_config(spec: ProfileProbeSpec) -> Result<ProfileConfig> {
    let policy = ExecutionPolicy::ConfiguredApprovals;
    let mut environment = spec.environment;
    environment.insert(
        spec.harness.home_env().into(),
        spec.profile_home.to_string_lossy().into_owned(),
    );
    let managed = super::harness::resolve(
        HarnessRuntimePolicy::Managed,
        spec.harness,
        policy,
        &environment,
    )
    .await?
    .context("managed discovery installation is missing")?;
    environment.extend(managed.environment.clone());
    let session_environment = hel::hel_login_environment::with_overrides(&environment).await?;
    let supervisor = spec.cwd.join("acp-supervisor.json");
    AcpSupervisorSpec {
        command: managed.command.clone(),
        args: managed.args.clone(),
        environment,
        cwd: spec.cwd.clone(),
        harness_lease: Some(managed.lease_path.clone()),
    }
    .write_spec(&supervisor)?;
    let launch = LaunchSpec {
        goal_recovery: Default::default(),
        command: std::env::current_exe()?,
        args: vec![
            "--login-environment-ready".into(),
            "worker".into(),
            "acp-supervisor".into(),
            "--spec".into(),
            supervisor.to_string_lossy().into_owned(),
        ],
        environment: session_environment,
        cwd: spec.cwd,
        additional_directories: vec![],
        project_memory: None,
        extra_mcp_servers: vec![],
        resume_session: None,
        accepted_config: Arc::new(Mutex::new(AcceptedSessionConfig::default())),
        harness: spec.harness,
        execution_policy: policy,
        acp_activity: Default::default(),
        step_clock: Default::default(),
    };
    probe(launch, spec.model).await
}

async fn probe(launch: LaunchSpec, model: Option<String>) -> Result<ProfileConfig> {
    probe_with_close_timeout(launch, model, Duration::from_secs(15)).await
}

async fn probe_with_close_timeout(
    launch: LaunchSpec,
    model: Option<String>,
    close_timeout: Duration,
) -> Result<ProfileConfig> {
    let harness = launch.harness;
    let (commands, requests) = mpsc::channel(8);
    let (events, mut updates) = mpsc::channel(128);
    let shutdown = tokio_util::sync::CancellationToken::new();
    let mut task = tokio::spawn(hel_acp::run_with_shutdown(
        launch,
        requests,
        events,
        shutdown.clone(),
    ));
    let discovery = tokio::time::timeout(Duration::from_secs(240), async {
        let mut warning = String::new();
        let mut initial_models = None;
        loop {
            match updates
                .recv()
                .await
                .context("discovery harness stopped before reporting choices")?
            {
                RuntimeEvent::SessionConfigured { config_options } if initial_models.is_none() => {
                    let models = hel_acp::session_config_choices(&config_options, "model");
                    if let Some(model) = &model
                        && models.iter().any(|choice| &choice.value == model)
                    {
                        initial_models = Some(models);
                        commands
                            .send(CommandRequest::SetConfig {
                                request_id: "discover-model".into(),
                                key: "model".into(),
                                value: model.clone(),
                            })
                            .await?;
                    } else {
                        return Ok(discovered(harness, config_options, models));
                    }
                }
                RuntimeEvent::ConfigApplied {
                    request_id,
                    config_options,
                    ..
                } if request_id == "discover-model" => {
                    return Ok(discovered(
                        harness,
                        config_options,
                        initial_models.take().unwrap_or_default(),
                    ));
                }
                RuntimeEvent::CommandRejected { message, .. }
                | RuntimeEvent::CommandInterrupted { message, .. } => {
                    bail!("discover model configuration: {message}")
                }
                RuntimeEvent::Warning { message } => warning = message,
                RuntimeEvent::Stopped => bail!("discovery harness stopped: {warning}"),
                _ => {}
            }
        }
    })
    .await
    .context("profile discovery timed out")
    .and_then(|result| result);
    // Keep draining while the runtime shuts down; it can emit more than one
    // channel's worth of final events. The supervisor owns process cleanup.
    let cleanup = async {
        let close = commands.send(CommandRequest::Close {
            request_id: "discovery-close".into(),
        });
        tokio::pin!(close);
        let mut sent = false;
        loop {
            tokio::select! {
                _ = &mut close, if !sent => sent = true,
                result = &mut task => return result.context("discovery task panicked")?,
                event = updates.recv() => if event.is_none() { return (&mut task).await.context("discovery task panicked")?; },
            }
        }
    };
    let cleanup = match tokio::time::timeout(close_timeout, cleanup).await {
        Ok(result) => result,
        Err(_) => {
            shutdown.cancel();
            // Closing the transport lets the supervisor terminate its process group.
            let stopped = async {
                loop {
                    tokio::select! {
                        result = &mut task => return result.context("discovery shutdown task panicked")?,
                        event = updates.recv() => if event.is_none() { return (&mut task).await.context("discovery shutdown task panicked")?; },
                    }
                }
            };
            match tokio::time::timeout(Duration::from_secs(10), stopped).await {
                Ok(Ok(())) => Err(anyhow::anyhow!(
                    "profile discovery close timed out; harness terminated"
                )),
                Ok(Err(error)) => {
                    Err(error.context("terminate profile discovery after close timeout"))
                }
                Err(_) => {
                    task.abort();
                    match task.await {
                        Ok(Err(error)) => {
                            tracing::error!(error = %format!("{error:#}"), "profile discovery failed during forced shutdown")
                        }
                        Err(error) if !error.is_cancelled() => {
                            tracing::error!(%error, "profile discovery task failed during forced shutdown")
                        }
                        _ => {}
                    }
                    Err(anyhow::anyhow!(
                        "profile discovery forced shutdown timed out"
                    ))
                }
            }
        }
    };
    preserve_discovery(discovery, cleanup)
}

fn preserve_discovery(
    discovery: Result<ProfileConfig>,
    cleanup: Result<()>,
) -> Result<ProfileConfig> {
    if let Err(error) = cleanup {
        tracing::warn!(discovery_succeeded = discovery.is_ok(), error = %format!("{error:#}"), "profile discovery cleanup failed");
    }
    discovery
}

fn discovered(
    harness: hel::hel_config::HarnessKind,
    options: Vec<agent_client_protocol::schema::v1::SessionConfigOption>,
    models: Vec<hel_acp::SessionConfigChoice>,
) -> ProfileConfig {
    let facts =
        hel_acp::AcpSessionFacts::from_operational(harness, &Default::default(), &options, None);
    ProfileConfig {
        model: facts.current_model().map(str::to_owned),
        models,
        efforts: hel_acp::session_config_choices(&options, "effort"),
        observed_at: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn discovery_cleanup_errors_do_not_replace_results() {
        let config = discovered(hel::hel_config::HarnessKind::Kimi, vec![], vec![]);
        assert!(preserve_discovery(Ok(config), Err(anyhow::anyhow!("cleanup failure"))).is_ok());
        let error = preserve_discovery(
            Err(anyhow::anyhow!("original discovery failure")),
            Err(anyhow::anyhow!("cleanup failure")),
        )
        .unwrap_err();
        assert_eq!(error.to_string(), "original discovery failure");
    }

    #[tokio::test]
    async fn discovery_reads_model_specific_efforts_without_sending_a_prompt() {
        let root = tempfile::tempdir().unwrap();
        let script = root.path().join("harness.py");
        std::fs::write(&script, r#"
import json, sys, os, time
model = 'default'
def options():
    return [
      {'id':'model_id','name':'Model','category':'model','type':'select', 'currentValue':model,
       'options':[{'value':x,'name':x} for x in ['default','chosen']]},
      {'id':'thinking','name':'Thinking','category':'thought_level','type':'select',
       'currentValue':'high' if model == 'chosen' else 'low',
       'options':[{'value':x,'name':x} for x in (['high','max'] if model == 'chosen' else ['low'])]}]
for line in sys.stdin:
    request = json.loads(line)
    method, ident = request.get('method'), request.get('id')
    assert method != 'session/prompt', 'discovery must never spend a turn'
    if ident is None: continue
    if method == 'initialize': result = {'protocolVersion':1}
    elif method == 'session/new': result = {'sessionId':'probe','configOptions':options()}
    elif method == 'session/close' and os.environ.get('HANG_CLOSE'):
        for i in range(300):
            print(json.dumps({'jsonrpc':'2.0','method':'session/update','params':{'sessionId':'probe','update':{'sessionUpdate':'agent_message_chunk','content':{'type':'text','text':'x'*1024}}}}), flush=True)
        time.sleep(60)
        continue
    elif method == 'session/set_config_option':
        if os.environ.get('FAIL_CONFIG'):
            print(json.dumps({'jsonrpc':'2.0','id':ident,'error':{'code':-32603,'message':'original model failure'}}), flush=True)
            continue
        assert request['params']['configId'] == 'model_id'
        model = request['params']['value']
        result = {'configOptions':options()}
    else: result = {}
    print(json.dumps({'jsonrpc':'2.0','id':ident,'result':result}), flush=True)
"#).unwrap();
        let launch = LaunchSpec {
            goal_recovery: Default::default(),
            command: "python3".into(),
            args: vec![script.to_string_lossy().into_owned()],
            environment: Default::default(),
            cwd: root.path().to_owned(),
            additional_directories: vec![],
            project_memory: None,
            extra_mcp_servers: vec![],
            resume_session: None,
            accepted_config: Arc::new(Mutex::new(AcceptedSessionConfig::default())),
            harness: hel::hel_config::HarnessKind::Kimi,
            execution_policy: ExecutionPolicy::ConfiguredApprovals,
            acp_activity: Default::default(),
            step_clock: Default::default(),
        };
        let defaults = probe(launch.clone(), None).await.unwrap();
        assert_eq!(defaults.model.as_deref(), Some("default"));
        assert_eq!(defaults.efforts[0].value, "low");
        let chosen = probe(launch.clone(), Some("chosen".into())).await.unwrap();
        assert_eq!(chosen.model.as_deref(), Some("chosen"));
        assert_eq!(
            chosen
                .efforts
                .iter()
                .map(|choice| choice.value.as_str())
                .collect::<Vec<_>>(),
            ["high", "max"]
        );
        let mut hanging = launch;
        hanging.accepted_config = Arc::new(Mutex::new(AcceptedSessionConfig::default()));
        hanging.environment.insert("HANG_CLOSE".into(), "1".into());
        let result = tokio::time::timeout(
            Duration::from_secs(12),
            probe_with_close_timeout(hanging.clone(), None, Duration::from_millis(200)),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(result.model.as_deref(), Some("default"));
        hanging.environment.insert("FAIL_CONFIG".into(), "1".into());
        let error = tokio::time::timeout(
            Duration::from_secs(12),
            probe_with_close_timeout(hanging, Some("chosen".into()), Duration::from_millis(200)),
        )
        .await
        .unwrap()
        .unwrap_err();
        assert!(
            format!("{error:#}").contains("original model failure"),
            "{error:#}"
        );
    }
}
