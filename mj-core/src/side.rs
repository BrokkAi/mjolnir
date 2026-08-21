//! Isolated ephemeral ACP runtime used by side conversations.

use std::path::{Path, PathBuf};
use std::time::Duration;

use tokio::sync::mpsc;

use crate::acp;
use crate::config::SelectedAgent;
use crate::event::{UiCommand, UiEvent};

pub struct Runtime {
    pub session_id: String,
    commands: mpsc::UnboundedSender<UiCommand>,
    runtime_task: tokio::task::JoinHandle<()>,
    event_task: tokio::task::JoinHandle<()>,
}

impl Runtime {
    pub fn send(&self, command: UiCommand) -> bool {
        self.commands.send(command).is_ok()
    }
}

pub struct Launch<'a> {
    pub agent: &'a SelectedAgent,
    pub cwd: PathBuf,
    pub additional_directories: Vec<PathBuf>,
    pub agent_stderr: Option<PathBuf>,
    pub fs_max_text_bytes: u64,
}

pub async fn start(
    launch: Launch<'_>,
    main_commands: &mpsc::UnboundedSender<UiCommand>,
    events: mpsc::UnboundedSender<UiEvent>,
) -> Result<Runtime, String> {
    let (responder, response) = tokio::sync::oneshot::channel();
    main_commands
        .send(UiCommand::ForkSideSession { responder })
        .map_err(|_| "the main ACP runtime closed before side startup".to_string())?;
    let source = match response.await {
        Ok(Ok(source)) => source,
        Ok(Err(message)) => return Err(message),
        Err(_) => {
            return Err("the main ACP runtime dropped the side fork response".to_string());
        }
    };
    let fork_source = source.has_history;
    let resume_session = fork_source.then_some(source.session_id);

    // A forked side session already has the provider's native context. A
    // fresh side session synchronizes native memory before it starts.
    let memory = if fork_source {
        None
    } else {
        crate::memory::worker_lane_memory(&launch.agent.source_id, &launch.cwd)
    };
    let (side_event_tx, mut side_event_rx) = mpsc::unbounded_channel();
    let (side_cmd_tx, side_cmd_rx) = mpsc::unbounded_channel();
    let side_cfg = isolated_runtime_config(
        launch.agent,
        resume_session,
        launch.cwd,
        launch.additional_directories,
        launch.agent_stderr,
        launch.fs_max_text_bytes,
        memory,
    );
    let runtime_task = tokio::spawn(async move {
        let _ = acp::run(side_cfg, side_event_tx, side_cmd_rx).await;
    });
    let (child_ready_tx, child_ready_rx) = tokio::sync::oneshot::channel();
    let expected_session_starts = if fork_source { 2 } else { 1 };
    let event_task = tokio::spawn(async move {
        let mut child_ready_tx = Some(child_ready_tx);
        let mut session_starts = 0_u8;
        let mut started = false;
        while let Some(event) = side_event_rx.recv().await {
            if let UiEvent::SessionStarted { session_id, .. } = &event {
                session_starts = session_starts.saturating_add(1);
                if session_starts < expected_session_starts {
                    continue;
                }
                if session_starts == expected_session_starts {
                    started = true;
                    if let Some(tx) = child_ready_tx.take() {
                        let _ = tx.send(Ok(session_id.clone()));
                    }
                }
            } else if !started {
                let failure = match &event {
                    UiEvent::SessionForkFailed { message } | UiEvent::Fatal(message) => {
                        Some(message.clone())
                    }
                    _ => None,
                };
                if let Some(message) = failure {
                    if let Some(tx) = child_ready_tx.take() {
                        let _ = tx.send(Err(message));
                    }
                    break;
                }
                continue;
            }
            if events.send(UiEvent::Side(Box::new(event))).is_err() {
                break;
            }
        }
    });
    if fork_source && side_cmd_tx.send(UiCommand::ForkSession).is_err() {
        runtime_task.abort();
        event_task.abort();
        return Err("the side ACP runtime closed before forking".to_string());
    }
    let child_session_id = match tokio::time::timeout(Duration::from_secs(15), child_ready_rx).await
    {
        Ok(Ok(Ok(session_id))) => session_id,
        Ok(Ok(Err(message))) => {
            let _ = side_cmd_tx.send(UiCommand::Shutdown);
            event_task.abort();
            return Err(message);
        }
        Ok(Err(_)) => {
            let _ = side_cmd_tx.send(UiCommand::Shutdown);
            event_task.abort();
            return Err("the side ACP runtime dropped its fork result".to_string());
        }
        Err(_) => {
            let _ = side_cmd_tx.send(UiCommand::Shutdown);
            event_task.abort();
            return Err("side session fork timed out".to_string());
        }
    };

    Ok(Runtime {
        session_id: child_session_id,
        commands: side_cmd_tx,
        runtime_task,
        event_task,
    })
}

pub fn isolated_runtime_config(
    agent: &SelectedAgent,
    resume_session: Option<String>,
    cwd: PathBuf,
    additional_directories: Vec<PathBuf>,
    agent_stderr: Option<PathBuf>,
    fs_max_text_bytes: u64,
    memory: Option<crate::memory::SessionMemory>,
) -> acp::AcpRuntimeConfig {
    acp::AcpRuntimeConfig {
        command: agent.program.clone(),
        args: agent.args.clone(),
        cwd,
        additional_directories,
        mcp_servers: Vec::new(),
        resume_session,
        session_restore_mode: acp::SessionRestoreMode::Continue,
        env: agent.env.clone(),
        agent_stderr,
        fs_max_text_bytes,
        access_mode: acp::RuntimeAccessMode::Full,
        agent_source_id: None,
        config_path: None,
        saved_session_config: std::collections::HashMap::new(),
        role_config: None,
        subagents: None,
        memory,
        side_prompt_policy: true,
        termination: None,
    }
}

pub async fn discard(
    side: Runtime,
    agent: &SelectedAgent,
    agent_stderr: Option<&Path>,
) -> Option<String> {
    let _ = side.commands.send(UiCommand::CancelPrompt);
    let _ = side.commands.send(UiCommand::Shutdown);
    let mut runtime_task = side.runtime_task;
    if tokio::time::timeout(Duration::from_secs(2), &mut runtime_task)
        .await
        .is_err()
    {
        runtime_task.abort();
        let _ = runtime_task.await;
    }
    side.event_task.abort();
    match tokio::time::timeout(
        Duration::from_secs(5),
        crate::session::delete_session(agent, side.session_id.clone(), agent_stderr),
    )
    .await
    {
        Ok(Ok(())) => None,
        Ok(Err(error)) => Some(format!(
            "could not delete side session {}: {error:#}",
            side.session_id
        )),
        Err(_) => Some(format!(
            "timed out deleting side session {}",
            side.session_id
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runtime_config_has_no_agent_services_or_persistence() {
        let agent = SelectedAgent {
            source_id: "test-agent".to_string(),
            program: PathBuf::from("agent"),
            args: vec!["acp".to_string()],
            env: std::collections::HashMap::new(),
        };
        let cfg = isolated_runtime_config(
            &agent,
            Some("child-session".to_string()),
            PathBuf::from("/workspace"),
            vec![PathBuf::from("/extra")],
            None,
            acp::DEFAULT_FS_TEXT_BYTES,
            None,
        );

        assert!(cfg.mcp_servers.is_empty());
        assert!(cfg.subagents.is_none());
        assert!(cfg.role_config.is_none());
        assert!(cfg.agent_source_id.is_none());
        assert!(cfg.config_path.is_none());
        assert!(cfg.saved_session_config.is_empty());
        assert!(cfg.side_prompt_policy);
        assert_eq!(cfg.resume_session.as_deref(), Some("child-session"));
        assert!(cfg.memory.is_none(), "forked side sessions carry no memory");

        let memory = crate::memory::SessionMemory {
            store_path: PathBuf::from("/tmp/memories.json"),
            config_path: None,
            project: PathBuf::from("/workspace"),
            inject: true,
            cleanup: false,
            tools: false,
        };
        let cfg = isolated_runtime_config(
            &agent,
            None,
            PathBuf::from("/workspace"),
            Vec::new(),
            None,
            acp::DEFAULT_FS_TEXT_BYTES,
            Some(memory),
        );
        let memory = cfg.memory.expect("fresh side sessions carry memory");
        assert!(memory.inject);
        assert!(!memory.tools);
    }
}
