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
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::schema::v1::{
    AgentCapabilities, CancelNotification, ContentBlock, ContentChunk, InitializeRequest,
    InitializeResponse, NewSessionRequest, NewSessionResponse, PromptRequest, PromptResponse,
    SessionId, SessionNotification, SessionUpdate, StopReason, TextContent,
};
use agent_client_protocol::{Agent, Stdio};
use anyhow::{Context, Result, bail};
use clap::Args;

use mj_controller::server::api::{
    ApiSession, StartSessionRequest, WaitOutcome, WaitRequest, WaitResponse,
};
use mj_controller::server::{ViewerChatPhase, ViewerLifecycleCategory};
use tokio_util::task::TaskTracker;

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
    #[command(flatten)]
    pub(crate) workspace: crate::WorkspaceName,
    /// What happens to the sessions this process created when it exits.
    #[arg(long = "on-exit", value_enum, default_value_t = ExitPolicy::Keep)]
    pub(crate) on_exit: ExitPolicy,
}

/// What the adapter does with its sessions when its consumer is gone.
///
/// The policy runs once, when the adapter exits, over every session it
/// created: a consumer that sends several prompts to one session, or opens
/// several sessions, is not interrupted between them.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, clap::ValueEnum)]
pub(crate) enum ExitPolicy {
    /// Leave every session as it is, for a consumer a person follows up on.
    #[default]
    Keep,
    /// Checkpoint each session and release its worker and target, leaving a
    /// session a person can resume.
    Suspend,
    /// Destroy each session once its turn has stopped, for a scheduler that
    /// takes its answer from the turn and wants nothing left behind.
    Destroy,
}

impl ExitPolicy {
    fn name(self) -> &'static str {
        match self {
            Self::Keep => "keep",
            Self::Suspend => "suspend",
            Self::Destroy => "destroy",
        }
    }
}

/// How long the exit policy waits for the daemon.
///
/// Suspension and destruction are answered when the daemon admits them, not
/// when they finish, so the adapter watches the session to learn how each one
/// ended. These bound that watch; they never cancel the daemon's work.
#[derive(Debug, Clone, Copy)]
struct ExitTiming {
    /// How often the session is looked at.
    poll: Duration,
    /// How long an interrupted turn may take to stop before destruction is
    /// refused. Destroying a session with a turn still running would throw
    /// away work nobody has seen.
    settle: Duration,
    /// How long a suspension or destruction may take. A suspension checkpoints
    /// the workspace, which on a large checkout over SSH takes minutes.
    finish: Duration,
}

impl Default for ExitTiming {
    fn default() -> Self {
        Self {
            poll: Duration::from_secs(1),
            settle: Duration::from_secs(60),
            finish: Duration::from_secs(300),
        }
    }
}

/// What the adapter needs beyond one request.
struct Adapter {
    args: AcpArgs,
    /// The workspace named with `--workspace`, resolved to its id
    /// when a session is created.
    workspace: Option<String>,
    /// The client every request goes through, when one was supplied. The
    /// real adapter connects to the daemon on demand instead, so that
    /// `initialize` works before the daemon has started.
    client: Option<Arc<ApiClient>>,
    timing: ExitTiming,
    /// Session creations in flight. Each runs on its own task so a creation
    /// the daemon accepted is recorded even when the consumer leaves before
    /// the answer arrives, and the exit policy waits for them, so it never
    /// misses a session because its creation had not answered yet.
    creating: TaskTracker,
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
/// Returns when the consumer closes its side of the pipe, the connection
/// fails, or, under a policy other than `keep`, the process is asked to stop;
/// in each case once the exit policy has been applied.
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
                async move |request: NewSessionRequest, responder, _cx| match Arc::clone(&adapter)
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
        .connect_to(transport);
    let served = match adapter.args.on_exit {
        // Unchanged from before the policy existed: a signal ends the process
        // where it stands.
        ExitPolicy::Keep => served.await,
        // A consumer that stops its agent with a signal instead of closing the
        // pipe gets the policy it asked for all the same.
        ExitPolicy::Suspend | ExitPolicy::Destroy => tokio::select! {
            served = served => served,
            signal = termination() => {
                tracing::info!(signal, "stopping on a signal");
                Ok(())
            }
        },
    };
    // The consumer is gone, so nothing it started can be watched or steered any
    // more: stop the turns it can no longer see. What happens to the sessions
    // themselves is the exit policy's decision.
    adapter.stop_active_turns().await;
    let retired = adapter.apply_exit_policy().await;
    let served = served.context("serving the Agent Client Protocol on standard input and output");
    match (served, retired) {
        (Ok(()), retired) => retired,
        (Err(error), Ok(())) => Err(error),
        (Err(error), Err(retired)) => Err(error.context(format!("{retired:#}"))),
    }
}

/// Resolve when the process is asked to stop, naming the signal.
async fn termination() -> &'static str {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let (Ok(mut terminate), Ok(mut hangup)) = (
            signal(SignalKind::terminate()),
            signal(SignalKind::hangup()),
        ) else {
            tracing::warn!("could not listen for termination signals; only an interrupt is heard");
            let _ = tokio::signal::ctrl_c().await;
            return "interrupt";
        };
        tokio::select! {
            _ = terminate.recv() => "terminate",
            _ = hangup.recv() => "hangup",
            _ = tokio::signal::ctrl_c() => "interrupt",
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
        "interrupt"
    }
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
            timing: ExitTiming::default(),
            creating: TaskTracker::new(),
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
    async fn new_session(self: Arc<Self>, request: NewSessionRequest) -> Result<SessionId> {
        let adapter = Arc::clone(&self);
        let created = self.creating.spawn(async move {
            let client = adapter.client().await?;
            // The same rule `mj new` follows: a named workspace is looked up,
            // and without one the daemon picks, which it can only do when
            // there is exactly one.
            let workspace_id = match adapter.workspace.as_deref() {
                Some(name) => Some(resolve_workspace(&client, name).await?),
                None => None,
            };
            let start = start_request(&adapter.args, workspace_id, &request.cwd);
            let started = client
                .start(&start)
                .await
                .map_err(crate::api_commands::name_launch_flags)
                .context("create the session")?;
            adapter
                .sessions
                .lock()
                .expect("adapter session set")
                .insert(started.session_id.clone());
            anyhow::Ok(started.session_id)
        });
        let session_id = created
            .await
            .context("the session creation task failed")??;
        Ok(SessionId::new(session_id))
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
        let cancel_requested = self
            .cancelling
            .lock()
            .expect("adapter cancel set")
            .remove(session_id);
        match result {
            // A turn that finished before the cancel could stop it delivered
            // its reply. Calling it cancelled would tell the consumer to
            // discard work that was done (R2-1).
            Ok(StopReason::EndTurn) => Ok(StopReason::EndTurn),
            // The specification requires `Cancelled` for a cancelled turn
            // even when the work underneath fails while it stops. The failure
            // is still worth recording.
            result if cancel_requested => {
                if let Err(error) = &result {
                    tracing::debug!(%error, %session_id, "a cancelled turn also failed");
                }
                Ok(StopReason::Cancelled)
            }
            result => result,
        }
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

    /// Apply the exit policy to every session this adapter created.
    ///
    /// The daemon is only reached when there is something to retire.
    ///
    /// Sessions are retired concurrently and independently, so one that fails
    /// does not keep the others alive. Every failure is reported, naming the
    /// session and the state it was left in.
    async fn apply_exit_policy(&self) -> Result<()> {
        let policy = self.args.on_exit;
        if policy == ExitPolicy::Keep {
            return Ok(());
        }
        self.creating.close();
        self.creating.wait().await;
        let sessions = self.owned_sessions();
        if sessions.is_empty() {
            return Ok(());
        }
        let client = self.client().await.with_context(|| {
            format!(
                "reach the Mjolnir daemon to {} sessions {}; they were left as they were",
                policy.name(),
                sessions.join(", ")
            )
        })?;
        let client = client.as_ref();
        let outcomes = futures::future::join_all(sessions.iter().map(|session_id| async move {
            match policy {
                ExitPolicy::Keep => Ok(()),
                ExitPolicy::Suspend => suspend_session(client, session_id, self.timing).await,
                ExitPolicy::Destroy => destroy_session(client, session_id, self.timing).await,
            }
        }))
        .await;
        let failures: Vec<String> = outcomes
            .into_iter()
            .filter_map(Result::err)
            .map(|error| format!("{error:#}"))
            .collect();
        if failures.is_empty() {
            return Ok(());
        }
        bail!(
            "the --on-exit {} policy did not complete: {}",
            policy.name(),
            failures.join("; ")
        )
    }

    fn owned_sessions(&self) -> Vec<String> {
        let mut sessions: Vec<String> = self
            .sessions
            .lock()
            .expect("adapter session set")
            .iter()
            .cloned()
            .collect();
        sessions.sort();
        sessions
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
    let workspaces = client
        .workspaces()
        .await
        .context("list the workspaces")?
        .workspaces;
    workspaces
        .iter()
        .find(|workspace| workspace.name.to_lowercase() == wanted)
        .map(|workspace| workspace.id.clone())
        .ok_or_else(|| crate::unknown_workspace(name, &workspaces))
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

/// Suspend one session and wait until the daemon says how that ended.
///
/// A session that is already suspended is left alone. A suspension that fails
/// leaves the session live, and the daemon records why on it; that reason is
/// what the consumer is told.
async fn suspend_session(client: &ApiClient, session_id: &str, timing: ExitTiming) -> Result<()> {
    let Some(session) = look(client, session_id).await? else {
        bail!("session {session_id} no longer exists, so it could not be suspended");
    };
    if session.lifecycle == ViewerLifecycleCategory::Suspended {
        return Ok(());
    }
    // Unpublished Git work is never acknowledged on a person's behalf: the
    // daemon refuses, and the session stays live for a person to publish it.
    client.suspend(session_id, false).await.with_context(|| {
        format!(
            "suspend session {session_id}; it was left {}, and `mj suspend --session {session_id}` retries",
            session.state
        )
    })?;
    let progress = Progress::since(&session);
    watch(client, session_id, timing, timing.finish, "suspend", |session| {
        let Some(session) = session else {
            return Watch::Failed("it no longer exists".to_owned());
        };
        if progress.not_taken_up(session) {
            return Watch::Pending;
        }
        match session.lifecycle {
            ViewerLifecycleCategory::Suspended => Watch::Done,
            ViewerLifecycleCategory::Failed => {
                Watch::Failed(format!("it ended {}{}", session.state, reason(session)))
            }
            ViewerLifecycleCategory::Suspending => Watch::Pending,
            ViewerLifecycleCategory::Live | ViewerLifecycleCategory::Starting => {
                match lifecycle_failure(session) {
                    Some(error) => Watch::Failed(format!(
                        "{error}; it is still {}, and `mj suspend --session {session_id}` retries",
                        session.state
                    )),
                    // The daemon has not taken the suspension up yet.
                    None => Watch::Pending,
                }
            }
        }
    })
    .await
}

/// Destroy one session once its turn has stopped, and wait until it is gone.
///
/// Destruction is refused, not forced, when an interrupted turn does not stop
/// in time: the session is then left live for a person to inspect, rather than
/// removed with work nobody has seen.
async fn destroy_session(client: &ApiClient, session_id: &str, timing: ExitTiming) -> Result<()> {
    let settled = watch(
        client,
        session_id,
        timing,
        timing.settle,
        "stop the turn of",
        |session| {
            let Some(session) = session else {
                return Watch::Done;
            };
            match session.lifecycle {
                ViewerLifecycleCategory::Live if session.chat_phase == ViewerChatPhase::Running => {
                    Watch::Pending
                }
                // A close someone else started owns the session until it ends.
                ViewerLifecycleCategory::Suspending => Watch::Pending,
                // Provisioning is cancelled by the destruction itself, and
                // nothing runs in a suspended or failed session.
                ViewerLifecycleCategory::Live
                | ViewerLifecycleCategory::Starting
                | ViewerLifecycleCategory::Suspended
                | ViewerLifecycleCategory::Failed => Watch::Done,
            }
        },
    )
    .await;
    if let Err(error) = settled {
        bail!(
            "{error:#}; it was not destroyed, and `mj destroy --session {session_id}` removes it"
        );
    }
    let Some(before) = look(client, session_id).await? else {
        return Ok(());
    };
    client.destroy(session_id, false).await.with_context(|| {
        format!(
            "destroy session {session_id}; it was left as it was, and `mj destroy --session {session_id}` retries"
        )
    })?;
    let progress = Progress::since(&before);
    watch(
        client,
        session_id,
        timing,
        timing.finish,
        "destroy",
        |session| {
            let Some(session) = session else {
                return Watch::Done;
            };
            if progress.not_taken_up(session) {
                return Watch::Pending;
            }
            match session.lifecycle {
                ViewerLifecycleCategory::Failed => {
                    Watch::Failed(format!("it ended {}{}", session.state, reason(session)))
                }
                ViewerLifecycleCategory::Suspending => Watch::Pending,
                ViewerLifecycleCategory::Live
                | ViewerLifecycleCategory::Starting
                | ViewerLifecycleCategory::Suspended => match lifecycle_failure(session) {
                    Some(error) => Watch::Failed(format!(
                        "{error}; it is still {}, and `mj destroy --session {session_id}` retries",
                        session.state
                    )),
                    // The daemon has not taken the destruction up yet.
                    None => Watch::Pending,
                },
            }
        },
    )
    .await
}

/// Whether the daemon has taken up a suspension or destruction it admitted.
///
/// The daemon answers before the operation changes the session, so the first
/// looks after can still show the state it was asked about, including a failed
/// state or a failure an earlier operation recorded. Until the session moves
/// on from that, neither is this operation's outcome.
struct Progress<'a> {
    before: &'a ApiSession,
    taken_up: AtomicBool,
}

impl<'a> Progress<'a> {
    fn since(before: &'a ApiSession) -> Self {
        Self {
            before,
            taken_up: AtomicBool::new(false),
        }
    }

    fn not_taken_up(&self, session: &ApiSession) -> bool {
        if self.taken_up.load(Ordering::Relaxed) {
            return false;
        }
        if session.lifecycle == self.before.lifecycle && session.error == self.before.error {
            return true;
        }
        self.taken_up.store(true, Ordering::Relaxed);
        false
    }
}

/// What one look at a session says about the operation being watched.
enum Watch {
    Pending,
    Done,
    Failed(String),
}

/// Look at a session until `judge` says the operation ended, or `within`
/// passes. Running out of time is a failure: the daemon may still finish, but
/// the adapter cannot say that it did.
async fn watch(
    client: &ApiClient,
    session_id: &str,
    timing: ExitTiming,
    within: Duration,
    operation: &str,
    judge: impl Fn(Option<&ApiSession>) -> Watch,
) -> Result<()> {
    let deadline = tokio::time::Instant::now() + within;
    loop {
        let session = look(client, session_id).await?;
        match judge(session.as_ref()) {
            Watch::Done => return Ok(()),
            Watch::Failed(why) => bail!("could not {operation} session {session_id}: {why}"),
            Watch::Pending => {}
        }
        if tokio::time::Instant::now() >= deadline {
            let last = session.map_or_else(
                || "gone".to_owned(),
                |session| format!("{}{}", session.state, reason(&session)),
            );
            bail!(
                "could not {operation} session {session_id} within {}s; it was last {last}, and `mj sessions --session {session_id}` shows whether the daemon finished",
                within.as_secs()
            );
        }
        tokio::time::sleep(timing.poll).await;
    }
}

async fn look(client: &ApiClient, session_id: &str) -> Result<Option<ApiSession>> {
    client
        .session_if_known(session_id)
        .await
        .with_context(|| format!("look up session {session_id}"))
}

/// The sentence a failed suspension or destruction recorded on the session.
fn lifecycle_failure(session: &ApiSession) -> Option<&str> {
    session
        .error
        .as_deref()
        .filter(|error| mj_core::state::is_public_lifecycle_error(error))
}

fn reason(session: &ApiSession) -> String {
    session
        .error
        .as_deref()
        .map(|error| format!(" ({error})"))
        .unwrap_or_default()
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
    let wait_request = WaitRequest {
        // Ask to be told about a structured input request rather than waiting
        // for an answer that will never come: the consumer is a program, and
        // this adapter has no one to ask.
        return_on_input: true,
        turn_id: Some(accepted.turn_id),
        timeout_secs: None,
    };
    let waited = interrupt_once_cancelled(
        client,
        session_id,
        client.wait(session_id, &wait_request),
        &cancelled,
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

/// How often a cancelled turn's interrupt is sent again while the daemon
/// refuses it.
const INTERRUPT_RETRY_INTERVAL: Duration = Duration::from_millis(250);

/// Wait for a turn, stopping it once the consumer has cancelled it.
///
/// A cancel can reach the daemon before there is a turn to stop: while the
/// daemon still holds the prompt for a session that is starting, or before the
/// turn the prompt became is visible. The daemon refuses that interrupt, so it
/// is sent again until the daemon takes one or the turn ends on its own (R2-1).
async fn interrupt_once_cancelled<T>(
    client: &ApiClient,
    session_id: &str,
    wait: impl std::future::Future<Output = Result<T>>,
    cancelled: &impl Fn() -> bool,
) -> Result<T> {
    // A cancel that arrived while the prompt was being submitted is sent
    // before the wait starts.
    let mut taken = cancelled() && interrupt_taken(client, session_id).await;
    tokio::pin!(wait);
    let mut retry = tokio::time::interval_at(
        tokio::time::Instant::now() + INTERRUPT_RETRY_INTERVAL,
        INTERRUPT_RETRY_INTERVAL,
    );
    retry.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            waited = &mut wait => return waited,
            _ = retry.tick(), if !taken => {
                taken = cancelled() && interrupt_taken(client, session_id).await;
            }
        }
    }
}

/// Ask the daemon to stop a turn, answering whether it took the request.
async fn interrupt_taken(client: &ApiClient, session_id: &str) -> bool {
    match client.interrupt_turn(session_id).await {
        Ok(()) => true,
        Err(error) => {
            tracing::debug!(%error, %session_id, "the daemon did not take the interrupt; asking again");
            false
        }
    }
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
        /// Sessions whose suspension or destruction the daemon refuses.
        refuses: &'static [&'static str],
        /// How long creating a session takes to answer.
        start_delay: Duration,
        /// How long the daemon holds a prompt before it becomes a turn, the
        /// way it does while a new session's worker attaches.
        prompt_delay: Duration,
        /// Whether a held prompt is withdrawn by an interrupt, the way the
        /// daemon answers an interrupt that arrives during the hold.
        prompt_withdrawn_on_interrupt: bool,
        /// How many interrupts are refused because there is no turn to stop
        /// yet, before one is taken.
        interrupts_refused: usize,
    }
    /// One answer to looking a session up.
    #[derive(Clone)]
    enum Look {
        Missing,
        Session {
            lifecycle: &'static str,
            state: &'static str,
            chat_phase: &'static str,
            error: Option<&'static str>,
        },
    }

    const IDLE: Look = Look::Session {
        lifecycle: "live",
        state: "running",
        chat_phase: "idle",
        error: None,
    };
    const WORKING: Look = Look::Session {
        lifecycle: "live",
        state: "running",
        chat_phase: "running",
        error: None,
    };
    const SUSPENDING: Look = Look::Session {
        lifecycle: "suspending",
        state: "suspending",
        chat_phase: "idle",
        error: None,
    };
    const SUSPENDED: Look = Look::Session {
        lifecycle: "suspended",
        state: "suspended",
        chat_phase: "idle",
        error: None,
    };

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
        /// Every lifecycle call and lookup, in the order they arrived.
        calls: Mutex<Vec<String>>,
        /// What each lookup of a session answers, in turn. The last answer
        /// repeats, so a session stays where its script leaves it.
        looks: Mutex<std::collections::HashMap<String, std::collections::VecDeque<Look>>>,
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
                calls: Mutex::new(Vec::new()),
                looks: Mutex::new(std::collections::HashMap::new()),
            });
            let app = Router::new()
                .route("/api/v1/sessions", post(record_start))
                .route("/api/v1/workspaces", get(list_workspaces))
                .route("/api/v1/sessions/{session_id}", get(record_look))
                .route("/api/v1/sessions/{session_id}/prompt", post(record_prompt))
                .route("/api/v1/sessions/{session_id}/wait", post(record_wait))
                .route(
                    "/api/v1/sessions/{session_id}/interrupt-turn",
                    post(record_interrupt),
                )
                .route(
                    "/api/v1/sessions/{session_id}/suspend",
                    post(record_suspend),
                )
                .route(
                    "/api/v1/sessions/{session_id}/destroy",
                    post(record_destroy),
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

        /// Script what looking `session_id` up answers.
        fn script(&self, session_id: &str, looks: &[Look]) {
            self.looks
                .lock()
                .unwrap()
                .insert(session_id.to_owned(), looks.iter().cloned().collect());
        }

        fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }

        /// The lifecycle calls alone, without the lookups between them.
        fn lifecycle_calls(&self) -> Vec<String> {
            self.calls()
                .into_iter()
                .filter(|call| !call.starts_with("look "))
                .collect()
        }
    }

    /// The session shape the API answers, which the fake has to honor in full:
    /// it is the daemon's contract, not something a test gets to simplify.
    fn session_json(
        session_id: &str,
        lifecycle: &str,
        state: &str,
        chat_phase: &str,
        error: Option<&str>,
    ) -> Value {
        json!({
            "id": session_id,
            "workspace_id": "default",
            "title": "adapter session",
            "harness_kind": "codex",
            "profile_id": "codex-work",
            "target_id": "localhost",
            "bundle_id": "",
            "state": state,
            "lifecycle": lifecycle,
            "chat_phase": chat_phase,
            "is_idle": chat_phase == "idle",
            "has_error": error.is_some(),
            "error": error,
            "created_at": "now",
            "updated_at": "now"
        })
    }

    async fn record_look(
        State(daemon): State<Arc<FakeDaemon>>,
        Path(session_id): Path<String>,
    ) -> axum::response::Response {
        use axum::response::IntoResponse;
        daemon
            .calls
            .lock()
            .unwrap()
            .push(format!("look {session_id}"));
        let look = {
            let mut looks = daemon.looks.lock().unwrap();
            let script = looks.entry(session_id.clone()).or_default();
            match script.len() {
                0 => Look::Missing,
                1 => script[0].clone(),
                _ => script.pop_front().unwrap(),
            }
        };
        match look {
            Look::Missing => (
                axum::http::StatusCode::NOT_FOUND,
                version(),
                Json(json!({"error": "unknown session"})),
            )
                .into_response(),
            Look::Session {
                lifecycle,
                state,
                chat_phase,
                error,
            } => (
                version(),
                Json(session_json(
                    &session_id,
                    lifecycle,
                    state,
                    chat_phase,
                    error,
                )),
            )
                .into_response(),
        }
    }

    async fn record_lifecycle(
        daemon: &FakeDaemon,
        operation: &str,
        session_id: &str,
        body: &Value,
    ) -> ([(&'static str, &'static str); 1], axum::http::StatusCode) {
        daemon
            .calls
            .lock()
            .unwrap()
            .push(format!("{operation} {session_id} {body}"));
        let status = if daemon.turn.refuses.contains(&session_id) {
            axum::http::StatusCode::CONFLICT
        } else {
            axum::http::StatusCode::ACCEPTED
        };
        (version(), status)
    }

    async fn record_suspend(
        State(daemon): State<Arc<FakeDaemon>>,
        Path(session_id): Path<String>,
        Json(body): Json<Value>,
    ) -> ([(&'static str, &'static str); 1], axum::http::StatusCode) {
        record_lifecycle(&daemon, "suspend", &session_id, &body).await
    }

    async fn record_destroy(
        State(daemon): State<Arc<FakeDaemon>>,
        Path(session_id): Path<String>,
        Json(body): Json<Value>,
    ) -> ([(&'static str, &'static str); 1], axum::http::StatusCode) {
        record_lifecycle(&daemon, "destroy", &session_id, &body).await
    }

    async fn record_start(
        State(daemon): State<Arc<FakeDaemon>>,
        Json(body): Json<Value>,
    ) -> ([(&'static str, &'static str); 1], Json<Value>) {
        tokio::time::sleep(daemon.turn.start_delay).await;
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
    ) -> axum::response::Response {
        use axum::response::IntoResponse;
        daemon.prompt.lock().unwrap().push((session_id, body));
        if daemon.turn.prompt_withdrawn_on_interrupt {
            daemon.interrupted.notified().await;
            return (
                axum::http::StatusCode::CONFLICT,
                version(),
                Json(json!({"error": "this prompt was withdrawn: its turn was interrupted before the session could take it"})),
            )
                .into_response();
        }
        tokio::time::sleep(daemon.turn.prompt_delay).await;
        (version(), Json(json!({"turn_id": 7}))).into_response()
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
        // A wait answers with the session's public view as well as the outcome.
        let session = session_json(&session_id, "live", "running", "idle", None);
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
        daemon
            .calls
            .lock()
            .unwrap()
            .push(format!("interrupt {session_id}"));
        let refused = {
            let mut interrupts = daemon.interrupts.lock().unwrap();
            interrupts.push(session_id);
            interrupts.len() <= daemon.turn.interrupts_refused
        };
        if refused {
            return (version(), axum::http::StatusCode::CONFLICT);
        }
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

    /// Wait until the fake has the prompt, which it may still be holding.
    async fn until_prompted(daemon: &FakeDaemon) {
        tokio::time::timeout(Duration::from_secs(10), async {
            while daemon.prompt.lock().unwrap().is_empty() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the prompt reaches the daemon");
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
    async fn a_cancel_sent_while_the_prompt_is_held_withdraws_it_and_answers_cancelled() {
        // R2-1: a new session's prompt is held until its worker attaches. A
        // cancel in that window withdraws the prompt, so no turn ever runs.
        let (client, daemon) = FakeDaemon::start(FakeTurn {
            prompt_withdrawn_on_interrupt: true,
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
        until_prompted(&daemon).await;

        consumer.send(json!({"jsonrpc": "2.0", "method": "session/cancel", "params": {"sessionId": "session-1"}}));
        let answer = consumer.response(3).await;

        assert_eq!(answer["result"]["stopReason"], "cancelled", "{answer}");
        assert!(
            daemon.wait.lock().unwrap().is_empty(),
            "a withdrawn prompt has no turn to wait for"
        );
    }

    #[tokio::test]
    async fn a_cancel_that_arrives_before_its_turn_exists_is_retried_until_it_stops_the_turn() {
        // R2-1: the cancel sent during the hold, and the one sent as the prompt
        // became a turn, both found no turn to stop. The turn then ran to the
        // end, its reply was stored, and the consumer was told `cancelled`.
        let (client, daemon) = FakeDaemon::start(FakeTurn {
            runs_until_interrupted: true,
            prompt_delay: Duration::from_millis(300),
            interrupts_refused: 2,
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
        until_prompted(&daemon).await;

        consumer.send(json!({"jsonrpc": "2.0", "method": "session/cancel", "params": {"sessionId": "session-1"}}));
        let answer = consumer.response(3).await;

        assert_eq!(answer["result"]["stopReason"], "cancelled", "{answer}");
        assert!(
            daemon.interrupts.lock().unwrap().len() > 2,
            "the interrupt is sent again until the daemon takes one"
        );
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
            workspace: crate::WorkspaceName::default(),
            on_exit: ExitPolicy::Keep,
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

    /// An adapter with an exit policy that already knows its sessions, and
    /// deadlines short enough for a test to run out of.
    fn adapter_with(client: Arc<ApiClient>, on_exit: ExitPolicy, sessions: &[&str]) -> Adapter {
        let mut adapter = Adapter::new(
            AcpArgs {
                on_exit,
                ..AcpArgs::default()
            },
            None,
            Some(client),
        );
        adapter.timing = ExitTiming {
            poll: Duration::from_millis(5),
            settle: Duration::from_millis(300),
            finish: Duration::from_millis(300),
        };
        adapter
            .sessions
            .lock()
            .unwrap()
            .extend(sessions.iter().map(|id| (*id).to_owned()));
        adapter
    }

    /// Leave as a consumer that is gone: stop its turns, then apply the policy.
    async fn exit(adapter: &Adapter) -> Result<()> {
        adapter.stop_active_turns().await;
        adapter.apply_exit_policy().await
    }

    fn working(adapter: &Adapter, session_id: &str) {
        adapter.active.lock().unwrap().insert(session_id.to_owned());
    }

    #[tokio::test]
    async fn keeping_leaves_every_session_as_it_was() {
        let (client, daemon) = FakeDaemon::start(FakeTurn::default()).await;
        let adapter = adapter_with(client, ExitPolicy::Keep, &["session-1"]);
        working(&adapter, "session-1");

        exit(&adapter).await.expect("keeping cannot fail");

        assert_eq!(
            daemon.calls(),
            ["interrupt session-1"],
            "the turn stops, and nothing else happens to the session"
        );
    }

    #[tokio::test]
    async fn suspending_stops_the_turn_then_waits_for_the_suspension_to_finish() {
        let (client, daemon) = FakeDaemon::start(FakeTurn::default()).await;
        // The daemon answers a suspension when it admits it, so the session is
        // still live on the first look after, and suspended only later.
        daemon.script("session-1", &[IDLE, IDLE, SUSPENDING, SUSPENDED]);
        let adapter = adapter_with(client, ExitPolicy::Suspend, &["session-1"]);
        working(&adapter, "session-1");

        exit(&adapter).await.expect("the session suspends");

        assert_eq!(
            daemon.calls(),
            [
                "interrupt session-1",
                "look session-1",
                r#"suspend session-1 {"acknowledge_active_subagents":true,"acknowledge_unpublished_work":false}"#,
                "look session-1",
                "look session-1",
                "look session-1",
            ]
        );
    }

    #[tokio::test]
    async fn destroying_waits_for_the_turn_to_stop_and_then_for_the_session_to_go() {
        let (client, daemon) = FakeDaemon::start(FakeTurn::default()).await;
        daemon.script(
            "session-1",
            &[WORKING, WORKING, IDLE, IDLE, IDLE, Look::Missing],
        );
        let adapter = adapter_with(client, ExitPolicy::Destroy, &["session-1"]);
        working(&adapter, "session-1");

        exit(&adapter).await.expect("the session is destroyed");

        assert_eq!(
            daemon.calls(),
            [
                "interrupt session-1",
                "look session-1",
                "look session-1",
                "look session-1",
                // The look that confirms the session still exists.
                "look session-1",
                r#"destroy session-1 {"delete_branch":false}"#,
                "look session-1",
                "look session-1",
            ],
            "destruction waits until the turn is no longer running, and keeps the branch"
        );
    }

    #[tokio::test]
    async fn a_turn_that_will_not_stop_is_not_destroyed() {
        let (client, daemon) = FakeDaemon::start(FakeTurn::default()).await;
        daemon.script("session-1", &[WORKING]);
        let adapter = adapter_with(client, ExitPolicy::Destroy, &["session-1"]);
        working(&adapter, "session-1");

        let error = exit(&adapter)
            .await
            .expect_err("a running turn is not destroyed");

        let message = format!("{error:#}");
        assert!(
            message.contains("session-1") && message.contains("was not destroyed"),
            "the consumer is told which session was left and why: {message}"
        );
        assert_eq!(daemon.lifecycle_calls(), ["interrupt session-1"]);
    }

    #[tokio::test]
    async fn a_failed_suspension_is_reported_rather_than_claimed() {
        let (client, daemon) = FakeDaemon::start(FakeTurn::default()).await;
        daemon.script(
            "session-1",
            &[
                IDLE,
                SUSPENDING,
                Look::Session {
                    lifecycle: "live",
                    state: "running",
                    chat_phase: "idle",
                    error: Some("the suspension did not finish: the checkpoint failed"),
                },
            ],
        );
        let adapter = adapter_with(client, ExitPolicy::Suspend, &["session-1"]);

        let error = exit(&adapter)
            .await
            .expect_err("a failed suspension is a failure");

        let message = format!("{error:#}");
        assert!(
            message.contains("the checkpoint failed") && message.contains("still running"),
            "the reason and the state the session was left in are both named: {message}"
        );
    }

    #[tokio::test]
    async fn a_suspension_the_daemon_refuses_is_reported() {
        let (client, daemon) = FakeDaemon::start(FakeTurn {
            refuses: &["session-1"],
            ..FakeTurn::default()
        })
        .await;
        daemon.script("session-1", &[IDLE]);
        let adapter = adapter_with(client, ExitPolicy::Suspend, &["session-1"]);

        let error = exit(&adapter)
            .await
            .expect_err("a refused suspension is a failure");

        assert!(
            format!("{error:#}").contains("suspend session session-1"),
            "{error:#}"
        );
    }

    #[tokio::test]
    async fn a_suspension_that_does_not_finish_in_time_is_not_reported_as_done() {
        let (client, daemon) = FakeDaemon::start(FakeTurn::default()).await;
        daemon.script("session-1", &[IDLE, SUSPENDING]);
        let adapter = adapter_with(client, ExitPolicy::Suspend, &["session-1"]);

        let error = exit(&adapter)
            .await
            .expect_err("an unconfirmed suspension is not a success");

        let message = format!("{error:#}");
        assert!(
            message.contains("within") && message.contains("last suspending"),
            "the consumer learns the outcome is unknown and where it was: {message}"
        );
    }

    #[tokio::test]
    async fn every_session_is_retired_even_when_one_fails() {
        let (client, daemon) = FakeDaemon::start(FakeTurn {
            refuses: &["session-2"],
            ..FakeTurn::default()
        })
        .await;
        daemon.script("session-1", &[IDLE, IDLE, Look::Missing]);
        daemon.script("session-2", &[IDLE]);
        let adapter = adapter_with(client, ExitPolicy::Destroy, &["session-1", "session-2"]);

        let error = exit(&adapter)
            .await
            .expect_err("one session could not be destroyed");

        let message = format!("{error:#}");
        assert!(message.contains("session-2"), "{message}");
        assert!(
            !message.contains("session-1"),
            "a session that was destroyed is not reported as failed: {message}"
        );
        let mut destroyed: Vec<String> = daemon
            .lifecycle_calls()
            .into_iter()
            .filter(|call| call.starts_with("destroy "))
            .collect();
        destroyed.sort();
        assert_eq!(
            destroyed,
            [
                r#"destroy session-1 {"delete_branch":false}"#,
                r#"destroy session-2 {"delete_branch":false}"#,
            ]
        );
    }

    #[tokio::test]
    async fn a_failed_session_is_not_reported_before_its_destruction_is_taken_up() {
        // The daemon admits the destruction before it touches the session, so
        // the looks right after still show the failure it was asked to remove.
        let (client, daemon) = FakeDaemon::start(FakeTurn::default()).await;
        let failed = Look::Session {
            lifecycle: "failed",
            state: "error",
            chat_phase: "idle",
            error: Some("the provision failed"),
        };
        daemon.script(
            "session-1",
            &[
                failed.clone(),
                failed.clone(),
                failed.clone(),
                failed,
                Look::Missing,
            ],
        );
        let adapter = adapter_with(client, ExitPolicy::Destroy, &["session-1"]);

        exit(&adapter)
            .await
            .expect("the failed session is destroyed");

        assert_eq!(
            daemon.lifecycle_calls(),
            [r#"destroy session-1 {"delete_branch":false}"#]
        );
    }

    #[tokio::test]
    async fn a_session_created_as_the_consumer_left_is_still_retired() {
        // The consumer asked for a session and left before the daemon answered.
        // The daemon still created it, so the policy must still reach it.
        let (client, daemon) = FakeDaemon::start(FakeTurn {
            start_delay: Duration::from_millis(100),
            ..FakeTurn::default()
        })
        .await;
        daemon.script("session-1", &[IDLE, IDLE, Look::Missing]);
        let adapter = Arc::new(adapter_with(client, ExitPolicy::Destroy, &[]));
        let creating = Arc::clone(&adapter).new_session(NewSessionRequest::new(
            std::path::PathBuf::from("/work/project"),
        ));
        assert!(
            tokio::time::timeout(Duration::from_millis(10), creating)
                .await
                .is_err(),
            "the consumer leaves while the creation is still in flight"
        );

        exit(&adapter).await.expect("the session is destroyed");

        assert_eq!(
            daemon.lifecycle_calls(),
            [r#"destroy session-1 {"delete_branch":false}"#]
        );
    }

    #[tokio::test]
    async fn a_turn_that_finishes_before_a_cancel_can_stop_it_reports_end_turn() {
        // R2-1: the adapter answered `cancelled` for a turn whose reply had
        // been streamed and stored. A consumer that believes it would discard
        // work that was in fact done. Here the turn finishes normally and every
        // interrupt fails, so nothing stopped it.
        let (client, daemon) = FakeDaemon::start(FakeTurn {
            outcome: "finished",
            final_message: Some("the work finished anyway"),
            interrupt_fails: true,
            ..FakeTurn::default()
        })
        .await;
        let adapter = adapter_owning("session-1", client);
        let mut sent: Vec<String> = Vec::new();
        let mut notify = |_session_id: &str, message: &str| -> Result<()> {
            sent.push(message.to_owned());
            Ok(())
        };

        adapter.cancel("session-1").await;
        let stop = adapter
            .turn("session-1", "hello", &mut notify)
            .await
            .expect("the turn runs");

        assert_eq!(stop, StopReason::EndTurn);
        assert_eq!(sent, ["the work finished anyway"]);
        assert!(
            daemon.interrupts.lock().unwrap().len() >= 2,
            "the daemon was asked to stop, and asked again once the prompt became a turn"
        );
    }

    #[tokio::test]
    async fn a_cancelled_turn_that_fails_is_still_answered_cancelled() {
        // The specification requires `Cancelled` for a turn the consumer
        // cancelled even when the work underneath fails as it stops.
        let (client, _daemon) = FakeDaemon::start(FakeTurn {
            outcome: "error",
            final_message: Some("the harness stopped mid-call"),
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
