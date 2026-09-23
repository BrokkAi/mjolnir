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
    AgentCapabilities, ContentBlock, ContentChunk, InitializeRequest, InitializeResponse,
    NewSessionRequest, NewSessionResponse, PromptRequest, PromptResponse, SessionId,
    SessionNotification, SessionUpdate, StopReason, TextContent,
};
use agent_client_protocol::{Agent, Stdio};
use anyhow::{Context, Result, bail};
use clap::Args;

use mj_controller::server::api::{StartSessionRequest, WaitOutcome, WaitRequest};

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
    /// The sessions this process created. A prompt may only name one of these,
    /// so an adapter instance cannot be used to drive unrelated sessions that
    /// happen to live in the same daemon.
    sessions: Mutex<HashSet<String>>,
}

/// Serve the Agent Client Protocol on standard input and output.
///
/// Returns when the consumer closes its side of the pipe or the connection
/// fails.
pub(crate) async fn serve(args: AcpArgs) -> Result<()> {
    let adapter = Arc::new(Adapter {
        args,
        sessions: Mutex::new(HashSet::new()),
    });
    Agent
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
                    let session_id = request.session_id.0.to_string();
                    let prompt = match prompt_text(&request.prompt) {
                        Ok(prompt) => prompt,
                        Err(error) => {
                            return responder.respond_with_internal_error(format!("{error:#}"));
                        }
                    };
                    let mut notify = |session_id: &str, message: &str| -> Result<()> {
                        cx.send_notification(SessionNotification::new(
                            SessionId::new(session_id),
                            SessionUpdate::AgentMessageChunk(ContentChunk::new(
                                ContentBlock::Text(TextContent::new(message)),
                            )),
                        ))
                        .map_err(|error| anyhow::anyhow!("send a session update: {error}"))
                    };
                    match adapter.turn(&session_id, &prompt, &mut notify).await {
                        Ok(stop_reason) => responder.respond(PromptResponse::new(stop_reason)),
                        Err(error) => responder.respond_with_internal_error(format!("{error:#}")),
                    }
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .connect_to(Stdio::new())
        .await
        .context("serving the Agent Client Protocol on standard input and output")
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
    /// Create the Mjolnir session that one ACP session will run in.
    ///
    /// The ACP session id is the Mjolnir session id. Keeping them the same
    /// means a consumer's logs, `mj sessions`, and the daemon's own records all
    /// name one thing, which is the difference between a debuggable integration
    /// and a search for a mapping table.
    async fn new_session(&self, request: NewSessionRequest) -> Result<SessionId> {
        let client = ApiClient::connect()
            .await
            .context("connect to the Mjolnir daemon")?;
        let start = start_request(&self.args, &request.cwd);
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
        let client = ApiClient::connect()
            .await
            .context("connect to the Mjolnir daemon")?;
        run_turn(&client, session_id, prompt, notify).await
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
) -> Result<StopReason> {
    let accepted = client
        .prompt(session_id, prompt.to_owned())
        .await
        .context("submit the prompt")?;
    let waited = client
        .wait(
            session_id,
            &WaitRequest {
                return_on_input: false,
                turn_id: Some(accepted.turn_id),
                timeout_secs: None,
            },
        )
        .await
        .context("wait for the turn")?;
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
        WaitOutcome::InputRequired
        | WaitOutcome::Error
        | WaitOutcome::QuotaLimit
        | WaitOutcome::Timeout
        | WaitOutcome::Stopped => StopReason::Refusal,
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
fn start_request(args: &AcpArgs, cwd: &std::path::Path) -> StartSessionRequest {
    StartSessionRequest {
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
    use axum::routing::post;
    use axum::{Json, Router};
    use mj_controller::server::api::{API_VERSION, API_VERSION_HEADER};
    use serde_json::{Value, json};
    use std::path::Path as StdPath;

    /// Every documented response carries the contract version, and the client
    /// refuses a server that does not, so the fake has to send it too.
    fn version() -> [(&'static str, &'static str); 1] {
        [(API_VERSION_HEADER, API_VERSION)]
    }

    /// A daemon that answers one turn and records what it was asked.
    ///
    /// Hand-written rather than mocked, the same way the controller's own route
    /// tests are: the adapter's job is to say the right things over HTTP, so the
    /// test asserts the bytes that arrived, not that a method was called.
    struct FakeDaemon {
        outcome: &'static str,
        final_message: Option<&'static str>,
        start: Mutex<Vec<Value>>,
        prompt: Mutex<Vec<(String, Value)>>,
        wait: Mutex<Vec<(String, Value)>>,
    }

    impl FakeDaemon {
        async fn start(
            outcome: &'static str,
            final_message: Option<&'static str>,
        ) -> (ApiClient, Arc<Self>) {
            let daemon = Arc::new(Self {
                outcome,
                final_message,
                start: Mutex::new(Vec::new()),
                prompt: Mutex::new(Vec::new()),
                wait: Mutex::new(Vec::new()),
            });
            let app = Router::new()
                .route("/api/v1/sessions", post(record_start))
                .route("/api/v1/sessions/{session_id}/prompt", post(record_prompt))
                .route("/api/v1/sessions/{session_id}/wait", post(record_wait))
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
            (client, daemon)
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
        daemon.wait.lock().unwrap().push((session_id, body));
        (
            version(),
            Json(json!({
                "outcome": daemon.outcome,
                "final_message": daemon.final_message,
                "session": session
            })),
        )
    }

    #[tokio::test]
    async fn a_finished_turn_emits_the_answer_and_reports_end_turn() {
        let (client, daemon) = FakeDaemon::start("finished", Some("the answer")).await;
        let mut sent: Vec<(String, String)> = Vec::new();
        let mut notify = |session_id: &str, message: &str| -> Result<()> {
            sent.push((session_id.to_owned(), message.to_owned()));
            Ok(())
        };

        let stop = run_turn(&client, "session-1", "do the thing", &mut notify)
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
        // without succeeding.
        for outcome in [
            "error",
            "quota_limit",
            "timeout",
            "stopped",
            "input_required",
        ] {
            let (client, _daemon) = FakeDaemon::start(outcome, Some("something went wrong")).await;
            let mut sent: Vec<(String, String)> = Vec::new();
            let mut notify = |session_id: &str, message: &str| -> Result<()> {
                sent.push((session_id.to_owned(), message.to_owned()));
                Ok(())
            };

            let stop = run_turn(&client, "session-1", "hello", &mut notify)
                .await
                .expect("the turn runs");

            assert_eq!(stop, StopReason::Refusal, "outcome {outcome}");
            assert_eq!(sent.len(), 1, "outcome {outcome} still reports its message");
        }
    }

    #[tokio::test]
    async fn a_cancelled_turn_reports_cancelled() {
        let (client, _daemon) = FakeDaemon::start("cancelled", None).await;
        let mut notify = |_session_id: &str, _message: &str| -> Result<()> { Ok(()) };

        let stop = run_turn(&client, "session-1", "hello", &mut notify)
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
        let managed = start_request(&args, StdPath::new("/work/project"));
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
}
