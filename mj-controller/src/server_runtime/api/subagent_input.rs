//! Deferred child input uses the parent's existing durable request queue.

use super::*;
use mj_client::session::ViewError;

const START_DEADLINE: Duration = Duration::from_secs(30 * 60);
const INPUT_POLL: Duration = Duration::from_millis(250);
const MIN_SESSION_ID_PREFIX_LENGTH: usize = 8;
use mj_core::subagent::{SubagentToolAction, SubagentToolRequest};

pub(super) enum SubagentInputDelivery {
    Mailbox,
    Turn { ordinal: u64 },
}

#[derive(Debug)]
pub(super) struct InputDeliveryFailure {
    pub via: &'static str,
    source: anyhow::Error,
}

impl std::fmt::Display for InputDeliveryFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{} delivery failed: {:#}", self.via, self.source)
    }
}

impl std::error::Error for InputDeliveryFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.source.as_ref())
    }
}

impl SubagentInputDelivery {
    pub(super) const fn via(&self) -> &'static str {
        match self {
            Self::Mailbox => "mailbox",
            Self::Turn { .. } => "turn",
        }
    }
}

pub(super) fn ensure_subagent_child_can_receive_work(
    record: &mj_core::state::SessionRecord,
    work: &str,
) -> Result<()> {
    ensure!(
        matches!(
            record.state,
            SessionState::Provisioning
                | SessionState::Running
                | SessionState::Disconnected
                | SessionState::Checkpointing
                | SessionState::Parked
        ),
        "child session is {:?}; queued {work} was not delivered{}",
        record.state,
        record
            .last_error
            .as_ref()
            .map(|error| format!(": {error}"))
            .unwrap_or_default()
    );
    Ok(())
}

impl ApiBackend {
    /// One authorization and routing operation shared by MCP and the
    /// authenticated HTTP/CLI surface. Sender identity is supplied by the
    /// caller context, never by MCP tool arguments.
    pub(super) async fn deliver_session_message(
        &self,
        sender_session_id: Option<String>,
        requested_target_id: String,
        text: String,
        request_id: String,
        created_at_ms: i64,
    ) -> Result<crate::server::api::SessionMessageResponse> {
        let requested_target_for_auth = requested_target_id.clone();
        let sender_for_auth = sender_session_id.clone();
        let (target_session_id, own_child, sender_record) =
            blocking("authorize session message", move || {
            let state = Controller::load()?.state;
            let sender = match sender_for_auth.as_deref() {
                Some(sender_id) => {
                    let sender = state.sessions.get(sender_id).ok_or_else(|| {
                        anyhow::Error::new(mj_core::refusal::Refusal::unusable(format!(
                            "sender session {sender_id} does not exist"
                        )))
                    })?;
                    if state.is_subagent_session(sender_id) {
                        anyhow::bail!(mj_core::refusal::Refusal::unusable(
                            "only a top-level session can send messages"
                        ));
                    }
                    Some(sender)
                }
                None => None,
            };
            let target_id = resolve_session_message_target(
                &state,
                sender_for_auth.as_deref(),
                &requested_target_for_auth,
            )?;
            let target = state.sessions.get(&target_id).cloned().ok_or_else(|| {
                anyhow::Error::new(mj_core::refusal::Refusal::unusable(format!(
                    "target session {} does not exist or was destroyed",
                    requested_target_for_auth
                )))
            })?;
            if sender_for_auth.as_deref() == Some(target_id.as_str()) {
                anyhow::bail!(mj_core::refusal::Refusal::unusable(
                    "a session cannot send a message to itself"
                ));
            }
            let own_child = state.is_subagent_session(&target_id)
                && sender_for_auth.as_deref().is_some_and(|sender_id| {
                    state
                        .subagents
                        .get(&target_id)
                        .is_some_and(|relation| relation.parent_session_id == sender_id)
                });
            if state.is_subagent_session(&target_id) && !own_child {
                anyhow::bail!(mj_core::refusal::Refusal::unusable(
                    "a session cannot message another session's sub-agent"
                ));
            }
            if target.state == SessionState::DestroyedWithDataLoss {
                anyhow::bail!(mj_core::refusal::Refusal::precondition(
                    "target session was destroyed and cannot receive messages"
                ));
            }
            // The person's own message still reaches the session as a turn, as
            // it does with mailboxes off; another session's does not.
            if !own_child && sender.is_some() && target.no_mailbox {
                anyhow::bail!(mj_core::refusal::Refusal::unusable(
                    "target session was started with --no-mailbox and accepts no messages from other sessions"
                ));
            }
            if !own_child {
                if target.state == SessionState::Stopped {
                    anyhow::bail!(mj_core::refusal::Refusal::precondition(
                        "target session is stopped (suspended); resume it before sending a message"
                    ));
                }
                if matches!(
                    target.state,
                    SessionState::Closing
                        | SessionState::Destroying
                        | SessionState::StartupCleanup
                        | SessionState::Lost
                        | SessionState::Error
                        | SessionState::Parked
                ) {
                    anyhow::bail!(mj_core::refusal::Refusal::precondition(format!(
                        "target session is {:?} and cannot receive messages",
                        target.state
                    )));
                }
            }
            Ok((target_id, own_child, sender.cloned()))
        })
        .await?;

        if own_child {
            let parent = sender_session_id
                .as_deref()
                .context("an owned child message requires its parent session")?;
            let request = SubagentToolRequest {
                originating_command_id: None,
                request_id: request_id.clone(),
                created_at_ms,
                action: SubagentToolAction::SendMessage {
                    child_session_id: target_session_id.clone(),
                    message: text.clone(),
                },
            };
            let delivery = self
                .deliver_subagent_input(parent, &target_session_id, &text, &request)
                .await?;
            let (via, turn_id) = match delivery {
                SubagentInputDelivery::Mailbox => ("mailbox", None),
                SubagentInputDelivery::Turn { ordinal } => ("turn", Some(ordinal)),
            };
            return Ok(crate::server::api::SessionMessageResponse {
                session_id: target_session_id,
                via: via.into(),
                turn_id,
                managed_child: true,
            });
        }

        let sender = match (sender_session_id, sender_record.as_ref()) {
            (Some(id), Some(record)) => mj_core::mailbox::Sender::Session {
                id,
                title: record.title.clone(),
            },
            (None, _) => mj_core::mailbox::Sender::User,
            _ => unreachable!("sender record accompanies every session sender id"),
        };
        // The request id is the durable mailbox idempotency key; the row's
        // target is the resolved full session ID.
        let event_key = format!("session-message-{request_id}");
        let mailbox_route_exists = {
            let event_key = event_key.clone();
            blocking("check existing session-message route", move || {
                crate::database::mailbox_event_exists(&event_key)
            })
            .await?
        };
        let event = mj_core::mailbox::MailboxEvent {
            key: event_key.clone(),
            source: "session_message".into(),
            wake: true,
            created_at_ms: created_at_ms.max(0) as u64,
            body: mj_core::mailbox::MailboxEventBody::SessionMessage { from: sender, text },
        };

        if mailbox_route_exists {
            self.enqueue_session_message(&target_session_id, &event, &event_key)
                .await?;
            return Ok(crate::server::api::SessionMessageResponse {
                session_id: target_session_id,
                via: "mailbox".into(),
                turn_id: None,
                managed_child: false,
            });
        }

        let command_id = format!("session-message-turn-{request_id}");
        if let Some(ordinal) = prompt_acceptance(&target_session_id, &command_id).await? {
            return Ok(crate::server::api::SessionMessageResponse {
                session_id: target_session_id,
                via: "turn".into(),
                turn_id: Some(ordinal),
                managed_child: false,
            });
        }

        let protocol = if self.exports.agent_mailboxes_enabled_for(&target_session_id) {
            match self.sessions.session(target_session_id.clone()).await {
                Ok(handle) => {
                    crate::mailbox_outbox::published_worker_relay_protocol(&handle.view())
                }
                Err(error) => {
                    tracing::debug!(
                        target_session_id,
                        error = %format!("{error:#}"),
                        "target worker protocol is not published; session message will use a turn"
                    );
                    None
                }
            }
        } else {
            None
        };
        let mailbox_supported = protocol
            .is_some_and(|protocol| protocol >= mj_core::relay::RELAY_SESSION_MESSAGE_PROTOCOL);
        if mailbox_supported {
            self.enqueue_session_message(&target_session_id, &event, &event_key)
                .await?;
            return Ok(crate::server::api::SessionMessageResponse {
                session_id: target_session_id,
                via: "mailbox".into(),
                turn_id: None,
                managed_child: false,
            });
        }

        let text = mj_core::mailbox::render_mailbox_event(&event);
        let turn_id = self
            .prompt_with_id(target_session_id.clone(), text, Some(command_id))
            .await?;
        Ok(crate::server::api::SessionMessageResponse {
            session_id: target_session_id,
            via: "turn".into(),
            turn_id: Some(turn_id),
            managed_child: false,
        })
    }

    async fn enqueue_session_message(
        &self,
        target: &str,
        event: &mj_core::mailbox::MailboxEvent,
        event_key: &str,
    ) -> Result<()> {
        let event_json = serde_json::to_string(event)?;
        let event_key = event_key.to_owned();
        let target = target.to_owned();
        let admission = crate::upgrade::activity_unless_draining("session message outbox write")?;
        blocking("enqueue session message", move || {
            let _admission = admission;
            crate::database::enqueue_mailbox_event(&event_key, &target, &event_json, true, false)
        })
        .await
        .map(|_| ())
    }

    pub(super) async fn subagent_input_progress(&self, parent: &str) -> Result<InputProgress> {
        let snapshot = self
            .session_handle(parent.to_owned())
            .await?
            .and_then(|handle| handle.view().snapshot);
        // The snapshot that scheduled these tools includes preceding requests.
        // The actor publishes completion updates; forcing a sync here would
        // race the lease used to deliver those very completions.
        let mut progress = snapshot
            .as_ref()
            .map(InputProgress::from_snapshot)
            .unwrap_or_default();
        let parent = parent.to_owned();
        let messages = blocking("read sub-agent mailbox delivery status", move || {
            crate::database::subagent_mailbox_messages(&parent)
        })
        .await?;
        progress.add_mailbox_messages(messages);
        Ok(progress)
    }

    pub(super) async fn deliver_subagent_input(
        &self,
        parent: &str,
        child: &str,
        message: &str,
        request: &SubagentToolRequest,
    ) -> Result<SubagentInputDelivery> {
        ensure!(
            !self.exports.close_is_requested(child),
            "child session is closing; queued message was not delivered"
        );
        let record = self
            .exports
            .session_record(child)
            .context("child session no longer exists")?;
        if let Some(StartStatus::Failed { message }) =
            self.exports.startup_status(child.to_owned()).await?
        {
            bail!("child startup failed: {message}");
        }
        ensure_subagent_child_can_receive_work(&record, "message")?;

        let event_key = format!("subagent-message-{}", request.request_id);
        // Retries keep the first durable route even after config or worker changes.
        let mailbox_route_exists = {
            let event_key = event_key.clone();
            blocking("check existing parent-message route", move || {
                crate::database::mailbox_event_exists(&event_key)
            })
            .await?
        };
        if mailbox_route_exists {
            self.enqueue_parent_message(child, message, request, &event_key)
                .await?;
            return Ok(SubagentInputDelivery::Mailbox);
        }

        let command_id = format!("subagent-input-{}", request.request_id);
        // A prior accepted turn also fixes the route before checking current
        // mailbox settings or worker protocol.
        if let Some(ordinal) = prompt_acceptance(child, &command_id).await? {
            return Ok(SubagentInputDelivery::Turn { ordinal });
        }

        // This is the single routing decision for send_message. Reuse the
        // published protocol fact and compatibility check used by the outbox.
        let published_protocol = if self.exports.agent_mailboxes_enabled_for(child) {
            match self.sessions.session(child.to_owned()).await {
                Ok(handle) => {
                    crate::mailbox_outbox::published_worker_relay_protocol(&handle.view())
                }
                Err(error) => {
                    tracing::debug!(
                        child_session_id = child,
                        error = %format!("{error:#}"),
                        "child worker protocol is not published; queued parent message will use a turn"
                    );
                    None
                }
            }
        } else {
            None
        };
        let deliver_via_mailbox = published_protocol.is_some_and(|protocol| {
            crate::mailbox_outbox::trusted_parent_message_protocol_error(Some(protocol)).is_none()
        });
        if deliver_via_mailbox {
            self.enqueue_parent_message(child, message, request, &event_key)
                .await?;
            return Ok(SubagentInputDelivery::Mailbox);
        }

        let elapsed_ms = mj_core::clock::epoch_millis()
            .saturating_sub(request.created_at_ms)
            .max(0) as u64;
        let remaining = START_DEADLINE.saturating_sub(Duration::from_millis(elapsed_ms));
        let deadline = tokio::time::Instant::now() + remaining;
        loop {
            ensure!(
                !self.exports.close_is_requested(child),
                "child session is closing; queued message was not delivered"
            );
            let record = self
                .exports
                .session_record(child)
                .context("child session no longer exists")?;
            // A child whose startup failed is recorded as failed too; the
            // startup's own cause is the answer either way.
            let start = self.start_status(child.to_owned()).await?;
            if let Some(StartStatus::Failed { message }) = &start {
                bail!("child startup failed: {message}");
            }
            ensure_subagent_child_can_receive_work(&record, "message")?;
            ensure!(
                tokio::time::Instant::now() < deadline,
                "child was not ready for queued message within 30 minutes"
            );
            if matches!(start, Some(StartStatus::Pending)) {
                tokio::time::sleep(INPUT_POLL).await;
                continue;
            }
            if crate::upgrade::is_draining() {
                tokio::time::sleep(Duration::from_millis(250)).await;
                continue;
            }
            if record.state == SessionState::Parked || self.exports.subagent_park_running(child) {
                let parent_id = parent.to_owned();
                let child_id = child.to_owned();
                blocking("admit parked child restart", move || {
                    Controller::load()?.ensure_subagent_slot_available(&parent_id, Some(&child_id))
                })
                .await?;
                self.unpark_child(child).await?;
                continue;
            }
            let Some(mut handle) = self.session_handle(child.to_owned()).await? else {
                tokio::time::sleep(Duration::from_millis(250)).await;
                continue;
            };
            let view = handle.view();
            if let Some(ViewError::TargetMissing(detail)) = view.error {
                bail!("child lost its target: {detail}");
            }
            if !view.connected
                || !view
                    .snapshot
                    .as_ref()
                    .is_some_and(|s| s.operational.native_session_is_ready())
            {
                if let Ok(Err(error)) = tokio::time::timeout(START_POLL, handle.changed()).await {
                    tracing::debug!(child, %error, "reacquiring child actor while input is queued");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
                continue;
            }
            let Ok(_submission) =
                crate::upgrade::activity_unless_draining("subagent input delivery")
            else {
                continue;
            };
            // Synchronization materializes every accepted prompt before we
            // decide whether this request still needs a relay submission.
            if let Err(error) = handle.sync_now().await {
                if matches!(
                    handle.view().error,
                    Some(ViewError::ProjectionIntegrity(_) | ViewError::TargetMissing(_))
                ) {
                    return Err(error);
                }
                // Sync has no delivery side effect. A park may reserve the
                // actor after our view was read; wait for its new owner.
                tracing::debug!(child, %error, "child not available for queued input yet");
                drop(_submission);
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }
            if let Some(ordinal) = prompt_acceptance(child, &command_id).await? {
                return Ok(SubagentInputDelivery::Turn { ordinal });
            }
            ensure!(
                !self.exports.close_is_requested(child),
                "child session is closing; queued message was not delivered"
            );
            let result = handle
                .submit(
                    command_id.clone(),
                    RelayCommand::Prompt {
                        prompt: vec![ContentBlock::Text(TextContent::new(message))],
                    },
                )
                .await;
            match result {
                Ok(ordinal) => return Ok(SubagentInputDelivery::Turn { ordinal }),
                Err(error)
                    if error
                        .downcast_ref::<mj_client::session::DeliveryUnconfirmed>()
                        .is_some() =>
                {
                    // Never treat a missing acknowledgement as permission to
                    // make a new prompt. Expose the uncertainty to the parent.
                    return Err(error.context(
                        "input delivery is unconfirmed; do not resend without checking the child",
                    ));
                }
                Err(error) => {
                    if self
                        .exports
                        .session_record(child)
                        .is_some_and(|r| r.state == SessionState::Parked)
                        || self.exports.subagent_park_running(child)
                    {
                        // The existing park admission explicitly refused the
                        // command. Re-enter readiness with the same identity.
                        drop(_submission);
                        tokio::time::sleep(Duration::from_millis(100)).await;
                        continue;
                    }
                    return Err(error);
                }
            }
        }
    }

    async fn enqueue_parent_message(
        &self,
        child: &str,
        message: &str,
        request: &SubagentToolRequest,
        event_key: &str,
    ) -> Result<()> {
        let event = mj_core::mailbox::MailboxEvent {
            key: event_key.to_owned(),
            source: "parent".into(),
            wake: true,
            created_at_ms: request.created_at_ms.max(0) as u64,
            body: mj_core::mailbox::MailboxEventBody::ParentMessage {
                text: message.to_owned(),
            },
        };
        let event_json = serde_json::to_string(&event).map_err(|error| InputDeliveryFailure {
            via: "mailbox",
            source: error.into(),
        })?;
        let event_key = event_key.to_owned();
        let target = child.to_owned();
        let admission =
            crate::upgrade::activity_unless_draining("subagent parent-message outbox write")
                .map_err(|source| InputDeliveryFailure {
                    via: "mailbox",
                    source,
                })?;
        blocking("enqueue parent message for child", move || {
            let _admission = admission;
            crate::database::enqueue_mailbox_event(&event_key, &target, &event_json, true, true)
        })
        .await
        .map_err(|source| InputDeliveryFailure {
            via: "mailbox",
            source,
        })?;
        Ok(())
    }
}

/// Resolve a message recipient without widening the sender's existing target
/// scope. Exact IDs are passed through so the authorization checks below keep
/// returning their established refusals for self and foreign-child targets.
pub(super) fn resolve_session_message_target(
    state: &mj_core::state::State,
    sender_session_id: Option<&str>,
    requested_id: &str,
) -> Result<String> {
    if state.sessions.contains_key(requested_id) {
        return Ok(requested_id.to_owned());
    }

    if requested_id.len() < MIN_SESSION_ID_PREFIX_LENGTH {
        return Err(mj_core::refusal::Refusal::unusable(format!(
            "session id must be a full id or a prefix of at least {MIN_SESSION_ID_PREFIX_LENGTH} hexadecimal characters"
        ))
        .into());
    }

    let is_hex_prefix = requested_id.bytes().all(|byte| byte.is_ascii_hexdigit());
    let mut permitted_matches = Vec::new();
    let mut refused_matches = Vec::new();
    if is_hex_prefix {
        for (id, record) in &state.sessions {
            if !id.bytes().all(|byte| byte.is_ascii_hexdigit())
                || !id
                    .get(..requested_id.len())
                    .is_some_and(|candidate| candidate.eq_ignore_ascii_case(requested_id))
            {
                continue;
            }

            let is_subagent = state.is_subagent_session(id);
            let is_own_child = state.subagents.get(id).is_some_and(|relation| {
                Some(relation.parent_session_id.as_str()) == sender_session_id
            });
            if is_subagent && !is_own_child {
                refused_matches.push((id.as_str(), record, "foreign_subagent"));
            } else if !is_subagent && Some(id.as_str()) == sender_session_id {
                refused_matches.push((id.as_str(), record, "self"));
            } else {
                permitted_matches.push((id.as_str(), record));
            }
        }
    }

    match permitted_matches.as_slice() {
        [(id, _)] => Ok((*id).to_owned()),
        [] if refused_matches.len() == 1 => match refused_matches[0].2 {
            "self" => Err(mj_core::refusal::Refusal::unusable(
                "a session cannot send a message to itself",
            )
            .into()),
            "foreign_subagent" => Err(mj_core::refusal::Refusal::unusable(
                "a session cannot message another session's sub-agent",
            )
            .into()),
            _ => unreachable!("all refused prefix matches have a refusal kind"),
        },
        [] => Err(mj_core::refusal::Refusal::unusable(format!(
            "target session {requested_id} does not exist or was destroyed"
        ))
        .into()),
        _ => {
            let candidates = permitted_matches
                .iter()
                .map(|(id, record)| format!("{id} ({:?})", record.title))
                .collect::<Vec<_>>()
                .join(", ");
            Err(mj_core::refusal::Refusal::unusable(format!(
                "session id prefix {requested_id:?} is ambiguous; matching sessions: {candidates}"
            ))
            .into())
        }
    }
}

async fn prompt_acceptance(child: &str, command: &str) -> Result<Option<u64>> {
    let child = child.to_owned();
    let command = command.to_owned();
    blocking("reconcile sub-agent input", move || {
        crate::database::load_prompt_acceptance(&child, &command)
    })
    .await
}

#[derive(Default)]
pub(super) struct InputProgress {
    pending: BTreeMap<String, Vec<String>>,
    deliveries: BTreeMap<String, Vec<serde_json::Value>>,
}

impl InputProgress {
    pub(super) fn from_snapshot(snapshot: &mj_core::state::ManagedSessionSnapshot) -> Self {
        let mut progress = Self::default();
        let mut requests = snapshot.subagent_requests.iter().collect::<Vec<_>>();
        requests.sort_by_key(|r| (r.created_at_ms, &r.request_id));
        for request in requests {
            match &request.action {
                SubagentToolAction::SendInput {
                    child_session_id, ..
                }
                | SubagentToolAction::SendMessage {
                    child_session_id, ..
                } => progress
                    .pending
                    .entry(child_session_id.clone())
                    .or_default()
                    .push(request.request_id.clone()),
                _ => {}
            }
        }
        for result in &snapshot.subagent_results {
            let Ok(mut value) = serde_json::from_str::<serde_json::Value>(&result.message) else {
                continue;
            };
            let Some(child) = value
                .get("child_session_id")
                .and_then(|v| v.as_str())
                .map(str::to_owned)
            else {
                continue;
            };
            // These fields distinguish message results from spawn/close/wait.
            if value.get("created_at_ms").is_none() || value.get("status").is_none() {
                continue;
            }
            value["request_id"] = result.request_id.clone().into();
            let via = value
                .get("via")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
                .unwrap_or_else(|| {
                    if value["kind"] == "message" {
                        "mailbox".to_owned()
                    } else {
                        "turn".to_owned()
                    }
                });
            value["via"] = via.clone().into();
            // The outbox owns mailbox acceptance and failure. Its row
            // supplies the pending or final delivery state.
            if via == "mailbox" && value["status"] == "queued" {
                continue;
            }
            progress.deliveries.entry(child).or_default().push(value);
        }
        progress.sort_deliveries();
        progress
    }

    fn add_mailbox_messages(&mut self, messages: Vec<crate::database::SubagentMailboxMessage>) {
        let mut accepted = BTreeMap::<String, std::collections::BTreeSet<String>>::new();
        for row in messages {
            let Some(request_id) = row.event_key.strip_prefix("subagent-message-") else {
                continue;
            };
            let Ok(event) = serde_json::from_str::<mj_core::mailbox::MailboxEvent>(&row.event_json)
            else {
                continue;
            };
            if event.key != row.event_key || event.source != "parent" {
                continue;
            }
            if let Some(error) = row.failure {
                if let Some(pending) = self.pending.get_mut(&row.target_session_id) {
                    pending.retain(|pending_id| pending_id != request_id);
                }
                self.remove_delivery(&row.target_session_id, request_id);
                self.deliveries
                    .entry(row.target_session_id)
                    .or_default()
                    .push(serde_json::json!({
                        "request_id":request_id,
                        "created_at_ms":event.created_at_ms,
                        "status":"failed",
                        "via":"mailbox",
                        "error":error
                    }));
                continue;
            }
            if row.accepted {
                accepted
                    .entry(row.target_session_id.clone())
                    .or_default()
                    .insert(request_id.to_owned());
                self.remove_delivery(&row.target_session_id, request_id);
            } else {
                self.pending
                    .entry(row.target_session_id.clone())
                    .or_default()
                    .push(request_id.to_owned());
                continue;
            }
            let mut delivery = serde_json::json!({
                "request_id":request_id,
                "created_at_ms":event.created_at_ms,
                "status":"delivered",
                "via":"mailbox"
            });
            if let Some(command_id) = row.accepted_command_id {
                delivery["command_id"] = command_id.into();
            }
            if let Some(ordinal) = row.accepted_ordinal {
                delivery["accepted_ordinal"] = ordinal.into();
            }
            self.deliveries
                .entry(row.target_session_id)
                .or_default()
                .push(delivery);
        }
        for pending in self.pending.values_mut() {
            pending.sort();
            pending.dedup();
        }
        self.pending.retain(|child, pending| {
            if let Some(delivered) = accepted.get(child) {
                pending.retain(|request_id| !delivered.contains(request_id));
            }
            !pending.is_empty()
        });
        self.sort_deliveries();
    }

    fn remove_delivery(&mut self, child: &str, request_id: &str) {
        if let Some(deliveries) = self.deliveries.get_mut(child) {
            deliveries.retain(|delivery| delivery["request_id"] != request_id);
        }
    }

    fn sort_deliveries(&mut self) {
        for deliveries in self.deliveries.values_mut() {
            deliveries.sort_by(|a, b| {
                (a["created_at_ms"].as_i64(), a["request_id"].as_str())
                    .cmp(&(b["created_at_ms"].as_i64(), b["request_id"].as_str()))
            });
        }
    }

    pub fn status(
        &self,
        child: &str,
        current: (String, Option<String>, bool),
    ) -> (String, Option<String>, bool) {
        if matches!(current.0.as_str(), "stopping" | "stopped") {
            return current;
        }
        if self
            .pending
            .get(child)
            .is_some_and(|requests| !requests.is_empty())
        {
            return ("running".into(), None, false);
        }
        if let Some(last) = self.deliveries.get(child).and_then(|items| items.last())
            && last["status"] == "failed"
        {
            return (
                "failed".into(),
                Some(format!(
                    "Message {}: {}",
                    last["request_id"].as_str().unwrap_or_default(),
                    last["error"].as_str().unwrap_or("delivery failed")
                )),
                true,
            );
        }
        current
    }

    pub fn annotate(&self, child: &str, entry: &mut serde_json::Value) {
        if let Some(pending) = self.pending.get(child) {
            let pending = serde_json::json!(pending);
            entry["pending_messages"] = pending.clone();
            // Earlier parents already consume these field names.
            entry["pending_inputs"] = pending;
        }
        if let Some(deliveries) = self.deliveries.get(child) {
            let deliveries = serde_json::json!(deliveries);
            entry["message_deliveries"] = deliveries.clone();
            // Earlier parents already consume these field names.
            entry["input_deliveries"] = deliveries;
        }
    }
}
