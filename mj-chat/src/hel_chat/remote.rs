//! The chat's background command channel: every relay operation the view can
//! ask for, the task that runs them, and how their results land back in state.

use agent_client_protocol::schema::v1::{ContentBlock, TextContent};

use hel::hel_elicitation::{ElicitationRequest, ElicitationResponse};
use hel::hel_state::{QueuedCommandKind, config_command_text};
use hel::hel_worker::RelayCommand;
use mj_controller::hel_session_manager::{ManagedSessionHandle, SessionManagerControl};

use super::{ChatState, PlanControl, PlanReviewFollowup, queued_prompt_preview};

const CHAT_REMOTE_QUEUE_CAPACITY: usize = 32;
const SESSION_ACTOR_REPLACEMENT_WAIT: std::time::Duration = std::time::Duration::from_secs(5);

#[derive(Debug)]
pub(super) enum ChatRemoteOperation {
    Sync,
    Prompt {
        command_id: String,
        text: String,
    },
    RunShell {
        command_id: String,
        command: String,
    },
    RemoveQueuedPrompt {
        command_id: String,
        id: String,
        text: String,
        kind: QueuedCommandKind,
    },
    SetConfig {
        command_id: String,
        key: String,
        value: String,
    },
    PlanCommand {
        command_id: String,
        original: String,
        control: PlanControl,
        requested_active: bool,
        prompt: Option<String>,
    },
    Cancel {
        command_id: String,
        cancel_agent: bool,
        shell_command_ids: Vec<String>,
    },
    RespondElicitation {
        request: ElicitationRequest,
        response: ElicitationResponse,
        plan_followup: Option<PlanReviewFollowup>,
    },
}

#[derive(Debug)]
pub(super) enum ChatRemoteResult {
    Sync(std::result::Result<(), String>),
    Prompt {
        text: String,
        result: std::result::Result<u64, String>,
    },
    RunShell {
        command: String,
        result: std::result::Result<u64, String>,
    },
    RemoveQueuedPrompt {
        id: String,
        text: String,
        kind: QueuedCommandKind,
        result: std::result::Result<(), String>,
    },
    SetConfig {
        key: String,
        value: String,
        result: std::result::Result<(), String>,
    },
    PlanCommand {
        original: String,
        requested_active: bool,
        control_applied: bool,
        result: std::result::Result<Option<u64>, String>,
    },
    Cancel(std::result::Result<(), String>),
    RespondElicitation {
        request: ElicitationRequest,
        desired_plan_active: Option<bool>,
        answered: bool,
        result: std::result::Result<(), String>,
    },
    WorkerFailed(String),
}

impl ChatRemoteResult {
    fn failure_message(&self) -> Option<&str> {
        match self {
            Self::Sync(Err(error))
            | Self::Prompt {
                result: Err(error), ..
            }
            | Self::RunShell {
                result: Err(error), ..
            }
            | Self::RemoveQueuedPrompt {
                result: Err(error), ..
            }
            | Self::SetConfig {
                result: Err(error), ..
            }
            | Self::PlanCommand {
                result: Err(error), ..
            }
            | Self::Cancel(Err(error))
            | Self::RespondElicitation {
                result: Err(error), ..
            }
            | Self::WorkerFailed(error) => Some(error),
            _ => None,
        }
    }
}

fn publish_chat_remote_result(
    results: &tokio::sync::mpsc::UnboundedSender<ChatRemoteResult>,
    attached: &std::sync::Arc<std::sync::atomic::AtomicBool>,
    result: ChatRemoteResult,
) {
    if !attached.load(std::sync::atomic::Ordering::Acquire) {
        if let Some(error) = result.failure_message() {
            tracing::error!(%error, "detached chat operation failed");
        }
        return;
    }
    if let Err(error) = results.send(result)
        && let Some(error) = error.0.failure_message()
    {
        tracing::error!(%error, "chat operation failed after its UI closed");
    }
}

pub(super) struct ChatRemoteSupervisor {
    operations: Option<tokio::sync::mpsc::Sender<ChatRemoteOperation>>,
    results: tokio::sync::mpsc::UnboundedReceiver<ChatRemoteResult>,
    attached: std::sync::Arc<std::sync::atomic::AtomicBool>,
    worker: Option<tokio::task::JoinHandle<()>>,
}

impl ChatRemoteSupervisor {
    pub(super) fn spawn(
        session: ManagedSessionHandle,
        session_manager: SessionManagerControl,
    ) -> Self {
        let (operations_tx, operations_rx) = tokio::sync::mpsc::channel(CHAT_REMOTE_QUEUE_CAPACITY);
        let (results_tx, results_rx) = tokio::sync::mpsc::unbounded_channel();
        let attached = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
        let worker_attached = attached.clone();
        let worker = tokio::spawn(run_chat_remote_worker(
            session,
            session_manager,
            operations_rx,
            results_tx,
            worker_attached,
        ));
        Self {
            operations: Some(operations_tx),
            results: results_rx,
            attached,
            worker: Some(worker),
        }
    }

    pub(super) fn operations(&self) -> &tokio::sync::mpsc::Sender<ChatRemoteOperation> {
        self.operations
            .as_ref()
            .expect("chat remote supervisor is attached")
    }

    pub(super) fn try_recv(
        &mut self,
    ) -> std::result::Result<ChatRemoteResult, tokio::sync::mpsc::error::TryRecvError> {
        self.results.try_recv()
    }

    /// Waits for the next result. `None` means the worker is gone and no
    /// further result can arrive, so the caller must stop awaiting this feed.
    /// Cancel safe: an unfinished `recv` takes no message.
    pub(super) async fn recv(&mut self) -> Option<ChatRemoteResult> {
        self.results.recv().await
    }

    pub(super) async fn take_finished(
        &mut self,
    ) -> Option<std::result::Result<(), tokio::task::JoinError>> {
        if !self
            .worker
            .as_ref()
            .is_some_and(tokio::task::JoinHandle::is_finished)
        {
            return None;
        }
        Some(
            self.worker
                .take()
                .expect("finished chat worker exists")
                .await,
        )
    }
}

impl Drop for ChatRemoteSupervisor {
    fn drop(&mut self) {
        self.attached
            .store(false, std::sync::atomic::Ordering::Release);
        self.results.close();
        while let Ok(result) = self.results.try_recv() {
            if let Some(error) = result.failure_message() {
                tracing::error!(%error, "chat operation failed while detaching");
            }
        }
        drop(self.operations.take());
        let Some(worker) = self.worker.take() else {
            return;
        };
        match tokio::runtime::Handle::try_current() {
            Ok(runtime) => {
                runtime.spawn(async move {
                    if let Err(error) = worker.await {
                        tracing::error!(%error, "detached chat background worker failed");
                    }
                });
            }
            Err(error) => {
                worker.abort();
                tracing::error!(%error, "could not supervise detached chat background worker");
            }
        }
    }
}

async fn run_chat_remote_worker(
    mut session: ManagedSessionHandle,
    session_manager: SessionManagerControl,
    mut operations: tokio::sync::mpsc::Receiver<ChatRemoteOperation>,
    results: tokio::sync::mpsc::UnboundedSender<ChatRemoteResult>,
    attached: std::sync::Arc<std::sync::atomic::AtomicBool>,
) {
    let mut pending = tokio::task::JoinSet::new();
    let mut accepting = true;
    loop {
        if !accepting && pending.is_empty() {
            break;
        }
        tokio::select! {
            operation = operations.recv(), if accepting => {
                let Some(operation) = operation else {
                    accepting = false;
                    continue;
                };
                if session.is_stopped() {
                    let session_id = session.session_id().to_owned();
                    match session_manager
                        .wait_for_session(&session_id, SESSION_ACTOR_REPLACEMENT_WAIT)
                        .await
                    {
                        Ok(replacement) => session = replacement,
                        Err(error) => tracing::warn!(
                            %session_id,
                            error = format!("{error:#}"),
                            "could not reacquire replacement session actor before dispatch"
                        ),
                    }
                }
                enqueue_chat_remote_operation(
                    &session,
                    operation,
                    &mut pending,
                    &results,
                    &attached,
                ).await;
            }
            joined = pending.join_next(), if !pending.is_empty() => {
                if let Some(Err(error)) = joined {
                    publish_chat_remote_result(
                        &results,
                        &attached,
                        ChatRemoteResult::WorkerFailed(format!(
                            "chat background operation failed: {error}"
                        )),
                    );
                }
            }
        }
    }
}

async fn enqueue_chat_remote_operation(
    session: &ManagedSessionHandle,
    operation: ChatRemoteOperation,
    pending: &mut tokio::task::JoinSet<()>,
    results: &tokio::sync::mpsc::UnboundedSender<ChatRemoteResult>,
    attached: &std::sync::Arc<std::sync::atomic::AtomicBool>,
) {
    match operation {
        ChatRemoteOperation::Sync => match session.enqueue_sync().await {
            Ok(response) => {
                let results = results.clone();
                let attached = attached.clone();
                pending.spawn(async move {
                    let result = response.wait().await.map_err(|error| format!("{error:#}"));
                    publish_chat_remote_result(&results, &attached, ChatRemoteResult::Sync(result));
                });
            }
            Err(error) => {
                publish_chat_remote_result(
                    results,
                    attached,
                    ChatRemoteResult::Sync(Err(format!("{error:#}"))),
                );
            }
        },
        ChatRemoteOperation::Prompt { command_id, text } => {
            let response = session
                .enqueue_submit(
                    command_id,
                    RelayCommand::Prompt {
                        prompt: vec![ContentBlock::Text(TextContent::new(text.clone()))],
                    },
                )
                .await;
            match response {
                Ok(response) => {
                    let results = results.clone();
                    let attached = attached.clone();
                    pending.spawn(async move {
                        let ordinal = match response.wait().await {
                            Ok(ordinal) => ordinal,
                            Err(error) => {
                                publish_chat_remote_result(
                                    &results,
                                    &attached,
                                    ChatRemoteResult::Prompt {
                                        text,
                                        result: Err(format!("{error:#}")),
                                    },
                                );
                                return;
                            }
                        };
                        publish_chat_remote_result(
                            &results,
                            &attached,
                            ChatRemoteResult::Prompt {
                                text: text.clone(),
                                result: Ok(ordinal),
                            },
                        );
                    });
                }
                Err(error) => {
                    publish_chat_remote_result(
                        results,
                        attached,
                        ChatRemoteResult::Prompt {
                            text,
                            result: Err(format!("{error:#}")),
                        },
                    );
                }
            }
        }
        ChatRemoteOperation::RunShell {
            command_id,
            command,
        } => {
            let response = session
                .enqueue_submit(
                    command_id,
                    RelayCommand::RunUserShell {
                        command: command.clone(),
                    },
                )
                .await;
            match response {
                Ok(response) => {
                    let results = results.clone();
                    let attached = attached.clone();
                    pending.spawn(async move {
                        let result = response.wait().await.map_err(|error| format!("{error:#}"));
                        publish_chat_remote_result(
                            &results,
                            &attached,
                            ChatRemoteResult::RunShell { command, result },
                        );
                    });
                }
                Err(error) => publish_chat_remote_result(
                    results,
                    attached,
                    ChatRemoteResult::RunShell {
                        command,
                        result: Err(format!("{error:#}")),
                    },
                ),
            }
        }
        ChatRemoteOperation::RemoveQueuedPrompt {
            command_id,
            id,
            text,
            kind,
        } => {
            let response = session
                .enqueue_submit(
                    command_id,
                    RelayCommand::RemoveQueuedPrompt {
                        queued_command_id: id.clone(),
                    },
                )
                .await;
            match response {
                Ok(response) => {
                    let results = results.clone();
                    let attached = attached.clone();
                    pending.spawn(async move {
                        let result = response
                            .wait()
                            .await
                            .map(|_| ())
                            .map_err(|error| format!("{error:#}"));
                        publish_chat_remote_result(
                            &results,
                            &attached,
                            ChatRemoteResult::RemoveQueuedPrompt {
                                id,
                                text,
                                kind,
                                result,
                            },
                        );
                    });
                }
                Err(error) => {
                    publish_chat_remote_result(
                        results,
                        attached,
                        ChatRemoteResult::RemoveQueuedPrompt {
                            id,
                            text,
                            kind,
                            result: Err(format!("{error:#}")),
                        },
                    );
                }
            }
        }
        ChatRemoteOperation::SetConfig {
            command_id,
            key,
            value,
        } => {
            let response = session
                .enqueue_submit(
                    command_id,
                    RelayCommand::SetConfig {
                        key: key.clone(),
                        value: value.clone(),
                    },
                )
                .await;
            match response {
                Ok(response) => {
                    let results = results.clone();
                    let attached = attached.clone();
                    pending.spawn(async move {
                        let result = response
                            .wait()
                            .await
                            .map(|_| ())
                            .map_err(|error| format!("{error:#}"));
                        publish_chat_remote_result(
                            &results,
                            &attached,
                            ChatRemoteResult::SetConfig { key, value, result },
                        );
                    });
                }
                Err(error) => {
                    publish_chat_remote_result(
                        results,
                        attached,
                        ChatRemoteResult::SetConfig {
                            key,
                            value,
                            result: Err(format!("{error:#}")),
                        },
                    );
                }
            }
        }
        ChatRemoteOperation::PlanCommand {
            command_id,
            original,
            control,
            requested_active,
            prompt,
        } => {
            let session = session.clone();
            let results = results.clone();
            let attached = attached.clone();
            pending.spawn(async move {
                let mut control_applied = false;
                let control_command = match control {
                    PlanControl::SetConfig { key, value } => RelayCommand::SetConfig { key, value },
                    PlanControl::SetSessionMode { mode_id } => {
                        RelayCommand::SetSessionMode { mode_id }
                    }
                };
                let result = async {
                    session
                        .enqueue_submit(command_id.clone(), control_command)
                        .await
                        .map_err(|error| format!("{error:#}"))?
                        .wait()
                        .await
                        .map_err(|error| format!("{error:#}"))?;
                    control_applied = true;
                    let Some(text) = prompt else {
                        return Ok(None);
                    };
                    let ordinal = session
                        .enqueue_submit(
                            format!("{command_id}-prompt"),
                            RelayCommand::Prompt {
                                prompt: vec![ContentBlock::Text(TextContent::new(text.clone()))],
                            },
                        )
                        .await
                        .map_err(|error| {
                            format!("mode changed, but prompt was not queued: {error:#}")
                        })?
                        .wait()
                        .await
                        .map_err(|error| format!("mode changed, but prompt failed: {error:#}"))?;
                    Ok(Some(ordinal))
                }
                .await;
                publish_chat_remote_result(
                    &results,
                    &attached,
                    ChatRemoteResult::PlanCommand {
                        original,
                        requested_active,
                        control_applied,
                        result,
                    },
                );
            });
        }
        ChatRemoteOperation::Cancel {
            command_id,
            cancel_agent,
            shell_command_ids,
        } => {
            let session = session.clone();
            let results = results.clone();
            let attached = attached.clone();
            pending.spawn(async move {
                let mut failures = Vec::new();
                if cancel_agent
                    && let Err(error) = session
                        .submit(command_id.clone(), RelayCommand::Cancel)
                        .await
                {
                    failures.push(format!("agent: {error:#}"));
                }
                for (index, shell_command_id) in shell_command_ids.into_iter().enumerate() {
                    if let Err(error) = session
                        .submit(
                            format!("{command_id}-shell-{index}"),
                            RelayCommand::CancelUserShell { shell_command_id },
                        )
                        .await
                    {
                        failures.push(format!("shell: {error:#}"));
                    }
                }
                publish_chat_remote_result(
                    &results,
                    &attached,
                    ChatRemoteResult::Cancel(if failures.is_empty() {
                        Ok(())
                    } else {
                        Err(failures.join("; "))
                    }),
                );
            });
        }
        ChatRemoteOperation::RespondElicitation {
            request,
            response,
            plan_followup,
        } => {
            let session = session.clone();
            let results = results.clone();
            let attached = attached.clone();
            pending.spawn(async move {
                let mut answered = false;
                let desired_plan_active = plan_followup
                    .as_ref()
                    .map(|followup| followup.desired_active);
                let result = async {
                    session
                        .respond_elicitation(request.id.clone(), response)
                        .await
                        .map_err(|error| format!("{error:#}"))?;
                    answered = true;
                    let Some(followup) = plan_followup else {
                        return Ok(());
                    };
                    if let Some(control) = followup.control {
                        let command = match control {
                            PlanControl::SetConfig { key, value } => {
                                RelayCommand::SetConfig { key, value }
                            }
                            PlanControl::SetSessionMode { mode_id } => {
                                RelayCommand::SetSessionMode { mode_id }
                            }
                        };
                        session
                            .submit(format!("plan-review-{}-mode", request.id), command)
                            .await
                            .map_err(|error| {
                                format!("review answered, but plan mode was not changed: {error:#}")
                            })?;
                    }
                    if let Some(prompt) = followup.prompt {
                        session
                            .submit(
                                format!("plan-review-{}-feedback", request.id),
                                RelayCommand::Prompt {
                                    prompt: vec![ContentBlock::Text(TextContent::new(
                                        prompt.clone(),
                                    ))],
                                },
                            )
                            .await
                            .map_err(|error| {
                                format!(
                                    "review answered, but revision feedback was not sent: {error:#}"
                                )
                            })?;
                    }
                    Ok(())
                }
                .await;
                publish_chat_remote_result(
                    &results,
                    &attached,
                    ChatRemoteResult::RespondElicitation {
                        request,
                        desired_plan_active,
                        answered,
                        result,
                    },
                );
            });
        }
    }
}

pub(super) fn restore_unsent_input(chat: &mut ChatState, input: &str) {
    if chat.input.is_empty() {
        chat.set_input(input.to_owned());
    } else if chat.input != input {
        chat.set_input(format!("{input}\n\n{}", chat.input));
    }
}

pub(super) fn apply_chat_remote_result(chat: &mut ChatState, result: ChatRemoteResult) {
    // Most failures are intentionally converted into a notice so the user can
    // keep reading and editing the chat. Keep the diagnostic in the process
    // log as well; notices are transient and can be overwritten by the next
    // event.
    if let Some(error) = result.failure_message() {
        tracing::warn!(
            session_id = %chat.session_id,
            %error,
            "chat operation failed and was shown in the UI"
        );
    }
    match result {
        ChatRemoteResult::Sync(Ok(())) => {
            chat.set_transcript_loading(false);
            chat.set_notice("Connected to session relay");
        }
        ChatRemoteResult::Sync(Err(error)) => {
            chat.set_transcript_loading(false);
            chat.set_notice(format!("Connection failed: {error}"))
        }
        ChatRemoteResult::Prompt {
            text,
            result: Ok(ordinal),
        } => chat.set_notice(format!(
            "Prompt accepted by relay at {ordinal}: {}",
            queued_prompt_preview(&text)
        )),
        ChatRemoteResult::Prompt {
            text,
            result: Err(error),
        } => {
            restore_unsent_input(chat, &text);
            chat.set_notice(format!("Prompt was not sent: {error}"));
        }
        ChatRemoteResult::RunShell {
            command,
            result: Ok(ordinal),
        } => chat.set_notice(format!(
            "Shell command accepted by relay at {ordinal}: {}",
            queued_prompt_preview(&command)
        )),
        ChatRemoteResult::RunShell {
            command,
            result: Err(error),
        } => {
            restore_unsent_input(chat, &format!("!{command}"));
            chat.set_notice(format!("Shell command was not sent: {error}"));
        }
        ChatRemoteResult::RemoveQueuedPrompt { result: Ok(()), .. } => {
            chat.set_notice("Queued prompt removed")
        }
        ChatRemoteResult::RemoveQueuedPrompt {
            id,
            text,
            kind,
            result: Err(error),
        } => {
            chat.fail_queued_prompt_removal(id, text, kind);
            chat.set_notice(format!("Queued prompt was not removed: {error}"));
        }
        ChatRemoteResult::SetConfig { result: Ok(()), .. } => {
            chat.set_notice("Configuration update accepted")
        }
        ChatRemoteResult::SetConfig {
            key,
            value,
            result: Err(error),
        } => {
            restore_unsent_input(chat, &config_command_text(&key, &value));
            chat.set_notice(format!("Configuration was not changed: {error}"));
        }
        ChatRemoteResult::PlanCommand {
            requested_active,
            result: Ok(ordinal),
            ..
        } => {
            chat.plan_command_pending = false;
            chat.finish_plan_mode_change(requested_active);
            chat.set_notice(match ordinal {
                Some(ordinal) => format!("Prompt accepted by relay at {ordinal}"),
                None if requested_active => "Plan mode on".to_owned(),
                None => "Plan mode off".to_owned(),
            });
        }
        ChatRemoteResult::PlanCommand {
            original,
            requested_active,
            control_applied,
            result: Err(error),
        } => {
            chat.plan_command_pending = false;
            chat.finish_plan_mode_change(control_applied == requested_active);
            restore_unsent_input(chat, &original);
            chat.set_notice(format!("Plan command was not completed: {error}"));
        }
        ChatRemoteResult::Cancel(Ok(())) => chat.set_notice("Cancellation requested"),
        ChatRemoteResult::Cancel(Err(error)) => {
            chat.set_notice(format!("Cancellation failed: {error}"))
        }
        ChatRemoteResult::RespondElicitation {
            desired_plan_active,
            result: Ok(()),
            ..
        } => {
            if let Some(active) = desired_plan_active {
                chat.finish_plan_mode_change(active);
            }
            chat.set_notice("Answer sent")
        }
        ChatRemoteResult::RespondElicitation {
            request,
            answered,
            result: Err(error),
            ..
        } => {
            if !answered {
                chat.restore_elicitation(request);
            }
            chat.set_notice(format!("Answer was not sent: {error}"));
        }
        ChatRemoteResult::WorkerFailed(error) => chat.set_notice(error),
    }
}

pub(super) fn queue_chat_remote_operation(
    operations: &tokio::sync::mpsc::Sender<ChatRemoteOperation>,
    operation: ChatRemoteOperation,
    chat: &mut ChatState,
) {
    if let Err(error) = operations.try_send(operation) {
        let operation = error.into_inner();
        match operation {
            ChatRemoteOperation::Prompt { text, .. } => restore_unsent_input(chat, &text),
            ChatRemoteOperation::RunShell { command, .. } => {
                restore_unsent_input(chat, &format!("!{command}"));
            }
            ChatRemoteOperation::RemoveQueuedPrompt { id, text, kind, .. } => {
                chat.fail_queued_prompt_removal(id, text, kind);
            }
            ChatRemoteOperation::SetConfig { key, value, .. } => {
                restore_unsent_input(chat, &config_command_text(&key, &value));
            }
            ChatRemoteOperation::PlanCommand {
                original,
                requested_active,
                ..
            } => {
                chat.plan_command_pending = false;
                chat.finish_plan_mode_change(!requested_active);
                restore_unsent_input(chat, &original);
            }
            ChatRemoteOperation::RespondElicitation { request, .. } => {
                chat.restore_elicitation(request)
            }
            ChatRemoteOperation::Sync | ChatRemoteOperation::Cancel { .. } => {}
        }
        chat.set_notice("The session command queue is full; the command was not sent");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hel_chat::test_support::snapshot;

    #[tokio::test]
    async fn prompt_reacquires_replacement_actor_before_dispatch() {
        let fixture = mj_controller::hel_session_manager::replacement_session_test_fixture(
            "session-replaced",
            73,
        );
        let mut remote = ChatRemoteSupervisor::spawn(fixture.stopped, fixture.control);

        remote
            .operations()
            .send(ChatRemoteOperation::Prompt {
                command_id: "prompt-1".into(),
                text: "keep going".into(),
            })
            .await
            .unwrap();

        let result = tokio::time::timeout(std::time::Duration::from_secs(1), remote.recv())
            .await
            .expect("replacement dispatch completed")
            .expect("remote worker stayed open");
        assert!(matches!(
            result,
            ChatRemoteResult::Prompt {
                text,
                result: Ok(73)
            } if text == "keep going"
        ));
    }

    #[test]
    fn full_remote_queue_restores_unsent_input_without_blocking() {
        let (operations, _receiver) = tokio::sync::mpsc::channel(1);
        operations.try_send(ChatRemoteOperation::Sync).unwrap();
        let mut chat = ChatState::new(&snapshot(), &[]);
        chat.set_input("new draft".into());

        queue_chat_remote_operation(
            &operations,
            ChatRemoteOperation::Prompt {
                command_id: "prompt-1".into(),
                text: "unsent prompt".into(),
            },
            &mut chat,
        );

        assert_eq!(chat.input, "unsent prompt\n\nnew draft");
        assert!(
            chat.notice()
                .as_deref()
                .is_some_and(|notice| notice.contains("queue is full"))
        );
    }

    #[test]
    fn failed_plan_control_restores_the_full_command_and_rolls_back_mode() {
        let mut chat = crate::hel_chat::test_support::grok_chat();
        chat.finish_plan_mode_change(true);
        chat.plan_command_pending = true;

        apply_chat_remote_result(
            &mut chat,
            ChatRemoteResult::PlanCommand {
                original: "/plan inspect this".into(),
                requested_active: true,
                control_applied: false,
                result: Err("rejected".into()),
            },
        );

        assert_eq!(chat.current_mode(), Some("default"));
        assert_eq!(chat.input, "/plan inspect this");
        assert!(!chat.plan_command_pending);
    }

    #[test]
    fn prompt_failure_after_plan_control_keeps_the_requested_mode() {
        let mut chat = crate::hel_chat::test_support::grok_chat();
        chat.plan_command_pending = true;

        apply_chat_remote_result(
            &mut chat,
            ChatRemoteResult::PlanCommand {
                original: "/plan inspect this".into(),
                requested_active: true,
                control_applied: true,
                result: Err("mode changed, but prompt failed".into()),
            },
        );

        assert_eq!(chat.current_mode(), Some("plan"));
        assert_eq!(chat.input, "/plan inspect this");
    }

    #[test]
    fn relay_acceptance_reports_the_durable_ordinal() {
        let mut chat = ChatState::new(&snapshot(), &[]);
        apply_chat_remote_result(
            &mut chat,
            ChatRemoteResult::Prompt {
                text: "ship it".into(),
                result: Ok(42),
            },
        );
        assert!(
            chat.notice()
                .as_deref()
                .is_some_and(|notice| notice.contains("accepted by relay at 42"))
        );
    }

    #[test]
    fn full_remote_queue_restores_a_shell_command_with_its_prefix() {
        let (operations, _receiver) = tokio::sync::mpsc::channel(1);
        operations.try_send(ChatRemoteOperation::Sync).unwrap();
        let mut chat = ChatState::new(&snapshot(), &[]);

        queue_chat_remote_operation(
            &operations,
            ChatRemoteOperation::RunShell {
                command_id: "shell-1".into(),
                command: "cargo test".into(),
            },
            &mut chat,
        );

        assert_eq!(chat.input, "!cargo test");
    }

    #[test]
    fn failed_fast_update_restores_the_user_facing_toggle_command() {
        let mut chat = ChatState::new(&snapshot(), &[]);

        apply_chat_remote_result(
            &mut chat,
            ChatRemoteResult::SetConfig {
                key: "fast-mode".into(),
                value: "on".into(),
                result: Err("rejected".into()),
            },
        );

        assert_eq!(chat.input, "/fast");
        assert_eq!(
            chat.notice().as_deref(),
            Some("Configuration was not changed: rejected")
        );
    }
}
