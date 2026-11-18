//! Deferred child input uses the parent's existing durable request queue.

use super::*;
use mj_client::session::ViewError;

const START_DEADLINE: Duration = Duration::from_secs(30 * 60);
const INPUT_POLL: Duration = Duration::from_millis(250);
const MIN_SESSION_ID_PREFIX_LENGTH: usize = mj_core::state::SHORT_ID_LEN;
pub(crate) const QUEUED_UNTIL_STARTUP_NOTE: &str = "Queued until startup ends.";
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
        record.state.can_receive_child_work(),
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
    /// authenticated HTTP/CLI surface for a message one session sends.
    /// Sender identity is supplied by the caller context, never by MCP tool
    /// arguments. A message from the user has no sender and is the user's
    /// prompt, which the HTTP route submits as one.
    pub(super) async fn deliver_session_message(
        &self,
        sender_session_id: String,
        requested_target_id: String,
        text: String,
        request_id: String,
        created_at_ms: i64,
    ) -> Result<crate::server::api::SessionMessageResponse> {
        let requested_target_for_auth = requested_target_id.clone();
        let sender_for_auth = sender_session_id.clone();
        let (target_session_id, own_child, managed_child, sender_title, target_state) =
            blocking("authorize session message", move || {
            let state = Controller::load()?.state;
            let sender = state.sessions.get(&sender_for_auth).ok_or_else(|| {
                anyhow::Error::new(mj_core::refusal::Refusal::unusable(format!(
                    "sender session {sender_for_auth} does not exist"
                )))
            })?;
            let target_id = resolve_session_target(
                &state,
                None,
                &requested_target_for_auth,
            )?;
            // The oracle is asked with ask_oracle, never addressed directly.
            if mj_controller::database::load_subagent(&target_id)?.is_some_and(|relation| {
                relation.kind == mj_core::subagent::SubagentKind::Oracle
                    && relation.parent_session_id == sender_for_auth
            }) {
                anyhow::bail!(mj_core::refusal::Refusal::unusable(format!(
                    "target session {} does not exist or was destroyed",
                    requested_target_for_auth
                )));
            }
            let target = state.sessions.get(&target_id).cloned().ok_or_else(|| {
                anyhow::Error::new(mj_core::refusal::Refusal::unusable(format!(
                    "target session {} does not exist or was destroyed",
                    requested_target_for_auth
                )))
            })?;
            if sender_for_auth == target_id {
                anyhow::bail!(mj_core::refusal::Refusal::unusable(
                    "a session cannot send a message to itself"
                ));
            }
            let own_child = state.is_subagent_session(&target_id)
                && state
                    .subagents
                    .get(&target_id)
                    .is_some_and(|relation| relation.parent_session_id == sender_for_auth);
            let managed_child = state.subagents.contains_key(&target_id);
            if target.state == SessionState::DestroyedWithDataLoss {
                anyhow::bail!(mj_core::refusal::Refusal::precondition(
                    "target session was destroyed and cannot receive messages"
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
                ) || (target.state == SessionState::Parked && !managed_child) {
                    anyhow::bail!(mj_core::refusal::Refusal::precondition(format!(
                        "target session is {:?} and cannot receive messages",
                        target.state
                    )));
                }
            }
            Ok((
                target_id,
                own_child,
                managed_child,
                sender.message_name(),
                target.state,
            ))
        })
        .await?;

        if own_child {
            return self
                .deliver_to_own_child(target_session_id, text, request_id, created_at_ms)
                .await;
        }

        if managed_child {
            ensure!(
                !self.exports.close_is_requested(&target_session_id),
                "child session is closing; queued message was not delivered"
            );
            self.unpark_child(&target_session_id).await?;
        }

        let sender = mj_core::mailbox::Sender::Session {
            id: sender_session_id,
            title: sender_title,
        };
        // The request id is the durable mailbox idempotency key; the row's
        // target is the resolved full session ID.
        let event_key = format!("session-message-{request_id}");
        let mailbox_route_exists = {
            let event_key = event_key.clone();
            blocking("check existing session-message route", move || {
                mj_controller::database::mailbox_event_exists(&event_key)
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
            self.enqueue_session_message(&target_session_id, &event, &event_key, managed_child)
                .await?;
            let mailbox_delivery = self.published_mailbox_delivery(&target_session_id).await;
            return Ok(crate::server::api::SessionMessageResponse {
                session_id: target_session_id,
                via: "mailbox".into(),
                note: mailbox_delivery_note(mailbox_delivery.as_ref()),
                mailbox_delivery,
                turn_id: None,
                managed_child: false,
            });
        }

        let command_id =
            mj_core::continuation::GeneratedPrompt::SessionMessageTurn.command_id(&request_id);
        if let Some(ordinal) = prompt_acceptance(&target_session_id, &command_id).await? {
            return Ok(crate::server::api::SessionMessageResponse {
                session_id: target_session_id,
                via: "turn".into(),
                mailbox_delivery: None,
                note: None,
                turn_id: Some(ordinal),
                managed_child: false,
            });
        }

        // A peer message sent while startup is still running joins that queue.
        // Mailing it immediately would land before the first prompt, and
        // refusing it makes the sender retry until the message is lost (#1553).
        if let Some(queued) = self
            .queue_session_message_until_startup(
                &target_session_id,
                target_state,
                &request_id,
                &command_id,
                &event,
            )
            .await?
        {
            return Ok(queued);
        }

        let protocol = {
            match self.sessions.session(target_session_id.clone()).await {
                Ok(handle) => {
                    mj_controller::mailbox_outbox::published_worker_relay_protocol(&handle.view())
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
        };
        let mailbox_supported = protocol
            .is_some_and(|protocol| protocol >= mj_core::relay::RELAY_SESSION_MESSAGE_PROTOCOL);
        if mailbox_supported {
            self.enqueue_session_message(&target_session_id, &event, &event_key, managed_child)
                .await?;
            let mailbox_delivery = self.published_mailbox_delivery(&target_session_id).await;
            return Ok(crate::server::api::SessionMessageResponse {
                session_id: target_session_id,
                via: "mailbox".into(),
                note: mailbox_delivery_note(mailbox_delivery.as_ref()),
                mailbox_delivery,
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
            mailbox_delivery: None,
            note: None,
            turn_id: Some(turn_id),
            managed_child: false,
        })
    }

    /// A parent's message to one of its children: through the child's mailbox,
    /// waking it when parked, or as a turn, keyed by the request for replay.
    pub(super) async fn deliver_to_own_child(
        &self,
        child_session_id: String,
        text: String,
        request_id: String,
        created_at_ms: i64,
    ) -> Result<crate::server::api::SessionMessageResponse> {
        let request = SubagentToolRequest {
            originating_command_id: None,
            request_id,
            created_at_ms,
            action: SubagentToolAction::SendMessage {
                child_session_id: child_session_id.clone(),
                message: text.clone(),
            },
        };
        let delivery = self
            .deliver_subagent_input(&child_session_id, &text, &request)
            .await?;
        let (via, turn_id) = match delivery {
            SubagentInputDelivery::Mailbox => ("mailbox", None),
            SubagentInputDelivery::Turn { ordinal } => ("turn", Some(ordinal)),
        };
        let mailbox_delivery = if via == "mailbox" {
            self.published_mailbox_delivery(&child_session_id).await
        } else {
            None
        };
        Ok(crate::server::api::SessionMessageResponse {
            session_id: child_session_id,
            via: via.into(),
            note: mailbox_delivery_note(mailbox_delivery.as_ref()),
            mailbox_delivery,
            turn_id,
            managed_child: true,
        })
    }

    /// Read the capability from the worker state already published by its
    /// session actor. Older or disconnected workers leave it unknown.
    pub(super) async fn published_mailbox_delivery(
        &self,
        session_id: &str,
    ) -> Option<mj_core::mailbox::MailboxDeliveryMode> {
        self.sessions
            .session(session_id.to_owned())
            .await
            .ok()
            .and_then(|handle| handle.view().snapshot)
            .and_then(|snapshot| snapshot.operational.mailbox_delivery)
    }

    async fn enqueue_session_message(
        &self,
        target: &str,
        event: &mj_core::mailbox::MailboxEvent,
        event_key: &str,
        unpark: bool,
    ) -> Result<()> {
        let event_json = serde_json::to_string(event)?;
        let event_key = event_key.to_owned();
        let target = target.to_owned();
        let admission =
            mj_controller::upgrade::activity_unless_draining("session message outbox write")?;
        let recipient = target.clone();
        let enqueued = blocking("enqueue session message", move || {
            let _admission = admission;
            mj_controller::database::enqueue_mailbox_event(
                &event_key,
                &target,
                &event_json,
                true,
                unpark,
            )
        })
        .await?;
        anyhow::ensure!(
            enqueued != mj_controller::database::Enqueued::TargetGone,
            "session {recipient} no longer exists, so the message was not delivered"
        );
        Ok(())
    }

    /// Queue `event` when `target` cannot take it yet. `None` means deliver now.
    /// An existing step for `command_id` is the retry: the first sender, text,
    /// and timestamp win, and a failed step stays failed.
    async fn queue_session_message_until_startup(
        &self,
        target: &str,
        target_state: SessionState,
        request_id: &str,
        command_id: &str,
        event: &mj_core::mailbox::MailboxEvent,
    ) -> Result<Option<crate::server::api::SessionMessageResponse>> {
        let (sender_id, message_text) = session_message_identity(event)?;
        if let Some(response) = self
            .session_message_startup_receipt(target, command_id, sender_id, message_text)
            .await?
        {
            return Ok(Some(response));
        }
        let eligible = matches!(
            target_state,
            SessionState::Provisioning
                | SessionState::Running
                | SessionState::Disconnected
                | SessionState::Checkpointing
        );
        if !eligible {
            return Ok(None);
        }
        let (group_id, status) = self.exports.startup_context(target.to_owned()).await?;
        let target_id = target.to_owned();
        let in_progress = blocking("read startup queue", move || {
            Ok(
                mj_controller::database::next_startup_delivery(&target_id)?.is_some_and(|step| {
                    matches!(step.phase.as_str(), "pending" | "delivering" | "accepted")
                }),
            )
        })
        .await?;
        let pending = matches!(status, Some(crate::server::api::StartStatus::Pending));
        let provisioning = target_state == SessionState::Provisioning;
        let actor_missing = !(provisioning || pending || in_progress)
            && self.sessions.session(target.to_owned()).await.is_err();
        if !(provisioning || pending || in_progress || actor_missing) {
            return Ok(None);
        }
        // A sticky failed startup must not block a later message once the
        // session can take work. Only a message queued behind a still-pending
        // group remembers that group, and only that group can retire it.
        let waited_on_group = pending.then_some(group_id).flatten();
        let message = crate::daemon::startup_followup::QueuedSessionMessage {
            request_id: request_id.to_owned(),
            sender_id: sender_id.to_owned(),
            sender_title: session_message_title(event)?,
            text: message_text.to_owned(),
            created_at_ms: event.created_at_ms,
            waited_on_group,
        };
        let queued = queued_session_message_response(target.to_owned());
        match Arc::clone(&self.exports)
            .queue_startup_session_message(target.to_owned(), message)
            .await
        {
            Ok(()) => Ok(Some(queued)),
            Err(error) if error.is::<mj_client::daemon::StartupPromptIdentityConflict>() => self
                .session_message_startup_receipt(target, command_id, sender_id, message_text)
                .await?
                .context("startup command ID was reused with a different payload")
                .map(Some),
            Err(error) => Err(error),
        }
    }

    /// What a retry of `command_id` should answer. `None` when no step exists.
    async fn session_message_startup_receipt(
        &self,
        target: &str,
        command_id: &str,
        sender_id: &str,
        message_text: &str,
    ) -> Result<Option<crate::server::api::SessionMessageResponse>> {
        let command_id = command_id.to_owned();
        let existing = blocking("read session-message startup step", move || {
            mj_controller::database::load_startup_delivery(&command_id)
        })
        .await?;
        let Some(existing) = existing else {
            return Ok(None);
        };
        if existing.session_id != target {
            return Err(session_message_identity_conflict());
        }
        let stored: crate::daemon::StartupStep = serde_json::from_str(&existing.step_json)
            .context("stored session message could not be read")?;
        let crate::daemon::StartupStep::SessionMessage {
            sender_id: stored_sender,
            text,
            ..
        } = &stored
        else {
            return Err(session_message_identity_conflict());
        };
        if stored_sender != sender_id || text != message_text {
            return Err(session_message_identity_conflict());
        }
        match existing.phase.as_str() {
            "failed" | "rejecting" => bail!(
                "{}",
                existing
                    .error
                    .unwrap_or_else(|| "the message was not delivered".to_owned())
            ),
            "pending" | "delivering" | "accepted" | "cancelling" => {
                Ok(Some(queued_session_message_response(target.to_owned())))
            }
            "done" | "dismissed" => {
                bail!("the message was already settled but its delivery receipt is missing")
            }
            other => bail!("session message is in an unknown startup phase {other}"),
        }
    }

    /// The parent's messages to its children and what became of them. The
    /// mailbox's part comes from the store. The part kept in the parent's
    /// worker queue is unknown while the parent has no session actor or its
    /// actor has no snapshot yet; see [`InputProgress::queue_known`].
    pub(super) async fn subagent_input_progress(&self, parent: &str) -> Result<InputProgress> {
        let snapshot = self
            .session_handle(parent.to_owned())
            .await?
            .and_then(|handle| handle.view().snapshot);
        if let Some(error) = snapshot
            .as_ref()
            .and_then(|snapshot| snapshot.subagent_queue_error.as_ref())
        {
            anyhow::bail!("{error}");
        }
        // The snapshot that scheduled these tools includes preceding requests.
        // The actor publishes completion updates; forcing a sync here would
        // race the lease used to deliver those very completions.
        let mut progress = snapshot
            .as_ref()
            .map(InputProgress::from_snapshot)
            .unwrap_or_default();
        let parent = parent.to_owned();
        let messages = blocking("read sub-agent mailbox delivery status", move || {
            mj_controller::database::subagent_mailbox_messages(&parent)
        })
        .await?;
        progress.add_mailbox_messages(messages);
        Ok(progress)
    }

    pub(super) async fn deliver_subagent_input(
        &self,
        child: &str,
        message: &str,
        request: &SubagentToolRequest,
    ) -> Result<SubagentInputDelivery> {
        let picked_up = tokio::time::Instant::now();
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
                mj_controller::database::mailbox_event_exists(&event_key)
            })
            .await?
        };
        if mailbox_route_exists {
            self.enqueue_parent_message(child, message, request, &event_key)
                .await?;
            return Ok(SubagentInputDelivery::Mailbox);
        }

        let command_id =
            mj_core::continuation::GeneratedPrompt::subagent_input_id(&request.request_id);
        // A prior accepted turn fixes the route before checking the current
        // worker protocol.
        if let Some(ordinal) = prompt_acceptance(child, &command_id).await? {
            return Ok(SubagentInputDelivery::Turn { ordinal });
        }

        // This is the single routing decision for send_message. Reuse the
        // published protocol fact and compatibility check used by the outbox.
        let deliver_via_mailbox =
            match self.sessions.parent_message_route(child.to_owned()).await? {
                mj_client::session::ParentMessageRoute::Starting => true,
                mj_client::session::ParentMessageRoute::Published(protocol) => protocol
                    .is_some_and(|protocol| {
                        mj_controller::mailbox_outbox::trusted_parent_message_protocol_error(Some(
                            protocol,
                        ))
                        .is_none()
                    }),
                mj_client::session::ParentMessageRoute::Unmanaged => false,
            };
        if deliver_via_mailbox {
            self.enqueue_parent_message(child, message, request, &event_key)
                .await?;
            return Ok(SubagentInputDelivery::Mailbox);
        }

        // The window runs from this daemon's first look at the request. The
        // request's own creation stamp is not the start: it comes from the
        // parent's clock, and a request that waited in the durable queue while
        // the daemon was away (#1445) has spent no time with a child yet.
        let deadline = picked_up + START_DEADLINE;
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
            if mj_controller::upgrade::is_draining() {
                tokio::time::sleep(Duration::from_millis(250)).await;
                continue;
            }
            if record.state == SessionState::Parked || self.exports.subagent_park_running(child) {
                match self.unpark_child(child).await {
                    Err(error)
                        if mj_core::refusal::Refusal::of(&error).is_some_and(|refusal| {
                            refusal.code() == Some(mj_controller::upgrade::HANDOFF_REFUSAL_CODE)
                        }) =>
                    {
                        tracing::debug!(child, %error, "child wake admission deferred; retaining queued input");
                        tokio::time::sleep(Duration::from_millis(250)).await;
                    }
                    // The wake was accepted and the child is starting under
                    // its durable boot intent; slow readiness is not a failed
                    // delivery. Wait for it within the delivery deadline. A
                    // reattach can fail at once, so pause before asking again
                    // rather than restarting the worker in a tight loop.
                    Err(error) if error.downcast_ref::<super::WakeNotReady>().is_some() => {
                        tracing::debug!(child, %error, "accepted child wake is not ready yet; retaining queued input");
                        tokio::time::sleep(INPUT_POLL).await;
                    }
                    result => {
                        result?;
                    }
                }
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
                mj_controller::upgrade::activity_unless_draining("subagent input delivery")
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
        let admission = mj_controller::upgrade::activity_unless_draining(
            "subagent parent-message outbox write",
        )
        .map_err(|source| InputDeliveryFailure {
            via: "mailbox",
            source,
        })?;
        let recipient = target.clone();
        let enqueued = blocking("enqueue parent message for child", move || {
            let _admission = admission;
            mj_controller::database::enqueue_mailbox_event(
                &event_key,
                &target,
                &event_json,
                true,
                true,
            )
        })
        .await
        .map_err(|source| InputDeliveryFailure {
            via: "mailbox",
            source,
        })?;
        if enqueued == mj_controller::database::Enqueued::TargetGone {
            return Err(InputDeliveryFailure {
                via: "mailbox",
                source: anyhow::anyhow!(
                    "session {recipient} no longer exists, so the message was not delivered"
                ),
            }
            .into());
        }
        Ok(())
    }
}

pub(super) fn mailbox_delivery_note(
    delivery: Option<&mj_core::mailbox::MailboxDeliveryMode>,
) -> Option<String> {
    match delivery {
        Some(mj_core::mailbox::MailboxDeliveryMode::TurnBoundary) => Some(
            "If the recipient is working, delivery is deferred until its current turn ends.".into(),
        ),
        Some(mj_core::mailbox::MailboxDeliveryMode::ToolBoundary) | None => None,
    }
}

/// Resolve any message recipient, including managed children. Supplying an
/// owner retains the narrower child-management scope for lifecycle operations.
/// Exact IDs pass through for the operation's final authorization check.
pub(super) fn resolve_session_target(
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
            if is_subagent && !is_own_child && sender_session_id.is_some() {
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
                .map(|(id, record)| format!("{id} ({:?})", record.message_name()))
                .collect::<Vec<_>>()
                .join(", ");
            Err(mj_core::refusal::Refusal::unusable(format!(
                "session id prefix {requested_id:?} is ambiguous; matching sessions: {candidates}"
            ))
            .into())
        }
    }
}

fn queued_session_message_response(
    session_id: String,
) -> crate::server::api::SessionMessageResponse {
    crate::server::api::SessionMessageResponse {
        session_id,
        via: "queued".into(),
        note: Some(QUEUED_UNTIL_STARTUP_NOTE.into()),
        mailbox_delivery: None,
        turn_id: None,
        managed_child: false,
    }
}

fn session_message_identity_conflict() -> anyhow::Error {
    anyhow::Error::new(mj_client::daemon::StartupPromptIdentityConflict)
}

fn session_message_identity(event: &mj_core::mailbox::MailboxEvent) -> Result<(&str, &str)> {
    match &event.body {
        mj_core::mailbox::MailboxEventBody::SessionMessage {
            from: mj_core::mailbox::Sender::Session { id, .. },
            text,
        } => Ok((id.as_str(), text.as_str())),
        _ => bail!("session message is missing its sender"),
    }
}

fn session_message_title(event: &mj_core::mailbox::MailboxEvent) -> Result<String> {
    match &event.body {
        mj_core::mailbox::MailboxEventBody::SessionMessage {
            from: mj_core::mailbox::Sender::Session { title, .. },
            ..
        } => Ok(title.clone()),
        _ => bail!("session message is missing its sender"),
    }
}

async fn prompt_acceptance(child: &str, command: &str) -> Result<Option<u64>> {
    let child = child.to_owned();
    let command = command.to_owned();
    blocking("reconcile sub-agent input", move || {
        mj_controller::database::load_prompt_acceptance(&child, &command)
    })
    .await
}

#[derive(Default)]
pub(super) struct InputProgress {
    pending: BTreeMap<String, Vec<String>>,
    deliveries: BTreeMap<String, Vec<serde_json::Value>>,
    /// See [`Self::queue_known`]. The default is unknown.
    queue_known: bool,
}

impl InputProgress {
    pub(super) fn from_snapshot(snapshot: &mj_core::state::ManagedSessionSnapshot) -> Self {
        let mut progress = Self {
            queue_known: true,
            ..Self::default()
        };
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
        for result in snapshot.subagent_completions.iter() {
            let Some(mut value) = result.input_delivery.clone() else {
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

    fn add_mailbox_messages(
        &mut self,
        messages: Vec<mj_controller::database::SubagentMailboxMessage>,
    ) {
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

    /// The child's state with queued messages taken into account. A message
    /// that is queued for a child that can take it means the child is about
    /// to run, whatever its last turn said. Nothing else changes the state: a
    /// delivery that failed is a fact about the message (see
    /// [`Self::failed_deliveries`]), not about the child, whose record
    /// alone says whether it can be given work (#1381).
    pub fn status(
        &self,
        child: &str,
        record: Option<&mj_core::state::SessionRecord>,
        current: (String, Option<String>, bool),
    ) -> (String, Option<String>, bool) {
        // `stopping` is how `subagent_status` reports an accepted close while
        // the record is still `Running`; a queued message cannot run then.
        if matches!(current.0.as_str(), "stopping" | "stopped")
            || !record.is_some_and(|record| record.state.can_receive_child_work())
        {
            return current;
        }
        if self
            .pending
            .get(child)
            .is_some_and(|requests| !requests.is_empty())
        {
            return ("running".into(), None, false);
        }
        current
    }

    /// Whether the parent's worker queue was read: the messages still queued
    /// in it and the outcome of every delivery made through it. It is not
    /// read while the parent has no session actor, or its actor has no
    /// snapshot yet, and then this progress holds only the mailbox's part.
    /// Unknown is not empty: a failure in the unread queue is not absent.
    pub fn queue_known(&self) -> bool {
        self.queue_known
    }

    /// Every message delivery to the child that failed, oldest first. A
    /// delivery that succeeded later does not undo one that failed: the work
    /// in the failed message is still lost, so each one is news (#1459).
    pub fn failed_deliveries(&self, child: &str) -> impl Iterator<Item = FailedDelivery<'_>> {
        self.deliveries
            .get(child)
            .into_iter()
            .flatten()
            .filter(|delivery| delivery["status"] == "failed")
            .map(|delivery| FailedDelivery {
                request_id: delivery["request_id"].as_str().unwrap_or_default(),
                error: delivery["error"].as_str().unwrap_or("delivery failed"),
                delivery,
            })
    }

    pub fn annotate(&self, child: &str, entry: &mut serde_json::Value) {
        self.annotate_pending(child, entry);
        if let Some(deliveries) = self.deliveries.get(child) {
            let deliveries = serde_json::json!(deliveries);
            entry["message_deliveries"] = deliveries.clone();
            // Earlier parents already consume these field names.
            entry["input_deliveries"] = deliveries;
        }
    }

    pub fn annotate_pending(&self, child: &str, entry: &mut serde_json::Value) {
        if let Some(pending) = self.pending.get(child) {
            let pending = serde_json::json!(pending);
            entry["pending_messages"] = pending.clone();
            // Earlier parents already consume these field names.
            entry["pending_inputs"] = pending;
        }
    }
}

/// One failed message delivery, as `list_agents` shows it.
pub(super) struct FailedDelivery<'a> {
    pub request_id: &'a str,
    pub error: &'a str,
    pub delivery: &'a serde_json::Value,
}

#[cfg(test)]
mod tests {
    use super::*;

    // Hard-won: 60adf13fe: a queued message turned a child whose close was running back to "running".
    #[test]
    fn a_queued_message_does_not_hide_a_running_close() {
        let mut record = mj_controller::controller::test_support::checkpoint_test_session("child");
        record.state = SessionState::Running;
        let progress = InputProgress {
            pending: BTreeMap::from([("child".to_owned(), vec!["message-1".to_owned()])]),
            deliveries: BTreeMap::new(),
            queue_known: true,
        };
        assert_eq!(
            progress.status("child", Some(&record), ("stopping".into(), None, false)),
            ("stopping".into(), None, false)
        );
        assert_eq!(
            progress.status("child", Some(&record), ("completed".into(), None, true)),
            ("running".into(), None, false),
            "a child that can take the queued message is about to run"
        );
    }
}
