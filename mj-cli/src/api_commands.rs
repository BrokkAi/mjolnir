//! The one-shot subcommands an orchestrating agent drives a session with.
//!
//! Each command is a thin call on [`ApiClient`]: it builds the request struct
//! the API exports, prints either the raw JSON response (`--json`) or a short
//! human-readable summary, and returns. Nothing here interprets a session
//! beyond formatting; the rules live behind the API.

use std::io::{IsTerminal, Read, Write};
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use clap::{Args, ValueEnum};
use mj_controller::server::api::{
    ExportKind, ExportRequest, RelayState, ResumeSessionRequest, StartSessionRequest, WaitOutcome,
    WaitRequest, WaitResponse,
};

use mj_client::daemon::WikiSessionStatus;
use mj_controller::sessionwiki::WikiContinuation;

use crate::api_client::{ApiClient, ExportResult};

#[derive(Debug, Args)]
pub(crate) struct EventsArgs {
    /// Restrict the stream to this session.
    #[arg(long)]
    session: Option<String>,
    /// Restrict the stream to this workspace ID.
    #[arg(long)]
    workspace_id: Option<String>,
    /// Replay events after this sequence; omit to follow new events only.
    #[arg(long)]
    after_seq: Option<u64>,
}

pub(crate) async fn events(args: EventsArgs, requested_workspace: Option<String>) -> Result<()> {
    use tokio::io::AsyncWriteExt;
    let workspace_id = match (args.workspace_id, requested_workspace) {
        (Some(id), _) => Some(id),
        (None, Some(name)) => Some(crate::resolve_store_workspace(Some(&name)).await?),
        (None, None) => None,
    };
    let client = ApiClient::connect().await?;
    let filter = mj_controller::database::ApiEventFilter {
        session_id: args.session,
        workspace_id,
    };
    let mut response = client.events(&filter, args.after_seq).await?;
    let mut decoder = crate::api_client::events::EventDecoder::default();
    let mut stdout = tokio::io::stdout();
    let mut last_seq = args.after_seq;
    loop {
        let chunk = tokio::select! {
            signal = tokio::signal::ctrl_c() => { signal?; return Ok(()); },
            chunk = response.chunk() => chunk.with_context(|| format!("event stream interrupted; resume with --after-seq {}", last_seq.unwrap_or(0)))?,
        };
        let Some(chunk) = chunk else {
            bail!(
                "event stream ended; resume with --after-seq {}",
                last_seq.unwrap_or(0)
            );
        };
        let events = decoder.push(&chunk).with_context(|| {
            format!(
                "decode event stream; resume with --after-seq {}",
                last_seq.unwrap_or(0)
            )
        })?;
        for event in events {
            let mut line = serde_json::to_vec(&event)?;
            line.push(b'\n');
            stdout.write_all(&line).await?;
            stdout.flush().await?;
            last_seq = Some(event.seq);
        }
    }
}

#[derive(Debug, Args)]
pub(crate) struct NewArgs {
    /// Profile the session runs its harness from.
    #[arg(long)]
    profile: String,
    /// Target template the session is provisioned on.
    #[arg(long)]
    target: String,
    /// Existing bundle to run. Omit it to bundle `--project-directory`.
    #[arg(long)]
    bundle: Option<String>,
    /// Directory to bundle and run the session against.
    #[arg(long)]
    project_directory: Option<PathBuf>,
    /// Workspace id to create the session in. The global `--workspace NAME`
    /// names the same workspace by name.
    #[arg(long)]
    workspace_id: Option<String>,
    #[arg(long)]
    title: Option<String>,
    /// Harness model to select before the first prompt.
    #[arg(long)]
    model: Option<String>,
    /// Harness reasoning effort to select before the first prompt.
    #[arg(long)]
    effort: Option<String>,
    /// The first prompt. `-` reads it from standard input.
    prompt: Option<String>,
    /// Read the first prompt from this file instead.
    #[arg(long)]
    prompt_file: Option<PathBuf>,
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
pub(crate) struct PromptArgs {
    #[arg(long)]
    session: String,
    /// The prompt text. `-` reads it from standard input.
    text: Option<String>,
    /// Read the prompt from this file instead.
    #[arg(long)]
    prompt_file: Option<PathBuf>,
    /// Block until the prompt's turn ends, and print its outcome.
    #[arg(long)]
    wait: bool,
    /// Seconds to wait for with `--wait`.
    #[arg(long)]
    timeout: Option<u64>,
    /// Return when the harness asks for structured input.
    #[arg(long)]
    return_on_input: bool,
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
pub(crate) struct WaitArgs {
    #[arg(long)]
    session: String,
    /// The turn to wait for, as `mj prompt` printed it. Omit it to wait until
    /// the session is idle with nothing queued.
    #[arg(long)]
    turn: Option<u64>,
    /// Seconds to wait before answering `timeout`.
    #[arg(long)]
    timeout: Option<u64>,
    /// Return when the harness asks for structured input.
    #[arg(long)]
    return_on_input: bool,
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
pub(crate) struct TranscriptArgs {
    #[arg(long)]
    session: String,
    /// Resume from the highest sequence already read.
    #[arg(long)]
    after_seq: Option<u64>,
    #[arg(long)]
    limit: Option<usize>,
    #[arg(long, value_parser = parse_transcript_role)]
    role: Option<mj_core::transcript::TranscriptRole>,
    #[arg(long)]
    json: bool,
}

fn parse_transcript_role(value: &str) -> Result<mj_core::transcript::TranscriptRole, String> {
    serde_json::from_value(serde_json::Value::String(value.into())).map_err(|e| e.to_string())
}

#[derive(Debug, Args)]
pub(crate) struct UsageArgs {
    #[arg(long)]
    session: String,
    #[arg(long)]
    after_seq: Option<u64>,
    #[arg(long)]
    limit: Option<usize>,
    #[arg(long)]
    json: bool,
}

pub(crate) async fn usage(args: UsageArgs) -> Result<()> {
    let page = ApiClient::connect()
        .await?
        .usage(&args.session, args.after_seq, args.limit)
        .await?;
    // Usage is structured even without --json: scope and coverage must travel
    // with counters so partial provider reports cannot look like full totals.
    print_json(&page)
}

#[derive(Debug, Args)]
pub(crate) struct PutFileArgs {
    #[arg(long)]
    session: String,
    /// Destination relative to the session workspace.
    #[arg(long)]
    path: String,
    /// Source file, or - for stdin.
    source: PathBuf,
    #[arg(long)]
    overwrite: bool,
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
pub(crate) struct ElicitationsArgs {
    #[arg(long)]
    session: String,
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
pub(crate) struct RespondArgs {
    #[arg(long)]
    session: String,
    #[arg(long)]
    elicitation: String,
    /// JSON response, or - for stdin.
    response: Option<String>,
    #[arg(long)]
    response_file: Option<PathBuf>,
}

pub(crate) async fn put_file(args: PutFileArgs) -> Result<()> {
    let bytes = if args.source == std::path::Path::new("-") {
        mj_checkpoint::archive::read_session_file_input(std::io::stdin().lock())?
    } else {
        mj_checkpoint::archive::read_session_file_input(
            std::fs::File::open(&args.source)
                .with_context(|| format!("open {}", args.source.display()))?,
        )?
    };
    let written = ApiClient::connect()
        .await?
        .put_file(&args.session, &args.path, bytes, args.overwrite)
        .await?;
    if args.json {
        print_json(&written)
    } else {
        println!(
            "wrote {} bytes to {}",
            written.bytes,
            written.path.display()
        );
        Ok(())
    }
}

pub(crate) async fn elicitations(args: ElicitationsArgs) -> Result<()> {
    print_json(
        &ApiClient::connect()
            .await?
            .elicitations(&args.session)
            .await?,
    )
}

pub(crate) async fn respond(args: RespondArgs) -> Result<()> {
    let text = read_prompt(args.response, args.response_file)?
        .context("provide a JSON response or --response-file")?;
    let response = serde_json::from_str(&text).context("parse elicitation response JSON")?;
    ApiClient::connect()
        .await?
        .respond_elicitation(&args.session, &args.elicitation, &response)
        .await?;
    println!("response accepted");
    Ok(())
}

#[derive(Debug, Args)]
pub(crate) struct DiffArgs {
    #[arg(long)]
    session: String,
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
pub(crate) struct ExportArgs {
    #[arg(long)]
    session: String,
    #[arg(long, value_enum, default_value_t = ExportKindArg::Patch)]
    kind: ExportKindArg,
    /// Branch to push, required by `--kind branch`.
    #[arg(long)]
    branch: Option<String>,
    /// File to read, relative to the directory the agent runs in, required by
    /// `--kind file`.
    #[arg(long)]
    path: Option<String>,
    /// Write the export here instead of standard output.
    #[arg(long)]
    out: Option<PathBuf>,
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub(crate) enum ExportKindArg {
    Patch,
    Branch,
    Bundle,
    /// One file from the session workspace, which the API serves on its own
    /// route rather than as an export kind.
    File,
}

#[derive(Debug, Args)]
pub(crate) struct SessionsArgs {
    /// Show one session, including how its last turn ended.
    #[arg(long)]
    session: Option<String>,
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
pub(crate) struct SessionArgs {
    #[arg(long)]
    session: String,
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
pub(crate) struct CloseArgs {
    #[arg(long)]
    session: Option<String>,
    /// Taken only so the command can explain itself. `mj close <id>` is a
    /// common mistake, and every session command in this CLI names its
    /// session with `--session`, so clap's "unexpected argument" would leave
    /// the person guessing which option it wanted.
    #[arg(value_name = "SESSION", hide = true)]
    misplaced_session: Option<String>,
    /// Destroy the session without a checkpoint: the target is torn down, the
    /// recovery archive removed, sub-agents destroyed first; irreversible.
    #[arg(long)]
    force: bool,
    /// With --force, also delete the session's managed branch. Without it the
    /// branch stays in the repository.
    #[arg(long, requires = "force")]
    delete_branch: bool,
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
#[command(group(
    clap::ArgGroup::new("resume-subject")
        .required(true)
        .args(["session", "wiki"])
))]
pub(crate) struct ResumeArgs {
    /// Stopped, lost, or failed session to resume. It keeps its identity,
    /// transcript, and work.
    #[arg(long)]
    session: Option<String>,
    /// SessionWiki id of a session to continue, whoever ran it: a Mjolnir
    /// session is resumed or restored, another tool's session is imported and
    /// then resumed.
    #[arg(long)]
    wiki: Option<String>,
    /// Profile to resume on. Defaults to the one the session last ran.
    #[arg(long)]
    profile: Option<String>,
    /// Target template to provision. Defaults to the session's own.
    #[arg(long)]
    target: Option<String>,
    /// Workspace to resume into. Defaults to the session's own.
    #[arg(long)]
    workspace_id: Option<String>,
    /// What to do with prompts queued when the session stopped.
    #[arg(long, value_enum)]
    queue: Option<ResumeQueueArg>,
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub(crate) enum ResumeQueueArg {
    Start,
    Discard,
}

impl From<ResumeQueueArg> for mj_core::state::ResumeQueueDisposition {
    fn from(queue: ResumeQueueArg) -> Self {
        match queue {
            ResumeQueueArg::Start => Self::Start,
            ResumeQueueArg::Discard => Self::Discard,
        }
    }
}

#[derive(Debug, Args)]
pub(crate) struct ApiInfoArgs {
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
pub(crate) struct WorkspacesListArgs {
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
pub(crate) struct WorkspaceCreateArgs {
    /// Workspace name, 1-64 characters. Naming one that already exists selects
    /// it instead of failing, so this is safe to run before every session.
    name: String,
    #[arg(long)]
    json: bool,
}

/// List the workspaces sessions can be created in.
pub(crate) async fn workspaces_list(args: WorkspacesListArgs) -> Result<()> {
    let list = ApiClient::connect().await?.workspaces().await?;
    if args.json {
        return print_json(&list);
    }
    let id_width = list
        .workspaces
        .iter()
        .map(|workspace| workspace.id.chars().count())
        .chain(std::iter::once(2))
        .max()
        .unwrap_or(2);
    println!("{:id_width$}  SESSIONS  NAME", "ID");
    for workspace in &list.workspaces {
        println!(
            "{:id_width$}  {:8}  {}",
            workspace.id, workspace.session_count, workspace.name
        );
    }
    Ok(())
}

/// Create a workspace, or select the one that already carries the name.
pub(crate) async fn workspaces_create(args: WorkspaceCreateArgs) -> Result<()> {
    let created = ApiClient::connect()
        .await?
        .create_workspace(args.name)
        .await?;
    if args.json {
        return print_json(&created);
    }
    println!(
        "workspace {}  {}",
        created.workspace.id, created.workspace.name
    );
    Ok(())
}

/// Create a session and, when a prompt was given, hand it over as the first
/// turn.
pub(crate) async fn new_session(args: NewArgs, requested_workspace: Option<String>) -> Result<()> {
    let prompt = read_prompt(args.prompt.clone(), args.prompt_file.clone())?;
    if args.bundle.is_none() && args.project_directory.is_none() {
        bail!("pass --bundle, --project-directory, or both");
    }
    let workspace_id = match (&args.workspace_id, requested_workspace.as_deref()) {
        (Some(workspace_id), _) => Some(workspace_id.clone()),
        (None, Some(name)) => Some(crate::resolve_store_workspace(Some(name)).await?),
        (None, None) => None,
    };
    let request = StartSessionRequest {
        mjolnir_subagents: None,
        create_managed_worktree: None,
        workspace_id,
        profile_id: args.profile.clone(),
        target_id: args.target.clone(),
        bundle_id: args.bundle.clone(),
        project_directory: args.project_directory.clone(),
        title: args.title.clone(),
        model: args.model.clone(),
        effort: args.effort.clone(),
        prompt,
    };
    let client = ApiClient::connect().await?;
    let response = client.start(&request).await?;
    if args.json {
        return print_json(&response);
    }
    println!("session {}", response.session_id);
    if let Some(turn_id) = response.turn_id {
        println!("turn {turn_id}");
    }
    Ok(())
}

/// Send a prompt, and with `--wait` block on the turn it became.
pub(crate) async fn prompt(args: PromptArgs) -> Result<()> {
    let text = read_prompt(args.text.clone(), args.prompt_file.clone())?
        .context("pass the prompt text, --prompt-file, or `-` to read standard input")?;
    let client = ApiClient::connect().await?;
    let accepted = client.prompt(&args.session, text).await?;
    if !args.wait {
        return match args.json {
            true => print_json(&accepted),
            false => {
                println!("turn {}", accepted.turn_id);
                Ok(())
            }
        };
    }
    let response = client
        .wait(
            &args.session,
            &WaitRequest {
                return_on_input: args.return_on_input,
                turn_id: Some(accepted.turn_id),
                timeout_secs: args.timeout,
            },
        )
        .await?;
    report_wait(&response, args.json)
}

pub(crate) async fn wait(args: WaitArgs) -> Result<()> {
    let client = ApiClient::connect().await?;
    let response = client
        .wait(
            &args.session,
            &WaitRequest {
                return_on_input: args.return_on_input,
                turn_id: args.turn,
                timeout_secs: args.timeout,
            },
        )
        .await?;
    report_wait(&response, args.json)
}

/// Print a wait result, and fail the process when the turn did not finish so a
/// shell script can branch on it.
fn report_wait(response: &WaitResponse, json: bool) -> Result<()> {
    if json {
        print_json(response)?;
    } else {
        for line in wait_report_lines(response) {
            println!("{line}");
        }
        if !response.pending_elicitations.is_empty() {
            print_json(&response.pending_elicitations)?;
        }
    }
    match response.outcome {
        WaitOutcome::Finished | WaitOutcome::InputRequired => Ok(()),
        outcome => bail!("the turn ended as {}", outcome_name(outcome)),
    }
}

/// What `mj wait` prints, one line per entry.
///
/// Separate from the printing so the content can be tested: a turn that failed
/// used to print the bare word "error" and leave the reason in the transcript,
/// where automation never saw it (#1020).
fn wait_report_lines(response: &WaitResponse) -> Vec<String> {
    let mut lines = Vec::new();
    let mut summary = outcome_name(response.outcome).to_owned();
    if let Some(stop_reason) = &response.stop_reason {
        summary.push_str(&format!(" ({stop_reason})"));
    }
    if let Some(turn_number) = response.turn_number {
        summary.push_str(&format!(" turn {turn_number}"));
    }
    if let Some(elapsed_ms) = response.elapsed_ms {
        summary.push_str(&format!(" in {:.1}s", elapsed_ms.max(0) as f64 / 1000.0));
    }
    lines.push(summary);
    if let Some(message) = &response.message {
        lines.push(message.clone());
    }
    // Why the turn ended, when the worker recorded a reason.
    if let Some(diagnostic) = &response.diagnostic
        && response.outcome != WaitOutcome::Finished
    {
        lines.push(diagnostic.message.clone());
    }
    if let Some(retry) = &response.capacity_retry {
        lines.push(format!(
            "a capacity retry is armed (attempt {}); do not send another prompt yet",
            retry.attempt
        ));
    }
    // A turn that is still running and a worker the daemon cannot see look
    // identical from a timeout alone, so name the relay when it is at fault.
    if response.outcome == WaitOutcome::Timeout
        && let Some(relay) = &response.relay
        && relay.state != RelayState::Connected
    {
        let mut line = format!(
            "the daemon's view of this session is {}",
            relay_state_name(relay.state)
        );
        if let Some(detail) = &relay.detail {
            line.push_str(&format!(": {detail}"));
        }
        lines.push(line);
    }
    if let Some(final_message) = &response.final_message {
        lines.push(String::new());
        lines.push(final_message.clone());
    }
    lines
}

fn relay_state_name(state: RelayState) -> &'static str {
    match state {
        RelayState::Connected => "connected",
        RelayState::Disconnected => "not attached",
        RelayState::Unreachable => "unreachable",
        RelayState::TargetMissing => "missing its target",
        RelayState::ProjectionIntegrity => "out of step with the worker's events",
    }
}

fn outcome_name(outcome: WaitOutcome) -> &'static str {
    match outcome {
        WaitOutcome::Finished => "finished",
        WaitOutcome::InputRequired => "input_required",
        WaitOutcome::Error => "error",
        WaitOutcome::Cancelled => "cancelled",
        WaitOutcome::QuotaLimit => "quota_limit",
        WaitOutcome::Timeout => "timeout",
        WaitOutcome::Stopped => "stopped",
    }
}

pub(crate) async fn transcript(args: TranscriptArgs) -> Result<()> {
    let client = ApiClient::connect().await?;
    let page = client
        .transcript(&args.session, args.after_seq, args.limit, args.role)
        .await?;
    if args.json {
        return print_json(&page);
    }
    for item in &page.items {
        println!("[{}] {}", item.seq, item.role);
        if !item.text.trim().is_empty() {
            println!("{}", item.text.trim_end());
        }
        println!();
    }
    println!(
        "next after seq {}; latest seq {}",
        page.next_after_seq, page.latest_seq
    );
    Ok(())
}

pub(crate) async fn diff(args: DiffArgs) -> Result<()> {
    let client = ApiClient::connect().await?;
    let diff = client.diff(&args.session).await?;
    match args.json {
        true => print_json(&serde_json::json!({ "diff": diff })),
        false => {
            print!("{diff}");
            std::io::stdout().flush().context("write the session diff")
        }
    }
}

pub(crate) async fn export(args: ExportArgs) -> Result<()> {
    let client = ApiClient::connect().await?;
    let kind = match args.kind {
        ExportKindArg::Patch => ExportKind::Patch,
        ExportKindArg::Branch => ExportKind::Branch,
        ExportKindArg::Bundle => ExportKind::Bundle,
        ExportKindArg::File => {
            let path = args
                .path
                .as_deref()
                .context("`--kind file` needs --path, relative to the agent's directory")?;
            let bytes = client.read_file(&args.session, path).await?;
            return write_bytes(&bytes, args.out.as_deref());
        }
    };
    let request = ExportRequest {
        kind,
        branch: args.branch.clone(),
    };
    match client.export(&args.session, &request).await? {
        ExportResult::Branch(pushed) => match args.json {
            true => print_json(&pushed),
            false => {
                println!("pushed {} to {}", pushed.branch, pushed.remote);
                Ok(())
            }
        },
        ExportResult::Bytes(bytes) => write_bytes(&bytes, args.out.as_deref()),
    }
}

pub(crate) async fn sessions(
    args: SessionsArgs,
    requested_workspace: Option<String>,
) -> Result<()> {
    let client = ApiClient::connect().await?;
    if let Some(session_id) = &args.session {
        // An id that names no Mjolnir session may still name a SessionWiki
        // row, which is what an agent has after a search.
        let Some(session) = client.session_if_known(session_id).await? else {
            return wiki_session(&client, session_id, args.json).await;
        };
        if args.json {
            return print_json(&session);
        }
        println!("{}  {}  {}", session.id, session.state, session.title);
        // Silence is reported, never acted on. A turn waiting on a long build
        // is quiet and healthy, so this says what is true and leaves the
        // decision — keep waiting, or `mj cancel-turn` — to the reader.
        if let Some(note) = session.activity_state.as_ref().and_then(|state| {
            mj_core::activity::silence_note(state, mj_core::clock::epoch_millis())
        }) {
            println!("running, {note}");
        }
        if let Some(error) = &session.error {
            println!("error: {error}");
        }
        if let Some(outcome) = &session.last_turn_outcome {
            println!("last turn {:?}", outcome.outcome);
        }
        return Ok(());
    }
    let workspace = match requested_workspace {
        Some(name) => Some(crate::resolve_store_workspace(Some(&name)).await?),
        None => None,
    };
    let list = client.sessions_in_workspace(workspace).await?;
    if args.json {
        return print_json(&list);
    }
    let id_width = list
        .sessions
        .iter()
        .map(|session| session.id.chars().count())
        .chain(std::iter::once(2))
        .max()
        .unwrap_or(2);
    let state_width = list
        .sessions
        .iter()
        .map(|session| session.state.chars().count())
        .chain(std::iter::once(5))
        .max()
        .unwrap_or(5);
    println!("{:id_width$}  {:state_width$}  TITLE", "ID", "STATE");
    for session in &list.sessions {
        println!(
            "{:id_width$}  {:state_width$}  {}",
            session.id, session.state, session.title
        );
    }
    Ok(())
}

/// Report one SessionWiki row for an id that names no Mjolnir session.
///
/// `mine` is a Mjolnir session the daemon still has a record of, `archived`
/// one whose record the archive job destroyed, and `native` another tool's
/// session. All three can be continued with `mj resume --wiki <id>`.
async fn wiki_session(client: &ApiClient, wiki_id: &str, json: bool) -> Result<()> {
    let info = client.wiki_session(wiki_id).await?.with_context(|| {
        format!("no session {wiki_id}: it names neither a Mjolnir session nor an indexed one")
    })?;
    if json {
        return print_json(&info);
    }
    let status = match info.status {
        WikiSessionStatus::Mine => "mine",
        WikiSessionStatus::Archived => "archived",
        WikiSessionStatus::Native => "native",
    };
    println!("{}  {status}  {}", info.wiki_id, info.title);
    println!("{} at {}", info.tool, info.path.display());
    if let Some(session_id) = &info.mjolnir_session_id {
        println!("Mjolnir session {session_id}");
    }
    println!("continue it with `mj resume --wiki {}`", info.wiki_id);
    Ok(())
}

/// Close a session, with or without a checkpoint.
///
/// A non-force close is accepted asynchronously, because checkpointing and
/// tearing down a target take minutes. Follow it with `mj wait --session <id>`,
/// which blocks while the close runs and reports why it failed if it does.
pub(crate) async fn close(args: CloseArgs) -> Result<()> {
    let session = close_session_id(&args)?;
    let client = ApiClient::connect().await?;
    client
        .close(session, args.force, args.delete_branch)
        .await?;
    match args.json {
        true => print_json(&serde_json::json!({
            "session_id": session,
            "accepted": true,
            "forced": args.force,
        })),
        false => {
            match args.force {
                true => println!("destroying {session}"),
                false => {
                    println!("closing {session}");
                    println!("watch it with `mj wait --session {session}`");
                }
            }
            Ok(())
        }
    }
}

/// Which session this close is for, or a message naming the option that says
/// so.
fn close_session_id(args: &CloseArgs) -> Result<&str> {
    match (args.session.as_deref(), args.misplaced_session.as_deref()) {
        (Some(session), None) => Ok(session),
        (None, Some(session)) => {
            bail!("name the session as an option: `mj close --session {session}`")
        }
        (Some(_), Some(_)) => bail!("name the session once, with --session"),
        (None, None) => bail!("name the session to close with --session <id>"),
    }
}

/// Resume a stopped session from its checkpoint.
///
/// The API answers as soon as the daemon admits the resume, because restoring
/// an archive onto a fresh target takes minutes. Follow it with
/// `mj wait --session <id>`, which blocks while the resume runs and reports why
/// it failed if it does.
pub(crate) async fn resume(args: ResumeArgs) -> Result<()> {
    if let Some(wiki_id) = args.wiki.clone() {
        return resume_wiki(args, wiki_id).await;
    }
    let session = args
        .session
        .clone()
        .context("name the session to resume with --session <id>")?;
    let client = ApiClient::connect().await?;
    let response = client
        .resume(
            &session,
            &ResumeSessionRequest {
                profile_id: args.profile.clone(),
                target_id: args.target.clone(),
                workspace_id: args.workspace_id.clone(),
                queue: args.queue.map(Into::into),
            },
        )
        .await?;
    if args.json {
        return print_json(&response);
    }
    println!(
        "resuming {} on profile {} and target {}",
        response.session_id, response.profile_id, response.target_id
    );
    println!("watch it with `mj wait --session {}`", response.session_id);
    Ok(())
}

/// Continue the session a SessionWiki id names, whoever ran it.
///
/// The branch is the daemon's: a Mjolnir session Mjolnir still has a record of
/// is resumed, one it has archived is restored from the indexed transcript,
/// and another tool's session is imported and then resumed. The output says
/// which of the three ran, so the caller can follow with `mj prompt`.
async fn resume_wiki(args: ResumeArgs, wiki_id: String) -> Result<()> {
    let client = ApiClient::connect().await?;
    let info = client
        .wiki_session(&wiki_id)
        .await?
        .with_context(|| format!("no indexed session {wiki_id}"))?;
    match mj_controller::sessionwiki::wiki_continuation(
        &info.wiki_id,
        &info.tool,
        &info.path,
        info.status == WikiSessionStatus::Mine,
    )? {
        WikiContinuation::Resume { session_id } => {
            let response = client
                .resume(
                    &session_id,
                    &ResumeSessionRequest {
                        profile_id: args.profile.clone(),
                        target_id: args.target.clone(),
                        workspace_id: args.workspace_id.clone(),
                        queue: args.queue.map(Into::into),
                    },
                )
                .await?;
            report_wiki_continuation(&args, "resume", &wiki_id, &response.session_id, || {
                format!("resumed {}", response.session_id)
            })
        }
        WikiContinuation::Restore { wiki_id } => {
            let profile_id = args
                .profile
                .clone()
                .or_else(|| info.profile_id.clone())
                .with_context(|| {
                    format!(
                        "the indexed session {wiki_id} records no profile; name one with --profile"
                    )
                })?;
            let target_id = args
                .target
                .clone()
                .or_else(|| info.target_template_id.clone())
                .with_context(|| {
                    format!(
                        "the indexed session {wiki_id} records no target; name one with --target"
                    )
                })?;
            let response = client
                .wiki_restore(
                    &wiki_id,
                    &mj_controller::server::api::WikiRestoreBody {
                        workspace_id: args.workspace_id.clone(),
                        profile_id,
                        target_id,
                        project_directory: None,
                        model: None,
                        effort: None,
                    },
                )
                .await?;
            report_wiki_continuation(&args, "restore", &wiki_id, &response.session_id, || {
                format!("restored {wiki_id} into {}", response.session_id)
            })
        }
        WikiContinuation::Import {
            harness,
            native_session_id,
        } => {
            let workspace_id = match args.workspace_id.clone() {
                Some(workspace_id) => workspace_id,
                None => crate::resolve_store_workspace(None).await?,
            };
            let indexed_at = info.path.clone();
            let imported = {
                let native_session_id = native_session_id.clone();
                let tool = info.tool.clone();
                tokio::task::spawn_blocking(move || {
                    crate::import::import_named_native_session(
                        harness,
                        native_session_id.clone(),
                        &workspace_id,
                    )
                    .with_context(|| {
                        format!(
                            "import the {tool} session {native_session_id}, which the index read from {}",
                            indexed_at.display()
                        )
                    })
                })
                .await
                .context("import task panicked")??
            };
            let session_id = imported.context("the import was cancelled")?;
            let response = client
                .resume(
                    &session_id,
                    &ResumeSessionRequest {
                        profile_id: args.profile.clone(),
                        target_id: args.target.clone(),
                        workspace_id: args.workspace_id.clone(),
                        queue: args.queue.map(Into::into),
                    },
                )
                .await?;
            report_wiki_continuation(&args, "import", &wiki_id, &response.session_id, || {
                format!(
                    "imported {} session {native_session_id} as {} and resumed it",
                    info.tool, response.session_id
                )
            })
        }
    }
}

/// One line saying which of the three cases ran, or the same three facts as
/// JSON. The `mj wait` hint is the same one a plain resume prints.
fn report_wiki_continuation(
    args: &ResumeArgs,
    action: &str,
    wiki_id: &str,
    session_id: &str,
    summary: impl FnOnce() -> String,
) -> Result<()> {
    if args.json {
        return print_json(&serde_json::json!({
            "action": action,
            "session_id": session_id,
            "wiki_id": wiki_id,
        }));
    }
    println!("{}", summary());
    println!("watch it with `mj wait --session {session_id}`");
    Ok(())
}

pub(crate) async fn cancel_turn(args: SessionArgs) -> Result<()> {
    let client = ApiClient::connect().await?;
    client.cancel_turn(&args.session).await?;
    match args.json {
        true => print_json(&serde_json::json!({ "session_id": args.session, "accepted": true })),
        false => {
            println!("cancelling the turn on {}", args.session);
            Ok(())
        }
    }
}

/// Print where the API is and which file holds its token, which is what a
/// caller driving it with curl needs.
pub(crate) async fn api_info(args: ApiInfoArgs) -> Result<()> {
    let client = ApiClient::connect().await?;
    let token_path = ApiClient::token_path();
    if args.json {
        return print_json(&serde_json::json!({
            "base_url": format!("{}/api/v1", client.base_url()),
            "token_path": token_path,
        }));
    }
    println!("base url   {}/api/v1", client.base_url());
    println!("token file {}", token_path.display());
    println!("version    Mjolnir API 1");
    Ok(())
}

/// Read prompt text from an argument, a file, or standard input.
fn read_prompt(text: Option<String>, file: Option<PathBuf>) -> Result<Option<String>> {
    match (text, file) {
        (Some(_), Some(_)) => bail!("pass the prompt text or --prompt-file, not both"),
        (Some(text), None) if text == "-" => read_stdin().map(Some),
        (Some(text), None) => Ok(Some(text)),
        (None, Some(file)) if file == std::path::Path::new("-") => read_stdin().map(Some),
        (None, Some(file)) => {
            let text = std::fs::read_to_string(&file)
                .with_context(|| format!("read prompt file {}", file.display()))?;
            Ok(Some(text))
        }
        (None, None) => Ok(None),
    }
}

fn read_stdin() -> Result<String> {
    let mut stdin = std::io::stdin().lock();
    if stdin.is_terminal() {
        bail!("`-` reads the prompt from standard input, but standard input is a terminal");
    }
    let mut text = String::new();
    stdin
        .read_to_string(&mut text)
        .context("read the prompt from standard input")?;
    Ok(text)
}

/// Write an export to a file, or to standard output when none was named.
fn write_bytes(bytes: &[u8], out: Option<&std::path::Path>) -> Result<()> {
    match out {
        Some(path) => {
            std::fs::write(path, bytes).with_context(|| format!("write {}", path.display()))?;
            println!("wrote {} bytes to {}", bytes.len(), path.display());
            Ok(())
        }
        None => {
            let mut stdout = std::io::stdout().lock();
            stdout.write_all(bytes).context("write the export")?;
            stdout.flush().context("write the export")
        }
    }
}

fn print_json<T: serde::Serialize>(value: &T) -> Result<()> {
    println!(
        "{}",
        serde_json::to_string_pretty(value).context("serialize the API response")?
    );
    Ok(())
}

#[derive(Debug, Args)]
pub(crate) struct ModelsArgs {
    #[arg(long)]
    profile: String,
    #[arg(long)]
    model: Option<String>,
    #[arg(long)]
    json: bool,
}
#[derive(Debug, Args)]
pub(crate) struct SetConfigArgs {
    #[arg(long)]
    session: String,
    #[arg(long)]
    key: String,
    #[arg(long)]
    value: String,
    #[arg(long)]
    json: bool,
}
pub(crate) async fn models(args: ModelsArgs) -> Result<()> {
    let choices = ApiClient::connect()
        .await?
        .models(&args.profile, args.model)
        .await?;
    if args.json {
        return print_json(&choices);
    }
    for model in choices.models {
        println!("{}  {}", model.value, model.name);
    }
    println!(
        "effort ({}): {}",
        choices.model.as_deref().unwrap_or("default"),
        choices
            .efforts
            .iter()
            .map(|c| c.value.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    );
    Ok(())
}
pub(crate) async fn set_config(args: SetConfigArgs) -> Result<()> {
    let session = ApiClient::connect()
        .await?
        .set_config(
            &args.session,
            &mj_controller::server::api::SetConfigRequest {
                key: args.key,
                value: args.value,
            },
        )
        .await?;
    if args.json {
        return print_json(&session);
    }
    for option in session.config_options {
        println!(
            "{} {}",
            option.key,
            option.current.as_deref().unwrap_or("unknown")
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Cli, Command};
    use clap::Parser as _;

    fn wait_response(outcome: &str, extra: serde_json::Value) -> WaitResponse {
        let mut body = serde_json::json!({
            "outcome": outcome,
            "session": {
                "id": "s1", "workspace_id": "w1", "title": "t",
                "harness_kind": "codex", "profile_id": "p", "target_id": "t",
                "bundle_id": "b", "state": "running", "lifecycle": "live",
                "chat_phase": "idle", "is_idle": false, "has_error": false,
                "created_at": "now", "updated_at": "now"
            }
        });
        let object = body.as_object_mut().expect("wait response object");
        for (key, value) in extra.as_object().expect("extra fields") {
            object.insert(key.clone(), value.clone());
        }
        serde_json::from_value(body).expect("wait response")
    }

    /// A turn the worker failed for going quiet has to say why, where a script
    /// waiting on it can see it. Before this the reason lived only in the
    /// transcript and `mj wait` printed the bare word "error" (#1020).
    #[test]
    fn a_failed_turn_reports_the_reason_the_worker_recorded() {
        let response = wait_response(
            "error",
            serde_json::json!({
                "stop_reason": "harness_inactive",
                "diagnostic": {
                    "message": "The Muse turn stopped responding: the tool call job_output-7 ran for about 241 minute(s).",
                    "code": "harness_inactive"
                }
            }),
        );
        let lines = wait_report_lines(&response);
        assert_eq!(lines[0], "error (harness_inactive)");
        assert!(
            lines.iter().any(|line| line.contains("job_output-7")),
            "the reason is printed: {lines:?}"
        );

        // A turn that finished normally is not annotated with a diagnostic it
        // may still carry from an earlier attempt.
        let finished = wait_response(
            "finished",
            serde_json::json!({
                "diagnostic": {"message": "an older failure"}
            }),
        );
        assert!(
            !wait_report_lines(&finished)
                .iter()
                .any(|line| line.contains("an older failure")),
            "a finished turn prints no failure reason"
        );
    }

    #[test]
    fn creating_a_session_parses_its_target_selection_and_first_prompt() {
        let cli = Cli::try_parse_from([
            "mj",
            "--workspace",
            "work",
            "new",
            "--profile",
            "codex",
            "--target",
            "local",
            "--project-directory",
            ".",
            "--model",
            "gpt-5",
            "--effort",
            "high",
            "add a README line",
        ])
        .unwrap();
        assert_eq!(cli.workspace.as_deref(), Some("work"));
        let Some(Command::New(args)) = cli.command else {
            panic!("expected the new subcommand");
        };
        assert_eq!(args.profile, "codex");
        assert_eq!(args.target, "local");
        assert_eq!(args.project_directory, Some(PathBuf::from(".")));
        assert_eq!(args.model.as_deref(), Some("gpt-5"));
        assert_eq!(args.effort.as_deref(), Some("high"));
        assert_eq!(args.prompt.as_deref(), Some("add a README line"));
        assert!(!args.json);

        // The idempotency key is gone; a command line that still passes it
        // must fail rather than be silently ignored.
        assert!(
            Cli::try_parse_from([
                "mj",
                "new",
                "--profile",
                "codex",
                "--target",
                "local",
                "--project-directory",
                ".",
                "--idempotency-key",
                "k",
                "add a README line",
            ])
            .is_err()
        );

        // The global workspace flag names a workspace; the session-scoped id
        // is its own flag, so the two cannot collide.
        let cli = Cli::try_parse_from([
            "mj",
            "new",
            "--profile",
            "codex",
            "--target",
            "local",
            "--bundle",
            "bundle-1",
            "--workspace-id",
            "workspace-7",
            "--json",
        ])
        .unwrap();
        let Some(Command::New(args)) = cli.command else {
            panic!("expected the new subcommand");
        };
        assert_eq!(args.workspace_id.as_deref(), Some("workspace-7"));
        assert_eq!(args.bundle.as_deref(), Some("bundle-1"));
        assert!(args.json);
    }

    #[test]
    fn the_session_driving_subcommands_parse_their_selectors() {
        let cli = Cli::try_parse_from([
            "mj",
            "prompt",
            "--session",
            "s1",
            "--wait",
            "--timeout",
            "30",
            "now add a test",
        ])
        .unwrap();
        let Some(Command::Prompt(args)) = cli.command else {
            panic!("expected the prompt subcommand");
        };
        assert_eq!(args.session, "s1");
        assert_eq!(args.text.as_deref(), Some("now add a test"));
        assert!(args.wait);
        assert_eq!(args.timeout, Some(30));

        let cli = Cli::try_parse_from(["mj", "wait", "--session", "s1", "--turn", "12"]).unwrap();
        let Some(Command::Wait(args)) = cli.command else {
            panic!("expected the wait subcommand");
        };
        assert_eq!(args.turn, Some(12));

        let cli = Cli::try_parse_from([
            "mj",
            "transcript",
            "--session",
            "s1",
            "--after-seq",
            "5",
            "--limit",
            "10",
            "--json",
        ])
        .unwrap();
        let Some(Command::Transcript(args)) = cli.command else {
            panic!("expected the transcript subcommand");
        };
        assert_eq!(
            (args.after_seq, args.limit, args.json),
            (Some(5), Some(10), true)
        );

        let cli = Cli::try_parse_from([
            "mj",
            "export",
            "--session",
            "s1",
            "--kind",
            "bundle",
            "--out",
            "work.bundle",
        ])
        .unwrap();
        let Some(Command::Export(args)) = cli.command else {
            panic!("expected the export subcommand");
        };
        assert_eq!(args.kind, ExportKindArg::Bundle);
        assert_eq!(args.out, Some(PathBuf::from("work.bundle")));

        let cli = Cli::try_parse_from([
            "mj",
            "export",
            "--session",
            "s1",
            "--kind",
            "file",
            "--path",
            "src/main.rs",
        ])
        .unwrap();
        let Some(Command::Export(args)) = cli.command else {
            panic!("expected the export subcommand");
        };
        assert_eq!(args.kind, ExportKindArg::File);
        assert_eq!(args.path.as_deref(), Some("src/main.rs"));

        // An export defaults to the patch, which is what a caller reviewing
        // the work asks for most.
        let cli = Cli::try_parse_from(["mj", "export", "--session", "s1"]).unwrap();
        let Some(Command::Export(args)) = cli.command else {
            panic!("expected the export subcommand");
        };
        assert_eq!(args.kind, ExportKindArg::Patch);

        let cli = Cli::try_parse_from(["mj", "close", "--session", "s1", "--force"]).unwrap();
        let Some(Command::Close(args)) = cli.command else {
            panic!("expected the close subcommand");
        };
        assert!(args.force);

        let cli = Cli::try_parse_from(["mj", "close", "--session", "s1"]).unwrap();
        let Some(Command::Close(args)) = cli.command else {
            panic!("expected the close subcommand");
        };
        assert!(!args.force);

        // Resume names the session and nothing else by default: the session's
        // own record supplies the profile and target.
        let cli = Cli::try_parse_from(["mj", "resume", "--session", "s1"]).unwrap();
        let Some(Command::Resume(args)) = cli.command else {
            panic!("expected the resume subcommand");
        };
        assert_eq!(args.session.as_deref(), Some("s1"));
        assert_eq!(args.profile, None);
        assert_eq!(args.target, None);
        assert_eq!(args.queue, None);

        let cli = Cli::try_parse_from([
            "mj",
            "resume",
            "--session",
            "s1",
            "--profile",
            "deepseek",
            "--target",
            "localhost",
            "--queue",
            "discard",
            "--json",
        ])
        .unwrap();
        let Some(Command::Resume(args)) = cli.command else {
            panic!("expected the resume subcommand");
        };
        assert_eq!(args.profile.as_deref(), Some("deepseek"));
        assert_eq!(args.target.as_deref(), Some("localhost"));
        assert_eq!(args.queue, Some(ResumeQueueArg::Discard));
        assert!(args.json);

        for (argv, matched) in [
            (vec!["mj", "diff", "--session", "s1"], "diff"),
            (vec!["mj", "resume", "--session", "s1"], "resume"),
            (vec!["mj", "sessions", "--json"], "sessions"),
            (vec!["mj", "close", "--session", "s1"], "close"),
            (vec!["mj", "cancel-turn", "--session", "s1"], "cancel-turn"),
            (vec!["mj", "api-info"], "api-info"),
        ] {
            let cli = Cli::try_parse_from(argv.clone()).unwrap_or_else(|error| {
                panic!("{matched} should parse: {error}");
            });
            assert_eq!(crate::command_name(cli.command.as_ref()), matched);
        }
    }

    /// `mj resume` names one subject: a Mjolnir session or a SessionWiki row.
    /// Both at once would leave the command guessing which to continue.
    #[test]
    fn resume_takes_a_session_or_a_wiki_id_but_not_both() {
        let cli = Cli::try_parse_from(["mj", "resume", "--wiki", "abc123"]).unwrap();
        let Some(Command::Resume(args)) = cli.command else {
            panic!("expected the resume subcommand");
        };
        assert_eq!(args.wiki.as_deref(), Some("abc123"));
        assert_eq!(args.session, None);

        assert!(
            Cli::try_parse_from(["mj", "resume", "--wiki", "abc123", "--session", "s1"]).is_err(),
            "--wiki and --session name different subjects"
        );
        assert!(
            Cli::try_parse_from(["mj", "resume"]).is_err(),
            "resume has to be told what to continue"
        );
    }

    /// `mj workspaces` keeps opening the manager, and the two subcommands are
    /// the non-interactive form a script uses to get a workspace before its
    /// first session (#1080).
    #[test]
    fn workspaces_keeps_its_interactive_form_and_gains_list_and_create() {
        let cli = Cli::try_parse_from(["mj", "workspaces"]).unwrap();
        let Some(Command::Workspaces(args)) = cli.command else {
            panic!("expected the workspaces subcommand");
        };
        assert!(args.command.is_none(), "the bare form opens the manager");

        let cli = Cli::try_parse_from(["mj", "workspaces", "list", "--json"]).unwrap();
        let Some(Command::Workspaces(args)) = cli.command else {
            panic!("expected the workspaces subcommand");
        };
        let Some(crate::WorkspacesCommand::List(list)) = args.command else {
            panic!("expected list");
        };
        assert!(list.json);

        let cli = Cli::try_parse_from(["mj", "workspaces", "create", "Release work"]).unwrap();
        let Some(Command::Workspaces(args)) = cli.command else {
            panic!("expected the workspaces subcommand");
        };
        let Some(crate::WorkspacesCommand::Create(create)) = args.command else {
            panic!("expected create");
        };
        assert_eq!(create.name, "Release work");
        assert!(!create.json);

        // The name is required: an empty create would otherwise reach the API.
        assert!(Cli::try_parse_from(["mj", "workspaces", "create"]).is_err());
    }

    #[test]
    fn close_names_the_option_that_takes_a_session_id() {
        let parsed = Cli::try_parse_from(["mj", "close", "s1"]).expect("the id is accepted");
        let Command::Close(args) = parsed.command.expect("close is a command") else {
            panic!("close parsed as another command");
        };
        let error = close_session_id(&args).unwrap_err();
        assert!(
            format!("{error:#}").contains("--session s1"),
            "the error has to say which option to use: {error:#}"
        );

        let parsed =
            Cli::try_parse_from(["mj", "close", "--session", "s1"]).expect("the option parses");
        let Command::Close(args) = parsed.command.expect("close is a command") else {
            panic!("close parsed as another command");
        };
        assert_eq!(close_session_id(&args).unwrap(), "s1");
    }

    #[test]
    fn a_prompt_comes_from_exactly_one_source() {
        assert_eq!(
            read_prompt(Some("inline".to_owned()), None)
                .unwrap()
                .as_deref(),
            Some("inline")
        );
        let error =
            read_prompt(Some("inline".to_owned()), Some(PathBuf::from("prompt.txt"))).unwrap_err();
        assert!(
            format!("{error:#}").contains("not both"),
            "unexpected error: {error:#}"
        );
        assert_eq!(read_prompt(None, None).unwrap(), None);
    }
}
