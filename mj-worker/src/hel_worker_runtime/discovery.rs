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
        command: std::env::current_exe()?,
        args: vec![
            "worker".into(),
            "acp-supervisor".into(),
            "--spec".into(),
            supervisor.to_string_lossy().into_owned(),
        ],
        environment: Default::default(),
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
    let harness = launch.harness;
    let (commands, requests) = mpsc::channel(8);
    let (events, mut updates) = mpsc::channel(128);
    let mut task = tokio::spawn(hel_acp::run(launch, requests, events));
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
        if commands
            .send(CommandRequest::Close {
                request_id: "discovery-close".into(),
            })
            .await
            .is_err()
        {
            return (&mut task).await.context("discovery task panicked")?;
        }
        loop {
            tokio::select! {
                result = &mut task => return result.context("discovery task panicked")?,
                event = updates.recv() => if event.is_none() { return (&mut task).await.context("discovery task panicked")?; },
            }
        }
    };
    match tokio::time::timeout(Duration::from_secs(15), cleanup).await {
        Ok(Ok(())) => discovery,
        Ok(Err(error)) => Err(error.context("stop profile discovery")),
        Err(_) => {
            task.abort();
            let _ = task.await;
            bail!("profile discovery cleanup timed out");
        }
    }
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
    #[tokio::test]
    async fn discovery_reads_model_specific_efforts_without_sending_a_prompt() {
        let root = tempfile::tempdir().unwrap();
        let script = root.path().join("harness.py");
        std::fs::write(&script, r#"
import json, sys
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
    elif method == 'session/set_config_option':
        assert request['params']['configId'] == 'model_id'
        model = request['params']['value']
        result = {'configOptions':options()}
    else: result = {}
    print(json.dumps({'jsonrpc':'2.0','id':ident,'result':result}), flush=True)
"#).unwrap();
        let launch = LaunchSpec {
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
        let chosen = probe(launch, Some("chosen".into())).await.unwrap();
        assert_eq!(chosen.model.as_deref(), Some("chosen"));
        assert_eq!(
            chosen
                .efforts
                .iter()
                .map(|choice| choice.value.as_str())
                .collect::<Vec<_>>(),
            ["high", "max"]
        );
    }
}
