//! The chat's background command channel: every relay operation the view can
//! ask for, the task that runs them, and how their results land back in state.

use agent_client_protocol::schema::v1::{ContentBlock, TextContent};

use mj_core::elicitation::{ElicitationRequest, ElicitationResponse};
use mj_core::state::{QueuedCommandKind, config_command_text};

use mj_client::session::{
    SessionControl as SessionManagerControl, SessionHandle as ManagedSessionHandle,
};
use mj_core::relay::RelayCommand;

use super::PromptImage;
use super::attachments;
use super::{
    ChatState, PlanControl, PlanReviewFollowup, PromptPayload, TurnControlIntent, UnsentKind,
};
#[cfg(test)]
use crate::clipboard::ClipboardImage;

const CHAT_REMOTE_QUEUE_CAPACITY: usize = 32;
const SESSION_ACTOR_REPLACEMENT_WAIT: std::time::Duration = std::time::Duration::from_secs(5);

#[derive(Debug)]
pub(super) enum ChatRemoteOperation {
    Sync,
    RecordNotice {
        id: String,
        text: String,
    },
    Prompt {
        command_id: String,
        text: String,
        images: Vec<PromptImage>,
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
    StopBackgroundTask {
        id: String,
    },
    GoalControl {
        command_id: String,
        action: mj_core::goal::GoalControlAction,
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
        intent: TurnControlIntent,
        cancel_agent: bool,
        command: Option<RelayCommand>,
        shell_command_ids: Vec<String>,
    },
    RespondElicitation {
        request: ElicitationRequest,
        response: ElicitationResponse,
        plan_followup: Option<PlanReviewFollowup>,
    },
}

impl ChatRemoteOperation {
    fn feedback(&self) -> Option<(String, String)> {
        match self {
            Self::RemoveQueuedPrompt { id, .. } => {
                Some((format!("remove:{id}"), "Removing queued prompt…".into()))
            }
            Self::GoalControl { action, .. } => {
                Some((format!("goal:{}", action.as_str()), "Updating goal…".into()))
            }
            Self::SetConfig { key, .. } => {
                Some((format!("config:{key}"), format!("Changing {key}…")))
            }
            Self::PlanCommand { command_id, .. } => {
                Some((format!("plan:{command_id}"), "Changing plan mode…".into()))
            }
            Self::Cancel { intent, .. } => Some((
                "turn-control".into(),
                match intent {
                    TurnControlIntent::Cancel => "Interrupting turn…",
                    TurnControlIntent::Steer => "Steering turn…",
                }
                .into(),
            )),
            Self::RespondElicitation { request, .. } => {
                Some((format!("answer:{}", request.id), "Sending answer…".into()))
            }
            _ => None,
        }
    }
}

#[derive(Debug)]
pub(super) enum ChatRemoteResult {
    Sync(std::result::Result<(), String>),
    NoticeRecorded(std::result::Result<(), String>),
    Prompt {
        command_id: String,
        text: String,
        images: Vec<PromptImage>,
        result: std::result::Result<u64, String>,
    },
    RunShell {
        command_id: String,
        command: String,
        result: std::result::Result<u64, String>,
    },
    RemoveQueuedPrompt {
        id: String,
        text: String,
        kind: QueuedCommandKind,
        result: std::result::Result<(), String>,
    },
    StopBackgroundTask {
        id: String,
        result: std::result::Result<(), String>,
    },
    GoalControl {
        action: mj_core::goal::GoalControlAction,
        result: std::result::Result<(), String>,
    },
    SetConfig {
        key: String,
        value: String,
        result: std::result::Result<(), String>,
    },
    PlanCommand {
        command_id: String,
        original: String,
        requested_active: bool,
        control_applied: bool,
        result: std::result::Result<Option<u64>, String>,
    },
    Cancel {
        intent: TurnControlIntent,
        result: std::result::Result<(), String>,
    },
    RespondElicitation {
        request: ElicitationRequest,
        desired_plan_active: Option<bool>,
        answered: bool,
        result: std::result::Result<(), String>,
    },
    DeliveryUnconfirmed {
        command_id: String,
        error: String,
    },
    FollowupUnconfirmed {
        command_id: String,
        feedback_key: String,
        desired_plan_active: Option<bool>,
        plan_command: bool,
        error: String,
    },
    WorkerFailed(String),
}

impl ChatRemoteResult {
    fn feedback_key(&self) -> Option<String> {
        match self {
            Self::FollowupUnconfirmed { feedback_key, .. } => Some(feedback_key.clone()),
            Self::RemoveQueuedPrompt { id, .. } => Some(format!("remove:{id}")),
            Self::GoalControl { action, .. } => Some(format!("goal:{}", action.as_str())),
            Self::SetConfig { key, .. } => Some(format!("config:{key}")),
            Self::PlanCommand { command_id, .. } => Some(format!("plan:{command_id}")),
            Self::Cancel { .. } => Some("turn-control".into()),
            Self::RespondElicitation { request, .. } => Some(format!("answer:{}", request.id)),
            _ => None,
        }
    }

    fn failure_message(&self) -> Option<&str> {
        match self {
            Self::Sync(Err(error))
            | Self::NoticeRecorded(Err(error))
            | Self::Prompt {
                result: Err(error), ..
            }
            | Self::RunShell {
                result: Err(error), ..
            }
            | Self::RemoveQueuedPrompt {
                result: Err(error), ..
            }
            | Self::StopBackgroundTask {
                result: Err(error), ..
            }
            | Self::GoalControl {
                result: Err(error), ..
            }
            | Self::SetConfig {
                result: Err(error), ..
            }
            | Self::PlanCommand {
                result: Err(error), ..
            }
            | Self::Cancel {
                result: Err(error), ..
            }
            | Self::RespondElicitation {
                result: Err(error), ..
            }
            | Self::FollowupUnconfirmed { error, .. }
            | Self::DeliveryUnconfirmed { error, .. }
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
        ChatRemoteOperation::RecordNotice { id, text } => {
            let session = session.clone();
            let results = results.clone();
            let attached = attached.clone();
            pending.spawn(async move {
                let result = session
                    .submit(id, RelayCommand::RecordNotice { text })
                    .await
                    .map(|_| ())
                    .map_err(|error| format!("{error:#}"));
                publish_chat_remote_result(
                    &results,
                    &attached,
                    ChatRemoteResult::NoticeRecorded(result),
                );
            });
        }

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
        ChatRemoteOperation::Prompt {
            command_id,
            text,
            images,
        } => {
            let original_images = images.clone();
            let session_id = session.session_id().to_owned();
            let normalized = match tokio::task::spawn_blocking(move || {
                normalize_prompt_images(&session_id, images)
            })
            .await
            {
                Ok(Ok(images)) => images,
                Ok(Err(error)) => {
                    publish_chat_remote_result(
                        results,
                        attached,
                        ChatRemoteResult::Prompt {
                            command_id,
                            text,
                            images: original_images,
                            result: Err(format!("prepare image attachments: {error:#}")),
                        },
                    );
                    return;
                }
                Err(error) => {
                    publish_chat_remote_result(
                        results,
                        attached,
                        ChatRemoteResult::Prompt {
                            command_id,
                            text,
                            images: original_images,
                            result: Err(format!("image preparation task failed: {error}")),
                        },
                    );
                    return;
                }
            };
            let prompt = PromptPayload {
                text: text.clone(),
                images: normalized.clone(),
            }
            .content_blocks();
            let command = RelayCommand::Prompt { prompt };
            if serde_json::to_vec(&command)
                .is_ok_and(|bytes| bytes.len() > mj_core::relay::RELAY_COMMAND_BYTE_BUDGET)
            {
                publish_chat_remote_result(
                    results,
                    attached,
                    ChatRemoteResult::Prompt {
                        command_id,
                        text,
                        images: normalized.clone(),
                        result: Err(
                            "Prompt exceeds the relay command budget; shorten the text".into()
                        ),
                    },
                );
                return;
            }
            if !normalized.is_empty()
                && !session
                    .view()
                    .snapshot
                    .is_some_and(|snapshot| snapshot.operational.accepts_prompt_images())
            {
                publish_chat_remote_result(
                    results,
                    attached,
                    ChatRemoteResult::Prompt {
                        command_id,
                        text,
                        images: normalized,
                        result: Err(super::input_state::IMAGE_CAPABILITY_NOTICE.into()),
                    },
                );
                return;
            }
            let response = session.enqueue_submit(command_id.clone(), command).await;
            match response {
                Ok(response) => {
                    let results = results.clone();
                    let attached = attached.clone();
                    pending.spawn(async move {
                        let ordinal = match response.wait().await {
                            Ok(ordinal) => ordinal,
                            Err(error) => {
                                if error.is::<mj_client::session::DeliveryUnconfirmed>() {
                                    publish_chat_remote_result(
                                        &results,
                                        &attached,
                                        ChatRemoteResult::DeliveryUnconfirmed {
                                            command_id,
                                            error: format!("{error:#}"),
                                        },
                                    );
                                    return;
                                }
                                publish_chat_remote_result(
                                    &results,
                                    &attached,
                                    ChatRemoteResult::Prompt {
                                        command_id,
                                        text,
                                        images: normalized.clone(),
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
                                command_id,
                                text: text.clone(),
                                images: normalized.clone(),
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
                            command_id,
                            text,
                            images: normalized,
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
                    command_id.clone(),
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
                        let result = response.wait().await;
                        if let Err(error) = &result
                            && error.is::<mj_client::session::DeliveryUnconfirmed>()
                        {
                            publish_chat_remote_result(
                                &results,
                                &attached,
                                ChatRemoteResult::DeliveryUnconfirmed {
                                    command_id,
                                    error: format!("{error:#}"),
                                },
                            );
                            return;
                        }
                        let result = result.map_err(|error| format!("{error:#}"));
                        publish_chat_remote_result(
                            &results,
                            &attached,
                            ChatRemoteResult::RunShell {
                                command_id,
                                command,
                                result,
                            },
                        );
                    });
                }
                Err(error) => publish_chat_remote_result(
                    results,
                    attached,
                    ChatRemoteResult::RunShell {
                        command_id,
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
        ChatRemoteOperation::StopBackgroundTask { id } => {
            let session = session.clone();
            let results = results.clone();
            let attached = attached.clone();
            pending.spawn(async move {
                let result = session
                    .stop_background_task(id.clone())
                    .await
                    .map_err(|error| format!("{error:#}"));
                publish_chat_remote_result(
                    &results,
                    &attached,
                    ChatRemoteResult::StopBackgroundTask { id, result },
                );
            });
        }
        ChatRemoteOperation::GoalControl { command_id, action } => {
            let response = session
                .enqueue_submit(command_id, RelayCommand::GoalControl { action })
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
                            ChatRemoteResult::GoalControl { action, result },
                        );
                    });
                }
                Err(error) => publish_chat_remote_result(
                    results,
                    attached,
                    ChatRemoteResult::GoalControl {
                        action,
                        result: Err(format!("{error:#}")),
                    },
                ),
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
                let mut unconfirmed = false;
                let result = async {
                    session
                        .apply_plan_control(command_id.clone(), control)
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
                        .map_err(|error| {
                            unconfirmed = error.is::<mj_client::session::DeliveryUnconfirmed>();
                            format!("mode changed, but prompt failed: {error:#}")
                        })?;
                    Ok(Some(ordinal))
                }
                .await;
                publish_chat_remote_result(
                    &results,
                    &attached,
                    if unconfirmed {
                        ChatRemoteResult::FollowupUnconfirmed {
                            command_id: format!("{command_id}-prompt"),
                            feedback_key: format!("plan:{command_id}"),
                            desired_plan_active: Some(requested_active),
                            plan_command: true,
                            error: result.expect_err("unconfirmed delivery failed"),
                        }
                    } else {
                        ChatRemoteResult::PlanCommand {
                            command_id,
                            original,
                            requested_active,
                            control_applied,
                            result,
                        }
                    },
                );
            });
        }
        ChatRemoteOperation::Cancel {
            command_id,
            intent,
            cancel_agent,
            command,
            shell_command_ids,
        } => {
            let session = session.clone();
            let results = results.clone();
            let attached = attached.clone();
            pending.spawn(async move {
                let mut failures = Vec::new();
                if cancel_agent
                    && let Err(error) = session
                        .submit(
                            command_id.clone(),
                            command.unwrap_or(RelayCommand::CancelTurn),
                        )
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
                    ChatRemoteResult::Cancel {
                        intent,
                        result: if failures.is_empty() {
                            Ok(())
                        } else {
                            Err(failures.join("; "))
                        },
                    },
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
                let mut unconfirmed = false;
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
                        session
                            .apply_plan_control(format!("plan-review-{}-mode", request.id), control)
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
                                unconfirmed = error.is::<mj_client::session::DeliveryUnconfirmed>();
                                format!("review answered, but revision feedback failed: {error:#}")
                            })?;
                    }
                    Ok(())
                }
                .await;
                publish_chat_remote_result(
                    &results,
                    &attached,
                    if unconfirmed {
                        ChatRemoteResult::FollowupUnconfirmed {
                            command_id: format!("plan-review-{}-feedback", request.id),
                            feedback_key: format!("answer:{}", request.id),
                            desired_plan_active,
                            plan_command: false,
                            error: result.expect_err("unconfirmed delivery failed"),
                        }
                    } else {
                        ChatRemoteResult::RespondElicitation {
                            request,
                            desired_plan_active,
                            answered,
                            result,
                        }
                    },
                );
            });
        }
    }
}

/// Convert legacy inline images to session references before the relay command
/// is serialized. Every image must be decoded, optimized, and installed; a
/// stale or unreadable attachment must be reported before the command leaves
/// the controller.
fn normalize_prompt_images(
    session_id: &str,
    images: Vec<PromptImage>,
) -> anyhow::Result<Vec<PromptImage>> {
    images
        .into_iter()
        .map(|mut image| {
            image.image = attachments::install_clipboard_image(session_id, image.image)?;
            Ok(image)
        })
        .collect()
}

pub(super) fn restore_unsent_input(chat: &mut ChatState, input: &str) {
    restore_unsent_prompt(chat, input.to_owned(), Vec::new());
}

pub(super) fn restore_unsent_prompt(chat: &mut ChatState, text: String, images: Vec<PromptImage>) {
    let mut payload = PromptPayload { text, images };
    if chat.draft_payload() == payload {
        return;
    }
    if !chat.input.is_empty() {
        payload.text.push_str("\n\n");
    }
    chat.replace_input_range(0..0, &payload);
}

pub(super) fn apply_chat_remote_result(chat: &mut ChatState, result: ChatRemoteResult) {
    if let Some(key) = result.feedback_key()
        && !matches!(result, ChatRemoteResult::Cancel { result: Ok(()), .. })
    {
        chat.operation_feedback.remove(&key);
    }
    // Keep diagnostics in the process log as well as in recoverable rows.
    // Publishing a conversation notice must never recursively publish failure.
    if let Some(error) = result.failure_message() {
        tracing::warn!(
            session_id = %chat.session_id,
            %error,
            "chat operation failed and was shown in the UI"
        );
    }
    match result {
        // The local row remains until projection confirms publication; failure
        // is logged once without recursively trying to publish another notice.
        ChatRemoteResult::NoticeRecorded(_) => {}

        ChatRemoteResult::Sync(Ok(())) => {
            chat.connection_feedback = None;
            chat.set_transcript_loading(false);
        }
        ChatRemoteResult::Sync(Err(error)) => {
            chat.set_transcript_loading(false);
            chat.set_connection_notice(format!("Connection failed: {error}"))
        }
        ChatRemoteResult::Prompt {
            command_id,
            text,
            images,
            result: Ok(_),
        } => {
            chat.finish_submission(&command_id, true);
            // The same text has now reached the relay, so the record of the
            // earlier refusal has nothing left to report.
            chat.clear_unsent_prompt(UnsentKind::Prompt, &text, &images);
        }
        ChatRemoteResult::Prompt {
            command_id,
            text,
            images,
            result: Err(error),
        } => {
            if !chat.finish_submission(&command_id, false) {
                return;
            }
            restore_unsent_prompt(chat, text.clone(), images.clone());

            chat.record_unsent_prompt(UnsentKind::Prompt, text, images, error);
        }
        ChatRemoteResult::RunShell {
            command_id,
            command,
            result: Ok(_),
        } => {
            chat.finish_submission(&command_id, true);
            chat.clear_unsent_prompt(UnsentKind::Shell, &command, &[]);
        }
        ChatRemoteResult::RunShell {
            command_id,
            command,
            result: Err(error),
        } => {
            if !chat.finish_submission(&command_id, false) {
                return;
            }
            restore_unsent_input(chat, &format!("!{command}"));

            chat.record_unsent_prompt(UnsentKind::Shell, command, Vec::new(), error);
        }
        ChatRemoteResult::RemoveQueuedPrompt { result: Ok(()), .. } => {}
        ChatRemoteResult::RemoveQueuedPrompt {
            id,
            text,
            kind,
            result: Err(error),
        } => {
            chat.fail_queued_prompt_removal(id, text, kind);
            chat.conversation_notice(format!("Queued prompt was not removed: {error}"));
        }
        ChatRemoteResult::StopBackgroundTask { result: Ok(()), .. } => {
            // The provider's acknowledgement only means it accepted the stop
            // request. The next activity snapshot remains authoritative for
            // removing the row and its pending label.
        }
        ChatRemoteResult::StopBackgroundTask {
            id,
            result: Err(error),
        } => chat.fail_background_stop(&id, &error),
        ChatRemoteResult::GoalControl { action, result } => match result {
            Ok(()) => {}
            Err(error) => {
                restore_unsent_input(chat, &format!("/goal {}", action.as_str()));
                chat.conversation_notice(format!("/goal {} failed: {error}", action.as_str()));
            }
        },
        ChatRemoteResult::SetConfig { result: Ok(()), .. } => {}
        ChatRemoteResult::SetConfig {
            key,
            value,
            result: Err(error),
        } => {
            restore_unsent_input(chat, &config_command_text(&key, &value));
            chat.conversation_notice(format!("Configuration was not changed: {error}"));
        }
        ChatRemoteResult::PlanCommand {
            command_id,
            requested_active,
            result: Ok(_),
            ..
        } => {
            chat.plan_command_pending = false;
            chat.finish_plan_mode_change(requested_active);
            chat.finish_submission(&format!("{command_id}-prompt"), true);
        }
        ChatRemoteResult::PlanCommand {
            command_id,
            original,
            requested_active,
            control_applied,
            result: Err(error),
        } => {
            let unsent = chat.finish_submission(&format!("{command_id}-prompt"), false);
            chat.plan_command_pending = false;
            chat.finish_plan_mode_change(control_applied == requested_active);
            if unsent {
                restore_unsent_input(chat, &original);
            }
            chat.conversation_notice(format!("Plan command was not completed: {error}"));
        }
        ChatRemoteResult::Cancel { intent, result } => {
            chat.turn_control_submitting = false;
            if result.is_err() {
                chat.turn_control_awaiting_state = None;
            }
            if chat.turn_control_awaiting_state.is_none()
                && chat.cancelling_prompt_id.is_none()
                && !chat.steering.as_ref().is_some_and(|s| s.holds_queue())
            {
                chat.operation_feedback.remove("turn-control");
            }
            if let Err(error) = result {
                chat.turn_control_error = Some(intent.failure_notice(&error));
                chat.turn_control_dialog_open = intent != TurnControlIntent::Cancel;
                chat.conversation_notice(intent.failure_notice(&error));
            }
        }
        ChatRemoteResult::RespondElicitation {
            request,
            desired_plan_active,
            result: Ok(()),
            ..
        } => {
            if let Some(active) = desired_plan_active {
                chat.finish_plan_mode_change(active);
            }
            chat.finish_submission(&format!("plan-review-{}-feedback", request.id), true);
        }
        ChatRemoteResult::RespondElicitation {
            request,
            answered,
            result: Err(error),
            ..
        } => {
            chat.finish_submission(&format!("plan-review-{}-feedback", request.id), false);
            if !answered {
                chat.restore_elicitation(request);
            }
            chat.conversation_notice(if answered {
                format!("Answer sent; follow-up failed: {error}")
            } else {
                format!("Answer was not sent: {error}")
            });
        }
        ChatRemoteResult::FollowupUnconfirmed {
            command_id,
            desired_plan_active,
            plan_command,
            error,
            ..
        } => {
            if plan_command {
                chat.plan_command_pending = false;
            }
            if let Some(active) = desired_plan_active {
                chat.finish_plan_mode_change(active);
            }
            chat.unconfirm_submission(&command_id, &error);
        }
        ChatRemoteResult::DeliveryUnconfirmed { command_id, error } => {
            chat.unconfirm_submission(&command_id, &error)
        }
        ChatRemoteResult::WorkerFailed(error) => {
            chat.unconfirm_all_submissions(&error);
            chat.operation_feedback.clear();
            if chat.fail_all_background_stops() {
                chat.conversation_notice(format!("Background task could not be stopped: {error}"));
            } else {
                chat.conversation_notice(error);
            }
        }
    }
}

pub(super) fn queue_chat_remote_operation(
    operations: &tokio::sync::mpsc::Sender<ChatRemoteOperation>,
    operation: ChatRemoteOperation,
    chat: &mut ChatState,
) {
    let feedback_key = operation.feedback().map(|(key, text)| {
        chat.operation_feedback.insert(key.clone(), text);
        key
    });
    match &operation {
        ChatRemoteOperation::Prompt {
            command_id,
            text,
            images,
        } => chat.begin_submission(
            command_id.clone(),
            UnsentKind::Prompt,
            PromptPayload {
                text: text.clone(),
                images: images.clone(),
            },
            "Sending…",
        ),
        ChatRemoteOperation::RunShell {
            command_id,
            command,
        } => chat.begin_submission(
            command_id.clone(),
            UnsentKind::Shell,
            PromptPayload::text(command.clone()),
            "Sending…",
        ),
        ChatRemoteOperation::PlanCommand {
            command_id,
            prompt: Some(text),
            ..
        } => chat.begin_submission(
            format!("{command_id}-prompt"),
            UnsentKind::Prompt,
            PromptPayload::text(text.clone()),
            "Changing mode…",
        ),
        ChatRemoteOperation::RespondElicitation {
            request,
            plan_followup: Some(followup),
            ..
        } => {
            if let Some(text) = &followup.prompt {
                chat.begin_submission(
                    format!("plan-review-{}-feedback", request.id),
                    UnsentKind::Prompt,
                    PromptPayload::text(text.clone()),
                    "Sending answer…",
                );
            }
        }
        _ => {}
    }
    if let Err(error) = operations.try_send(operation) {
        if let Some(key) = feedback_key {
            chat.operation_feedback.remove(&key);
        }
        let operation = error.into_inner();
        match operation {
            ChatRemoteOperation::Prompt {
                command_id,
                text,
                images,
            } => {
                chat.finish_submission(&command_id, false);
                restore_unsent_prompt(chat, text.clone(), images.clone());
                chat.record_unsent_prompt(
                    UnsentKind::Prompt,
                    text,
                    images,
                    "session command queue is full".into(),
                );
            }
            ChatRemoteOperation::RunShell {
                command_id,
                command,
            } => {
                chat.finish_submission(&command_id, false);
                chat.record_unsent_prompt(
                    UnsentKind::Shell,
                    command.clone(),
                    Vec::new(),
                    "session command queue is full".into(),
                );
                restore_unsent_input(chat, &format!("!{command}"));
            }
            ChatRemoteOperation::RemoveQueuedPrompt { id, text, kind, .. } => {
                chat.fail_queued_prompt_removal(id, text, kind);
            }
            ChatRemoteOperation::StopBackgroundTask { id } => {
                chat.fail_background_stop(&id, "session command queue is full");
            }
            ChatRemoteOperation::GoalControl { action, .. } => {
                restore_unsent_input(chat, &format!("/goal {}", action.as_str()));
            }
            ChatRemoteOperation::SetConfig { key, value, .. } => {
                restore_unsent_input(chat, &config_command_text(&key, &value));
            }
            ChatRemoteOperation::PlanCommand {
                command_id,
                original,
                requested_active,
                ..
            } => {
                chat.finish_submission(&format!("{command_id}-prompt"), false);
                chat.plan_command_pending = false;
                chat.finish_plan_mode_change(!requested_active);
                restore_unsent_input(chat, &original);
            }
            ChatRemoteOperation::RespondElicitation { request, .. } => {
                chat.finish_submission(&format!("plan-review-{}-feedback", request.id), false);
                chat.restore_elicitation(request)
            }
            ChatRemoteOperation::Cancel { intent, .. } => {
                chat.turn_control_submitting = false;
                chat.turn_control_awaiting_state = None;
                chat.turn_control_error =
                    Some(intent.failure_notice("session command queue is full"));
                chat.turn_control_dialog_open = intent == TurnControlIntent::Steer;
            }
            ChatRemoteOperation::Sync | ChatRemoteOperation::RecordNotice { .. } => {}
        }
        chat.set_notice("The session command queue is full; the command was not sent");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chat::test_support::{snapshot, transcript_text};
    use base64::Engine as _;
    use mj_core::state::MaterializedSession;

    fn valid_image() -> ClipboardImage {
        let mut bytes = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut bytes, 2, 2);
            encoder.set_color(png::ColorType::Rgba);
            encoder.set_depth(png::BitDepth::Eight);
            let mut writer = encoder.write_header().unwrap();
            writer
                .write_image_data(&[
                    255, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, 255, 255, 0, 255,
                ])
                .unwrap();
        }
        ClipboardImage::from_png_base64(base64::engine::general_purpose::STANDARD.encode(bytes))
            .unwrap()
    }

    /// Whether any transcript row contains `text`, at a width wide enough that
    /// nothing under test wraps.
    fn transcript_shows(chat: &mut ChatState, text: &str) -> bool {
        transcript_text(chat, 100)
            .iter()
            .any(|line| line.contains(text))
    }

    #[test]
    fn rejected_steering_is_not_reported_as_failed_cancellation() {
        let mut chat = ChatState::new(&snapshot(), &[]);
        apply_chat_remote_result(
            &mut chat,
            ChatRemoteResult::Cancel {
                intent: TurnControlIntent::Steer,
                result: Err("session disconnected".into()),
            },
        );
        assert!(transcript_shows(
            &mut chat,
            "Steering request failed: session disconnected"
        ));
    }

    #[tokio::test]
    async fn prompt_reacquires_replacement_actor_before_dispatch() {
        let fixture = mj_client::session::replacement_session_test_fixture("session-replaced", 73);
        let mut remote = ChatRemoteSupervisor::spawn(fixture.stopped, fixture.control);

        remote
            .operations()
            .send(ChatRemoteOperation::Prompt {
                command_id: "prompt-1".into(),
                text: "keep going".into(),
                images: Vec::new(),
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
                command_id: _,
                text,
                images,
                result: Ok(73)
            } if text == "keep going" && images.is_empty()
        ));
    }

    #[tokio::test]
    async fn image_bytes_reach_the_session_actor_and_oversized_prompts_are_refused_intact() {
        let mut fixture = mj_client::session::replacement_session_test_fixture("image-session", 19);
        let materialized = mj_core::state::MaterializedSession::empty("image-session");
        let mut operational =
            mj_core::relay::RelaySnapshot::new("image-session".into()).operational_state();
        operational.agent_capabilities = Some(Box::new(
            serde_json::from_value(serde_json::json!({"promptCapabilities": {"image": true}}))
                .unwrap(),
        ));
        fixture
            .replacement_view
            .send_replace(mj_client::session::ManagedSessionView {
                connected: true,
                error: None,
                snapshot: Some(mj_core::state::ManagedSessionSnapshot {
                    window: mj_core::state::ProjectionWindow::of(&materialized),
                    materialized,
                    operational,
                    latest_credential_sync_signal: None,
                    worker_build: None,
                    subagent_requests: Vec::new(),
                    subagent_results: Vec::new(),
                }),
            });
        let mut remote = ChatRemoteSupervisor::spawn(fixture.stopped, fixture.control);
        let image = valid_image();
        let mut normalized_image = None;
        for text in ["", "inspect this"] {
            let payload = PromptPayload::with_image(text, image.clone());
            remote
                .operations()
                .send(ChatRemoteOperation::Prompt {
                    command_id: format!("image-{text}"),
                    text: payload.text.clone(),
                    images: payload.images.clone(),
                })
                .await
                .unwrap();
            let command =
                tokio::time::timeout(std::time::Duration::from_secs(2), fixture.submitted.recv())
                    .await
                    .unwrap()
                    .unwrap();
            let RelayCommand::Prompt { prompt } = command else {
                panic!("expected image prompt command");
            };
            let image_block = prompt
                .iter()
                .find_map(|block| match block {
                    ContentBlock::Image(image) => Some(image),
                    ContentBlock::Text(_) => None,
                    _ => None,
                })
                .expect("image prompt contains an image block");
            assert!(image_block.data.is_empty());
            let reference = mj_core::attachment::image_reference(image_block)
                .unwrap()
                .expect("image prompt contains an attachment reference");
            assert_eq!((reference.width, reference.height), (2, 2));

            let Some(ChatRemoteResult::Prompt {
                command_id: _,
                images,
                result: Ok(19),
                ..
            }) = remote.recv().await
            else {
                panic!("expected successful image prompt result");
            };
            assert_eq!(images.len(), 1);
            assert_eq!(images[0].image.reference.as_ref(), Some(&reference));
            normalized_image = Some(images[0].image.clone());
        }
        // The supervisor must consult the latest view, even if the composer
        // queued the image while support was still advertised.
        fixture.replacement_view.send_modify(|view| {
            view.snapshot
                .as_mut()
                .unwrap()
                .operational
                .agent_capabilities = None;
        });
        let revoked = PromptPayload::with_image("keep this draft", image.clone());
        remote
            .operations()
            .send(ChatRemoteOperation::Prompt {
                command_id: "capability-revoked".into(),
                text: revoked.text.clone(),
                images: revoked.images.clone(),
            })
            .await
            .unwrap();
        let result = tokio::time::timeout(std::time::Duration::from_secs(2), remote.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(
            matches!(result, ChatRemoteResult::Prompt { text, images, result: Err(error), .. }
            if text == revoked.text && images.len() == 1 && error.contains("advertised image support"))
        );
        assert!(fixture.submitted.try_recv().is_err());

        let huge =
            PromptPayload::with_image("x".repeat(mj_core::relay::RELAY_COMMAND_BYTE_BUDGET), image);
        remote
            .operations()
            .send(ChatRemoteOperation::Prompt {
                command_id: "too-big".into(),
                text: huge.text.clone(),
                images: huge.images.clone(),
            })
            .await
            .unwrap();
        let result = tokio::time::timeout(std::time::Duration::from_secs(2), remote.recv())
            .await
            .unwrap()
            .unwrap();
        let mut expected_images = huge.images.clone();
        expected_images[0].image = normalized_image.expect("successful image was normalized");
        assert!(matches!(
            result,
            ChatRemoteResult::Prompt {
                command_id: _,
                text,
                images,
                result: Err(_)
            } if text == huge.text && images == expected_images
        ));
        assert!(
            fixture.submitted.try_recv().is_err(),
            "oversized input must not reach the actor"
        );
    }

    #[tokio::test]
    async fn malformed_inline_image_is_refused_before_actor_dispatch() {
        let mut fixture =
            mj_client::session::replacement_session_test_fixture("malformed-image-session", 23);
        let mut remote = ChatRemoteSupervisor::spawn(fixture.stopped, fixture.control);
        let image = ClipboardImage {
            data_base64: "not-base64".into(),
            mime_type: "image/png".into(),
            reference: None,
        };
        let payload = PromptPayload::with_image("inspect", image);

        remote
            .operations()
            .send(ChatRemoteOperation::Prompt {
                command_id: "malformed-image".into(),
                text: payload.text.clone(),
                images: payload.images.clone(),
            })
            .await
            .unwrap();

        let result = tokio::time::timeout(std::time::Duration::from_secs(2), remote.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(
            result,
            ChatRemoteResult::Prompt {
                command_id: _,
                text,
                images,
                result: Err(_)
            } if text == payload.text && images == payload.images
        ));
        assert!(
            fixture.submitted.try_recv().is_err(),
            "malformed image must not reach the actor"
        );
    }

    #[test]
    fn prompt_content_blocks_keep_image_for_image_only_and_mixed_prompts() {
        let image = ClipboardImage {
            data_base64: "encoded-png".into(),
            mime_type: "image/png".into(),
            reference: None,
        };
        let image_only = PromptPayload::with_image("", image.clone()).content_blocks();
        assert!(matches!(
            image_only.as_slice(),
            [ContentBlock::Image(content)]
                if content.data == "encoded-png" && content.mime_type == "image/png"
        ));
        let mixed = PromptPayload::with_image("describe this", image).content_blocks();
        assert!(matches!(
            mixed.as_slice(),
            [ContentBlock::Text(_), ContentBlock::Image(_)]
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
                images: Vec::new(),
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
        let mut chat = crate::chat::test_support::grok_chat();
        chat.finish_plan_mode_change(true);
        chat.plan_command_pending = true;

        apply_chat_remote_result(
            &mut chat,
            ChatRemoteResult::PlanCommand {
                command_id: "test-submit".into(),
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
        let mut chat = crate::chat::test_support::grok_chat();
        chat.plan_command_pending = true;

        apply_chat_remote_result(
            &mut chat,
            ChatRemoteResult::PlanCommand {
                command_id: "test-submit".into(),
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
    fn relay_acceptance_does_not_emit_transport_chatter() {
        let mut chat = ChatState::new(&snapshot(), &[]);
        apply_chat_remote_result(
            &mut chat,
            ChatRemoteResult::Prompt {
                command_id: "test-submit".into(),
                text: "ship it".into(),
                images: Vec::new(),
                result: Ok(42),
            },
        );
        assert!(chat.notice().is_none());
        assert!(chat.conversation_notices.is_empty());
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
    fn a_refused_prompt_stays_in_the_transcript_after_a_projection_rebuild() {
        let mut chat = ChatState::new(&snapshot(), &[]);
        chat.set_input("later draft".into());

        apply_chat_remote_result(
            &mut chat,
            ChatRemoteResult::Prompt {
                command_id: "test-submit".into(),
                text: "read the journal\nand summarise it".into(),
                images: Vec::new(),
                result: Err("relay attach failed".into()),
            },
        );

        // The failure remains in the conversation and preserves the draft.
        assert!(chat.notice().is_none());
        assert_eq!(
            chat.input,
            "read the journal\nand summarise it\n\nlater draft"
        );
        assert!(transcript_shows(
            &mut chat,
            "Prompt was not sent: relay attach failed"
        ));
        assert!(transcript_shows(
            &mut chat,
            "read the journal and summarise it"
        ));
        // The row is timestamped like the rest of the transcript.
        assert!(transcript_shows(&mut chat, "Mjolnir · "));

        // A newer projection rebuilds the entries; the record is client-local
        // and the relay never saw the prompt, so it has to outlive that.
        let mut session = MaterializedSession::empty("1234567890");
        session.applied_event_ordinal = 9;
        chat.apply_materialized(&session, &[], &[]);

        assert!(transcript_shows(
            &mut chat,
            "Prompt was not sent: relay attach failed"
        ));
    }

    #[test]
    fn only_accepting_the_same_text_clears_a_refused_prompt() {
        let mut chat = ChatState::new(&snapshot(), &[]);
        apply_chat_remote_result(
            &mut chat,
            ChatRemoteResult::Prompt {
                command_id: "test-submit".into(),
                text: "read the journal".into(),
                images: Vec::new(),
                result: Err("relay attach failed".into()),
            },
        );

        apply_chat_remote_result(
            &mut chat,
            ChatRemoteResult::Prompt {
                command_id: "test-submit".into(),
                text: "something else entirely".into(),
                images: Vec::new(),
                result: Ok(11),
            },
        );
        assert!(transcript_shows(
            &mut chat,
            "Prompt was not sent: relay attach failed"
        ));

        apply_chat_remote_result(
            &mut chat,
            ChatRemoteResult::Prompt {
                command_id: "test-submit".into(),
                text: "read the journal".into(),
                images: Vec::new(),
                result: Ok(12),
            },
        );
        assert!(!transcript_shows(&mut chat, "Prompt was not sent"));
    }

    #[test]
    fn a_refused_shell_command_is_recorded_the_same_way() {
        let mut chat = ChatState::new(&snapshot(), &[]);

        apply_chat_remote_result(
            &mut chat,
            ChatRemoteResult::RunShell {
                command_id: "test-submit".into(),
                command: "cargo test".into(),
                result: Err("relay attach failed".into()),
            },
        );

        assert_eq!(chat.input, "!cargo test");
        assert!(chat.notice().is_none());
        assert!(transcript_shows(
            &mut chat,
            "Shell command was not sent: relay attach failed"
        ));

        apply_chat_remote_result(
            &mut chat,
            ChatRemoteResult::RunShell {
                command_id: "test-submit".into(),
                command: "cargo test".into(),
                result: Ok(4),
            },
        );
        assert!(!transcript_shows(&mut chat, "Shell command was not sent"));
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
        assert!(transcript_shows(
            &mut chat,
            "Configuration was not changed: rejected"
        ));
    }
}
