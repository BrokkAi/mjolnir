//! `mj acp`: an ACP agent whose turns run as Mjolnir sessions.
//!
//! A program that speaks the Agent Client Protocol normally starts a coding
//! harness as a child process and talks to it over standard input and output.
//! This module makes Mjolnir look like one of those harnesses: the consumer
//! starts `mj acp` instead, and every session it creates is a Mjolnir session,
//! which means it runs on a configured target, appears in `mj sessions`, is
//! indexed for search, and can be watched, steered, and resumed like any other.
//!
//! Everything here is a client of the documented HTTP API rather than of the
//! daemon's internals. That is deliberate for two reasons: the adapter cannot
//! depend on behavior no other consumer can, and it stays an honest test of the
//! contract every other consumer sees.
//!
//! Standard output carries the protocol and nothing else. Mjolnir's diagnostic
//! logging goes to a file, and its failures to standard error.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::schema::v1::{
    AgentCapabilities, CancelNotification, ContentBlock, ContentChunk, InitializeRequest,
    InitializeResponse, NewSessionRequest, NewSessionResponse, PromptRequest, PromptResponse,
    SessionId, SessionNotification, SessionUpdate, StopReason, TextContent,
};
use agent_client_protocol::{Agent, Stdio};
use anyhow::{Context, Result, bail};
use clap::Args;

use mj_controller::server::api::{StartSessionRequest, WaitOutcome, WaitRequest, WaitResponse};

use crate::api_client::ApiClient;

/// Where a consumer tells the adapter its sessions should run.
///
/// Every flag is optional, and an omitted one falls back to the pair that
/// `mj new` resolves the same way, so `mj acp` alone is a valid agent command.
#[derive(Debug, Clone, Default, Args)]
pub(crate) struct AcpArgs {
    /// Profile the session runs its harness from.
    #[arg(long)]
    pub(crate) profile: Option<String>,
    /// Target template the session is provisioned on.
    #[arg(long)]
    pub(crate) target: Option<String>,
    /// Existing bundle to run. Without one the consumer's working directory is
    /// the project, which is what a local target needs.
    #[arg(long)]
    pub(crate) bundle: Option<String>,
}

/// What the adapter needs beyond one request.
struct Adapter {
    args: AcpArgs,
    /// The workspace named with the global `--workspace`, resolved to its id
    /// when a session is created.
    workspace: Option<String>,
    /// The client every request goes through, when one was supplied. The
    /// real adapter connects to the daemon on demand instead, so that
    /// `initialize` works before the daemon has started.
    client: Option<Arc<ApiClient>>,
    /// The sessions this process created. A prompt may only name one of these,
    /// so an adapter instance cannot be used to drive unrelated sessions that
    /// happen to live in the same daemon.
    sessions: Mutex<HashSet<String>>,
    /// Sessions with a turn in flight, so closing the pipe can stop them.
    active: Mutex<HashSet<String>>,
    /// Sessions the consumer asked to cancel, kept until the turn that is
    /// answering them has read the request.
    cancelling: Mutex<HashSet<String>>,
}

/// Serve the Agent Client Protocol on standard input and output.
///
/// Returns when the consumer closes its side of the pipe or the connection
/// fails.
pub(crate) async fn serve(args: AcpArgs, workspace: Option<String>) -> Result<()> {
    serve_on(Arc::new(Adapter::new(args, workspace, None)), Stdio::new()).await
}

/// Serve the protocol over any transport.
///
/// A prompt runs in a task of its own rather than in its handler. The
/// connection handles one message at a time and waits for each handler to
/// return, so a handler that awaited the whole turn held back the consumer's
/// `session/cancel` until the turn had already ended, and kept the adapter
/// from noticing that the consumer had gone away.
async fn serve_on(
    adapter: Arc<Adapter>,
    transport: impl agent_client_protocol::ConnectTo<Agent> + 'static,
) -> Result<()> {
    let served = Agent
        .builder()
        .name("mjolnir")
        .on_receive_request(
            async |request: InitializeRequest, responder, _cx| {
                responder.respond(initialize(request))
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let adapter = Arc::clone(&adapter);
                async move |request: NewSessionRequest, responder, _cx| match adapter
                    .new_session(request)
                    .await
                {
                    Ok(session_id) => responder.respond(NewSessionResponse::new(session_id)),
                    Err(error) => responder.respond_with_internal_error(format!("{error:#}")),
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let adapter = Arc::clone(&adapter);
                async move |request: PromptRequest, responder, cx| {
                    let adapter = Arc::clone(&adapter);
                    let connection = cx.clone();
                    cx.spawn(async move {
                        let session_id = request.session_id.0.to_string();
                        let prompt = match prompt_text(&request.prompt) {
                            Ok(prompt) => prompt,
                            Err(error) => {
                                return responder.respond_with_internal_error(format!("{error:#}"));
                            }
                        };
                        let mut notify = |session_id: &str, message: &str| -> Result<()> {
                            connection
                                .send_notification(SessionNotification::new(
                                    SessionId::new(session_id),
                                    SessionUpdate::AgentMessageChunk(ContentChunk::new(
                                        ContentBlock::Text(TextContent::new(message)),
                                    )),
                                ))
                                .map_err(|error| anyhow::anyhow!("send a session update: {error}"))
                        };
                        match adapter.turn(&session_id, &prompt, &mut notify).await {
                            Ok(stop_reason) => responder.respond(PromptResponse::new(stop_reason)),
                            Err(error) => {
                                responder.respond_with_internal_error(format!("{error:#}"))
                            }
                        }
                    })
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_notification(
            {
                let adapter = Arc::clone(&adapter);
                async move |notification: CancelNotification, _cx| {
                    adapter.cancel(&notification.session_id.0).await;
                    Ok(())
                }
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .connect_to(transport)
        .await;
    // The consumer is gone, so nothing it started can be watched or steered any
    // more: stop the turns it can no longer see. The sessions themselves stay,
    // because they are durable and a person may still want to resume one.
    adapter.stop_active_turns().await;
    served.context("serving the Agent Client Protocol on standard input and output")
}

/// Answer `initialize`.
///
/// This adapter speaks ACP v1 and claims no optional capability. The workspace,
/// the shell, and the files belong to the session's own worker on its target,
/// so there is nothing for the consumer to provide and nothing to advertise:
/// a capability promised here would invite a consumer to hand over work this
/// process has no business accepting.
fn initialize(_request: InitializeRequest) -> InitializeResponse {
    InitializeResponse::new(ProtocolVersion::V1).agent_capabilities(AgentCapabilities::new())
}

impl Adapter {
    fn new(args: AcpArgs, workspace: Option<String>, client: Option<Arc<ApiClient>>) -> Self {
        Self {
            args,
            workspace,
            client,
            sessions: Mutex::new(HashSet::new()),
            active: Mutex::new(HashSet::new()),
            cancelling: Mutex::new(HashSet::new()),
        }
    }

    async fn client(&self) -> Result<Arc<ApiClient>> {
        match &self.client {
            Some(client) => Ok(Arc::clone(client)),
            None => Ok(Arc::new(
                ApiClient::connect()
                    .await
                    .context("connect to the Mjolnir daemon")?,
            )),
        }
    }

    /// Create the Mjolnir session that one ACP session will run in.
    ///
    /// The ACP session id is the Mjolnir session id. Keeping them the same
    /// means a consumer's logs, `mj sessions`, and the daemon's own records all
    /// name one thing, which is the difference between a debuggable integration
    /// and a search for a mapping table.
    async fn new_session(&self, request: NewSessionRequest) -> Result<SessionId> {
        let client = self.client().await?;
        // The same rule `mj new` follows: a named workspace is looked up, and
        // without one the daemon picks, which it can only do when there is
        // exactly one.
        let workspace_id = match self.workspace.as_deref() {
            Some(name) => Some(resolve_workspace(&client, name).await?),
            None => None,
        };
        let start = start_request(&self.args, workspace_id, &request.cwd);
        let started = client.start(&start).await.context("create the session")?;
        self.sessions
            .lock()
            .expect("adapter session set")
            .insert(started.session_id.clone());
        Ok(SessionId::new(started.session_id))
    }

    fn owns(&self, session_id: &str) -> bool {
        self.sessions
            .lock()
            .expect("adapter session set")
            .contains(session_id)
    }

    fn is_cancelling(&self, session_id: &str) -> bool {
        self.cancelling
            .lock()
            .expect("adapter cancel set")
            .contains(session_id)
    }

    /// Run one turn in a session this adapter created.
    async fn turn(
        &self,
        session_id: &str,
        prompt: &str,
        notify: &mut impl FnMut(&str, &str) -> Result<()>,
    ) -> Result<StopReason> {
        if !self.owns(session_id) {
            bail!("this adapter did not create session {session_id}");
        }
        let client = self.client().await?;
        self.active
            .lock()
            .expect("adapter active set")
            .insert(session_id.to_owned());
        let result = run_turn(&client, session_id, prompt, notify, || {
            self.is_cancelling(session_id)
        })
        .await;
        self.active
            .lock()
            .expect("adapter active set")
            .remove(session_id);
        // A cancellation is the consumer's decision, and the specification
        // requires `Cancelled` even when the work underneath fails while the
        // cancellation is being applied. The failure is still worth recording.
        if self
            .cancelling
            .lock()
            .expect("adapter cancel set")
            .remove(session_id)
        {
            if let Err(error) = &result {
                tracing::debug!(%error, %session_id, "a cancelled turn also failed");
            }
            return Ok(StopReason::Cancelled);
        }
        result
    }

    /// Remember that the consumer asked to cancel, then ask the daemon to stop.
    async fn cancel(&self, session_id: &str) {
        // Marked first: a daemon this process cannot reach must not turn a
        // cancel into an end of turn, because the consumer asked for this.
        self.mark_cancelled(session_id);
        match self.client().await {
            Ok(client) => interrupt(&client, session_id).await,
            Err(error) => {
                tracing::warn!(%error, %session_id, "could not reach the daemon to cancel a turn");
            }
        }
    }

    /// Stop the turns the consumer can no longer see.
    async fn stop_active_turns(&self) {
        let active: Vec<String> = self
            .active
            .lock()
            .expect("adapter active set")
            .iter()
            .cloned()
            .collect();
        if active.is_empty() {
            return;
        }
        match self.client().await {
            Ok(client) => {
                for session_id in active {
                    interrupt(&client, &session_id).await;
                }
            }
            Err(error) => {
                tracing::warn!(%error, "could not reach the daemon to stop active turns");
            }
        }
    }

    fn mark_cancelled(&self, session_id: &str) {
        self.cancelling
            .lock()
            .expect("adapter cancel set")
            .insert(session_id.to_owned());
    }
}

/// The id of the workspace a name selects, read the way `mj new --workspace`
/// reads it: a name is unique regardless of case.
async fn resolve_workspace(client: &ApiClient, name: &str) -> Result<String> {
    let wanted = name.trim().to_lowercase();
    client
        .workspaces()
        .await
        .context("list the workspaces")?
        .workspaces
        .into_iter()
        .find(|workspace| workspace.name.to_lowercase() == wanted)
        .map(|workspace| workspace.id)
        .with_context(|| format!("unknown workspace {name:?}"))
}

/// Ask the daemon to stop one session's turn.
///
/// An interruption that fails is logged rather than raised: the consumer has
/// already been told its turn was cancelled, and a failure here changes what
/// the daemon is doing, not what the consumer was promised.
async fn interrupt(client: &ApiClient, session_id: &str) {
    if let Err(error) = client.interrupt_turn(session_id).await {
        tracing::warn!(%error, %session_id, "could not interrupt a turn");
    }
}

/// Run one turn and report how it ended.
///
/// The final message is emitted through `notify` before this returns, so a
/// consumer that reads only updates still sees the answer, and the stop reason
/// never claims success for a turn that failed.
async fn run_turn(
    client: &ApiClient,
    session_id: &str,
    prompt: &str,
    notify: &mut impl FnMut(&str, &str) -> Result<()>,
    cancelled: impl Fn() -> bool,
) -> Result<StopReason> {
    let accepted = client
        .prompt(session_id, prompt.to_owned())
        .await
        .context("submit the prompt")?;
    // A cancel that arrived while the prompt was being submitted found no turn
    // to stop. Now there is one.
    if cancelled() {
        interrupt(client, session_id).await;
    }
    let waited = client
        .wait(
            session_id,
            &WaitRequest {
                // Ask to be told about a structured input request rather than
                // waiting for an answer that will never come: the consumer is a
                // program, and this adapter has no one to ask.
                return_on_input: true,
                turn_id: Some(accepted.turn_id),
                timeout_secs: None,
            },
        )
        .await
        .context("wait for the turn")?;
    if let Some(pending) = pending_input(&waited) {
        // End the turn instead of leaving the session waiting for a person. The
        // reason travels as an error rather than a stop reason because the
        // protocol gives a refusal nowhere to carry its explanation, and a
        // consumer that cannot see why its turn stopped has gained nothing.
        interrupt(client, session_id).await;
        bail!("this session is waiting for input a program cannot provide: {pending}");
    }
    if let Some(message) = waited
        .final_message
        .as_deref()
        .filter(|message| !message.trim().is_empty())
    {
        notify(session_id, message)?;
    }
    // A turn that ended for any reason other than success is a refusal, never
    // an end of turn: a consumer that treats `EndTurn` as success would
    // otherwise accept a failed or quota-limited run as a finished one.
    Ok(match waited.outcome {
        WaitOutcome::Finished => StopReason::EndTurn,
        WaitOutcome::Cancelled => StopReason::Cancelled,
        // Normally answered above, with the request named. Kept so the mapping
        // stays total if a wait ever reports input without a pending request.
        WaitOutcome::InputRequired
        | WaitOutcome::Error
        | WaitOutcome::QuotaLimit
        | WaitOutcome::Timeout
        | WaitOutcome::Stopped => StopReason::Refusal,
    })
}

/// What a turn is waiting to be asked, when it is waiting for input.
fn pending_input(waited: &WaitResponse) -> Option<String> {
    if !matches!(waited.outcome, WaitOutcome::InputRequired) {
        return None;
    }
    Some(match waited.pending_elicitations.first() {
        Some(request) => request.message.clone(),
        None => "the session is waiting for input".to_owned(),
    })
}

/// The text of an ACP prompt.
///
/// Attachments and resource links are dropped rather than refused. A resource
/// link names something in the workspace the session already runs in, so the
/// agent can read it directly, and a consumer that sends one beside a text
/// prompt should not have its turn rejected for it.
fn prompt_text(blocks: &[ContentBlock]) -> Result<String> {
    let text = blocks
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text(text) => Some(text.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    if text.trim().is_empty() {
        bail!("the prompt carried no text, and this adapter forwards text prompts");
    }
    Ok(text)
}

/// The session request one ACP session creation turns into.
fn start_request(
    args: &AcpArgs,
    workspace_id: Option<String>,
    cwd: &std::path::Path,
) -> StartSessionRequest {
    StartSessionRequest {
        workspace_id,
        profile_id: args.profile.clone(),
        target_id: args.target.clone(),
        bundle_id: args.bundle.clone(),
        // A managed target provisions its own workspace from the bundle. A
        // local one works in the directory the consumer is already in, which is
        // what its working directory means.
        project_directory: args.bundle.is_none().then(|| cwd.to_path_buf()),
        ..StartSessionRequest::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::extract::{Path, State};
    use axum::routing::{get, post};
    use axum::{Json, Router};
    use futures::channel::mpsc;
    use futures::{SinkExt, StreamExt};
    use mj_controller::server::api::{API_VERSION, API_VERSION_HEADER};
    use serde_json::{Value, json};
    use std::path::Path as StdPath;
    use std::time::Duration;

    /// Every documented response carries the contract version, and the client
    /// refuses a server that does not, so the fake has to send it too.
    fn version() -> [(&'static str, &'static str); 1] {
        [(API_VERSION_HEADER, API_VERSION)]
    }

    /// How the fake daemon should answer one turn.
    #[derive(Default)]
    struct FakeTurn {
        outcome: &'static str,
        final_message: Option<&'static str>,
        /// The question a turn is waiting for, when it is waiting for one.
        pending_message: Option<&'static str>,
        /// Whether interrupting fails, which is what a cancel racing a dead
        /// daemon looks like.
        interrupt_fails: bool,
        /// Whether the turn runs until it is interrupted, the way a real turn
        /// that outlasts the consumer's patience does. The wait then answers
        /// `cancelled`.
        runs_until_interrupted: bool,
    }

    /// A daemon that answers one turn and records what it was asked.
    ///
    /// Hand-written rather than mocked, the same way the controller's own route
    /// tests are: the adapter's job is to say the right things over HTTP, so the
    /// test asserts the bytes that arrived, not that a method was called.
    struct FakeDaemon {
        turn: FakeTurn,
        start: Mutex<Vec<Value>>,
        prompt: Mutex<Vec<(String, Value)>>,
        wait: Mutex<Vec<(String, Value)>>,
        interrupts: Mutex<Vec<String>>,
        interrupted: tokio::sync::Notify,
    }

    impl FakeDaemon {
        async fn start(turn: FakeTurn) -> (Arc<ApiClient>, Arc<Self>) {
            let daemon = Arc::new(Self {
                turn,
                start: Mutex::new(Vec::new()),
                prompt: Mutex::new(Vec::new()),
                wait: Mutex::new(Vec::new()),
                interrupts: Mutex::new(Vec::new()),
                interrupted: tokio::sync::Notify::new(),
            });
            let app = Router::new()
                .route("/api/v1/sessions", post(record_start))
                .route("/api/v1/workspaces", get(list_workspaces))
                .route("/api/v1/sessions/{session_id}/prompt", post(record_prompt))
                .route("/api/v1/sessions/{session_id}/wait", post(record_wait))
                .route(
                    "/api/v1/sessions/{session_id}/interrupt-turn",
                    post(record_interrupt),
                )
                .with_state(Arc::clone(&daemon));
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("bind the fake daemon");
            let address = listener.local_addr().expect("fake daemon address");
            tokio::spawn(async move {
                let _ = axum::serve(listener, app).await;
            });
            let client = ApiClient::new(format!("http://{address}"), "test-token".to_owned())
                .expect("build the client");
            (Arc::new(client), daemon)
        }
    }

    async fn record_start(
        State(daemon): State<Arc<FakeDaemon>>,
        Json(body): Json<Value>,
    ) -> ([(&'static str, &'static str); 1], Json<Value>) {
        daemon.start.lock().unwrap().push(body);
        (
            version(),
            Json(json!({"session_id": "session-1", "turn_id": null})),
        )
    }

    /// Two workspaces, which is when the daemon refuses to pick one itself.
    async fn list_workspaces() -> ([(&'static str, &'static str); 1], Json<Value>) {
        let workspace = |id: &str, name: &str| json!({"id": id, "name": name, "created_at": "now", "last_opened_at": "now", "session_count": 0});
        (
            version(),
            Json(
                json!({"workspaces": [workspace("ws-1", "default"), workspace("ws-2", "Release")]}),
            ),
        )
    }

    async fn record_prompt(
        State(daemon): State<Arc<FakeDaemon>>,
        Path(session_id): Path<String>,
        Json(body): Json<Value>,
    ) -> ([(&'static str, &'static str); 1], Json<Value>) {
        daemon.prompt.lock().unwrap().push((session_id, body));
        (version(), Json(json!({"turn_id": 7})))
    }

    async fn record_wait(
        State(daemon): State<Arc<FakeDaemon>>,
        Path(session_id): Path<String>,
        Json(body): Json<Value>,
    ) -> ([(&'static str, &'static str); 1], Json<Value>) {
        daemon.wait.lock().unwrap().push((session_id.clone(), body));
        let outcome = if daemon.turn.runs_until_interrupted {
            daemon.interrupted.notified().await;
            "cancelled"
        } else {
            daemon.turn.outcome
        };
        // A wait answers with the session's public view as well as the outcome,
        // so the fake has to carry one: the shape is the daemon's contract, not
        // something this test gets to simplify.
        let session = json!({
            "id": session_id,
            "workspace_id": "default",
            "title": "adapter session",
            "harness_kind": "codex",
            "profile_id": "codex-work",
            "target_id": "localhost",
            "bundle_id": "",
            "state": "running",
            "lifecycle": "live",
            "chat_phase": "idle",
            "is_idle": true,
            "has_error": false,
            "created_at": "now",
            "updated_at": "now"
        });
        let pending: Vec<Value> = daemon
            .turn
            .pending_message
            .map(|message| vec![json!({"id": "q1", "message": message, "fields": []})])
            .unwrap_or_default();
        (
            version(),
            Json(json!({
                "outcome": outcome,
                "final_message": daemon.turn.final_message,
                "pending_elicitations": pending,
                "session": session
            })),
        )
    }

    async fn record_interrupt(
        State(daemon): State<Arc<FakeDaemon>>,
        Path(session_id): Path<String>,
    ) -> ([(&'static str, &'static str); 1], axum::http::StatusCode) {
        daemon.interrupts.lock().unwrap().push(session_id);
        daemon.interrupted.notify_one();
        let status = if daemon.turn.interrupt_fails {
            axum::http::StatusCode::INTERNAL_SERVER_ERROR
        } else {
            axum::http::StatusCode::OK
        };
        (version(), status)
    }

    /// A consumer on the far side of the adapter's standard input and output.
    struct Consumer {
        to_adapter: mpsc::UnboundedSender<std::io::Result<String>>,
        from_adapter: mpsc::UnboundedReceiver<String>,
        served: tokio::task::JoinHandle<Result<()>>,
    }

    impl Consumer {
        fn connect(adapter: Arc<Adapter>) -> Self {
            let (to_adapter, incoming) = mpsc::unbounded();
            let (outgoing, from_adapter) = mpsc::unbounded::<String>();
            let outgoing = outgoing.sink_map_err(std::io::Error::other);
            let transport = agent_client_protocol::Lines::new(outgoing, incoming);
            let served = tokio::spawn(serve_on(adapter, transport));
            Self {
                to_adapter,
                from_adapter,
                served,
            }
        }

        fn send(&self, message: Value) {
            self.to_adapter
                .unbounded_send(Ok(message.to_string()))
                .expect("the adapter is reading");
        }

        fn request(&self, id: u64, method: &str, params: Value) {
            self.send(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
        }

        /// The adapter's answer to one request, skipping notifications.
        async fn response(&mut self, id: u64) -> Value {
            tokio::time::timeout(Duration::from_secs(10), async {
                loop {
                    let line = self.from_adapter.next().await.expect("the adapter answers");
                    let message: Value = serde_json::from_str(&line).expect("a JSON-RPC line");
                    if message["id"] == id {
                        return message;
                    }
                }
            })
            .await
            .expect("the adapter answers within ten seconds")
        }

        /// Start a session, the way every consumer does before its first prompt.
        async fn open_session(&mut self) {
            self.request(
                1,
                "initialize",
                json!({"protocolVersion": 1, "clientCapabilities": {}}),
            );
            self.response(1).await;
            self.request(
                2,
                "session/new",
                json!({"cwd": "/work/project", "mcpServers": []}),
            );
            let created = self.response(2).await;
            assert_eq!(created["result"]["sessionId"], "session-1", "{created}");
        }

        fn prompt(&self, id: u64) {
            self.request(
                id,
                "session/prompt",
                json!({"sessionId": "session-1", "prompt": [{"type": "text", "text": "work"}]}),
            );
        }
    }

    /// Wait until the fake has seen the turn's wait, so the turn is running.
    async fn until_waiting(daemon: &FakeDaemon) {
        tokio::time::timeout(Duration::from_secs(10), async {
            while daemon.wait.lock().unwrap().is_empty() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the turn starts waiting");
    }

    #[tokio::test]
    async fn a_cancel_sent_during_a_turn_stops_it_and_answers_cancelled() {
        // The prompt's answer used to be computed inside the message handler,
        // and the connection handles one message at a time, so the cancel was
        // read only after the turn had ended on its own as `end_turn` (F-2).
        let (client, daemon) = FakeDaemon::start(FakeTurn {
            runs_until_interrupted: true,
            ..FakeTurn::default()
        })
        .await;
        let mut consumer = Consumer::connect(Arc::new(Adapter::new(
            AcpArgs::default(),
            None,
            Some(client),
        )));
        consumer.open_session().await;
        consumer.prompt(3);
        until_waiting(&daemon).await;

        consumer.send(json!({"jsonrpc": "2.0", "method": "session/cancel", "params": {"sessionId": "session-1"}}));
        let answer = consumer.response(3).await;

        assert_eq!(answer["result"]["stopReason"], "cancelled", "{answer}");
        assert_eq!(daemon.interrupts.lock().unwrap().as_slice(), ["session-1"]);
    }

    #[tokio::test]
    async fn a_consumer_that_goes_away_mid_turn_has_its_turn_stopped() {
        // Closing the pipe used to wait for the running prompt handler, so by
        // the time the adapter looked for turns to stop there were none (F-3).
        let (client, daemon) = FakeDaemon::start(FakeTurn {
            runs_until_interrupted: true,
            ..FakeTurn::default()
        })
        .await;
        let mut consumer = Consumer::connect(Arc::new(Adapter::new(
            AcpArgs::default(),
            None,
            Some(client),
        )));
        consumer.open_session().await;
        consumer.prompt(3);
        until_waiting(&daemon).await;

        consumer.to_adapter.close_channel();
        tokio::time::timeout(Duration::from_secs(10), consumer.served)
            .await
            .expect("the adapter exits once its consumer is gone")
            .expect("the adapter task")
            .expect("a closed pipe is a clean exit");

        assert_eq!(daemon.interrupts.lock().unwrap().as_slice(), ["session-1"]);
    }

    #[tokio::test]
    async fn a_named_workspace_is_the_one_the_session_is_created_in() {
        // With two workspaces the daemon will not pick one, so a name the
        // consumer gave on the command line has to reach it (F-1).
        let (client, daemon) = FakeDaemon::start(FakeTurn::default()).await;
        let mut consumer = Consumer::connect(Arc::new(Adapter::new(
            AcpArgs::default(),
            Some("release".to_owned()),
            Some(client),
        )));
        consumer.open_session().await;

        assert_eq!(daemon.start.lock().unwrap()[0]["workspace_id"], "ws-2");
    }

    #[tokio::test]
    async fn without_a_workspace_the_daemon_chooses_as_it_does_for_mj_new() {
        let (client, daemon) = FakeDaemon::start(FakeTurn::default()).await;
        let mut consumer = Consumer::connect(Arc::new(Adapter::new(
            AcpArgs::default(),
            None,
            Some(client),
        )));
        consumer.open_session().await;

        assert!(daemon.start.lock().unwrap()[0]["workspace_id"].is_null());
    }

    #[tokio::test]
    async fn an_unknown_workspace_is_named_in_the_refusal() {
        let (client, daemon) = FakeDaemon::start(FakeTurn::default()).await;
        let mut consumer = Consumer::connect(Arc::new(Adapter::new(
            AcpArgs::default(),
            Some("nowhere".to_owned()),
            Some(client),
        )));
        consumer.request(
            2,
            "session/new",
            json!({"cwd": "/work/project", "mcpServers": []}),
        );
        let refused = consumer.response(2).await;

        assert!(
            refused["error"]["data"].to_string().contains("nowhere"),
            "{refused}"
        );
        assert!(daemon.start.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_finished_turn_emits_the_answer_and_reports_end_turn() {
        let (client, daemon) = FakeDaemon::start(FakeTurn {
            outcome: "finished",
            final_message: Some("the answer"),
            ..FakeTurn::default()
        })
        .await;
        let mut sent: Vec<(String, String)> = Vec::new();
        let mut notify = |session_id: &str, message: &str| -> Result<()> {
            sent.push((session_id.to_owned(), message.to_owned()));
            Ok(())
        };

        let stop = run_turn(&client, "session-1", "do the thing", &mut notify, || false)
            .await
            .expect("the turn runs");

        assert_eq!(stop, StopReason::EndTurn);
        assert_eq!(
            sent,
            [("session-1".to_owned(), "the answer".to_owned())],
            "a consumer that reads only updates still sees the answer"
        );
        let prompt = daemon.prompt.lock().unwrap();
        assert_eq!(prompt[0].0, "session-1");
        assert_eq!(prompt[0].1["text"], "do the thing");
        let wait = daemon.wait.lock().unwrap();
        assert_eq!(wait[0].1["turn_id"], 7);
    }

    #[tokio::test]
    async fn every_turn_that_did_not_finish_is_refused() {
        // A consumer treats `EndTurn` as success, so nothing short of a finished
        // turn may report it. Each of these is a different reason a turn stops
        // without succeeding. `input_required` is answered differently, with the
        // question named, which its own test covers.
        for outcome in ["error", "quota_limit", "timeout", "stopped"] {
            let (client, _daemon) = FakeDaemon::start(FakeTurn {
                outcome,
                final_message: Some("something went wrong"),
                ..FakeTurn::default()
            })
            .await;
            let mut sent: Vec<(String, String)> = Vec::new();
            let mut notify = |session_id: &str, message: &str| -> Result<()> {
                sent.push((session_id.to_owned(), message.to_owned()));
                Ok(())
            };

            let stop = run_turn(&client, "session-1", "hello", &mut notify, || false)
                .await
                .expect("the turn runs");

            assert_eq!(stop, StopReason::Refusal, "outcome {outcome}");
            assert_eq!(sent.len(), 1, "outcome {outcome} still reports its message");
        }
    }

    #[tokio::test]
    async fn a_cancelled_turn_reports_cancelled() {
        let (client, _daemon) = FakeDaemon::start(FakeTurn {
            outcome: "cancelled",
            ..FakeTurn::default()
        })
        .await;
        let mut notify = |_session_id: &str, _message: &str| -> Result<()> { Ok(()) };

        let stop = run_turn(&client, "session-1", "hello", &mut notify, || false)
            .await
            .expect("the turn runs");

        assert_eq!(stop, StopReason::Cancelled);
    }

    #[test]
    fn a_bundle_session_uses_the_managed_workspace_and_a_bare_one_the_consumers_directory() {
        let args = AcpArgs {
            profile: Some("codex-work".to_owned()),
            target: Some("builder-podman".to_owned()),
            bundle: Some("product".to_owned()),
        };
        let managed = start_request(&args, None, StdPath::new("/work/project"));
        assert_eq!(managed.profile_id.as_deref(), Some("codex-work"));
        assert_eq!(managed.target_id.as_deref(), Some("builder-podman"));
        assert_eq!(managed.bundle_id.as_deref(), Some("product"));
        assert!(
            managed.project_directory.is_none(),
            "a managed target provisions its own workspace"
        );

        let bare = start_request(
            &AcpArgs {
                bundle: None,
                ..args
            },
            None,
            StdPath::new("/work/project"),
        );
        assert!(bare.bundle_id.is_none());
        assert_eq!(
            bare.project_directory.as_deref(),
            Some(StdPath::new("/work/project"))
        );
    }

    #[test]
    fn a_prompt_is_the_text_of_its_blocks_and_nothing_else() {
        let blocks = [
            ContentBlock::Text(TextContent::new("first")),
            ContentBlock::Text(TextContent::new("second")),
        ];
        assert_eq!(prompt_text(&blocks).unwrap(), "first\nsecond");
        assert!(
            prompt_text(&[]).is_err(),
            "a turn with no text has nothing to run"
        );
        assert!(
            prompt_text(&[ContentBlock::Text(TextContent::new("   "))]).is_err(),
            "whitespace is not a prompt"
        );
    }

    /// An adapter that already knows one session, without a daemon to create it.
    fn adapter_owning(session_id: &str, client: Arc<ApiClient>) -> Adapter {
        let adapter = Adapter::new(AcpArgs::default(), None, Some(client));
        adapter
            .sessions
            .lock()
            .unwrap()
            .insert(session_id.to_owned());
        adapter
    }

    #[tokio::test]
    async fn a_cancel_is_answered_cancelled_even_when_the_interrupt_fails() {
        // The specification requires `Cancelled` when the client sends
        // `session/cancel`, even if cancellation raises underneath. Here the
        // turn finishes normally and the interrupt fails, which is the worst
        // case: a consumer would otherwise read a successful end of turn for
        // work it asked to stop.
        let (client, daemon) = FakeDaemon::start(FakeTurn {
            outcome: "finished",
            final_message: Some("the work finished anyway"),
            interrupt_fails: true,
            ..FakeTurn::default()
        })
        .await;
        let adapter = adapter_owning("session-1", client);
        let mut notify = |_session_id: &str, _message: &str| -> Result<()> { Ok(()) };

        adapter.cancel("session-1").await;
        let stop = adapter
            .turn("session-1", "hello", &mut notify)
            .await
            .expect("the turn runs");

        assert_eq!(stop, StopReason::Cancelled);
        assert_eq!(
            daemon.interrupts.lock().unwrap().as_slice(),
            ["session-1", "session-1"],
            "the daemon was asked to stop, and asked again once the prompt became a turn"
        );
    }

    #[tokio::test]
    async fn an_input_request_ends_the_turn_and_names_what_it_waits_for() {
        let (client, daemon) = FakeDaemon::start(FakeTurn {
            outcome: "input_required",
            pending_message: Some("Which branch should I target?"),
            ..FakeTurn::default()
        })
        .await;
        let adapter = adapter_owning("session-1", client);
        let mut notify = |_session_id: &str, _message: &str| -> Result<()> { Ok(()) };

        let error = adapter
            .turn("session-1", "hello", &mut notify)
            .await
            .expect_err("a program cannot answer a question");

        let message = format!("{error:#}");
        assert!(
            message.contains("Which branch should I target?"),
            "the consumer is told what the session is waiting for: {message}"
        );
        assert_eq!(
            daemon.interrupts.lock().unwrap().as_slice(),
            ["session-1"],
            "a turn nobody can answer is ended rather than left waiting"
        );
    }

    #[tokio::test]
    async fn leaving_stops_the_turns_the_consumer_can_no_longer_see() {
        let (client, daemon) = FakeDaemon::start(FakeTurn::default()).await;
        let adapter = adapter_owning("session-1", client);
        adapter
            .active
            .lock()
            .unwrap()
            .insert("session-1".to_owned());

        adapter.stop_active_turns().await;

        assert_eq!(daemon.interrupts.lock().unwrap().as_slice(), ["session-1"]);
        assert!(
            adapter.sessions.lock().unwrap().contains("session-1"),
            "a durable session outlives the consumer that asked for it"
        );
    }
}
