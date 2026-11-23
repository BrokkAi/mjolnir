//! The one-shot subcommands an orchestrating agent drives a session with.
//!
//! Each command is a thin call on [`ApiClient`]: it builds the request struct
//! the API exports, prints either the raw JSON response (`--json`) or a short
//! human-readable summary, and returns. Nothing here interprets a session
//! beyond formatting; the rules live behind the API.

use std::io::{IsTerminal, Read, Write};
use std::path::PathBuf;

use anyhow::{Context, Result, bail, ensure};
use clap::{ArgGroup, Args, ValueEnum};
use mj_daemon::server::api::{
    ApiSession, ExportKind, ExportRequest, MailboxEventRequest, PurgeUntrackedRequest,
    PurgeUntrackedResponse, PurgeUntrackedStatus, RelayState, ResumeSessionRequest,
    StartSessionRequest, WaitOutcome, WaitRequest, WaitResponse,
};

use mj_client::daemon::WikiSessionStatus;
use mj_controller::sessionwiki::WikiContinuation;
use mj_core::transcript::sanitize_terminal_text;
use mj_daemon::server::ViewerLifecycleCategory;

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
    #[command(flatten)]
    pub(crate) workspace: crate::WorkspaceName,
}

pub(crate) async fn events(args: EventsArgs, requested_workspace: Option<String>) -> Result<()> {
    use tokio::io::AsyncWriteExt;
    let workspace_id = match (args.workspace_id, requested_workspace) {
        (Some(id), _) => Some(id),
        (None, Some(name)) => Some(crate::resolve_store_workspace(Some(&name)).await?),
        (None, None) => None,
    };
    let mut client = ApiClient::connect().await?;
    let filter = mj_controller::database::ApiEventFilter {
        session_id: args.session,
        workspace_id,
    };
    let mut last_seq = args.after_seq;
    let mut stdout = tokio::io::stdout();
    loop {
        let mut response = match client.events(&filter, last_seq).await {
            // Reached the old daemon again while it exits: ask once more.
            Err(error) if crate::api_client::is_daemon_handoff(&error) => {
                client = follow_handoff(events_continuation(&filter, last_seq)).await?;
                continue;
            }
            response => response?,
        };
        let mut decoder = crate::api_client::events::EventDecoder::default();
        let resume = loop {
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
            if let Some(cursor) = decoder.handoff() {
                break cursor;
            }
        };
        // The daemon is being replaced. It named the cursor its stream
        // reached, so the next daemon's stream continues from there and no
        // event is lost or printed twice.
        eprintln!("The Mjolnir daemon is being replaced by an upgrade; following the new daemon.");
        last_seq = Some(resume);
        client = follow_handoff(events_continuation(&filter, last_seq)).await?;
    }
}

#[derive(Debug, Args)]
pub(crate) struct NewArgs {
    /// Profile the session runs its harness from. Omit it to use the saved
    /// default saved by an interactive launch. This command does not change it.
    #[arg(long)]
    profile: Option<String>,
    /// Target template the session is provisioned on. Omit it to use the
    /// saved default, as for `--profile`, without changing it.
    #[arg(long)]
    target: Option<String>,
    /// Existing bundle to run. Bare targets use `--project-directory` instead.
    #[arg(long)]
    bundle: Option<String>,
    /// Existing directory on the bare target, or a checkout on the controller
    /// host for discovering an isolated project.
    #[arg(long)]
    project_directory: Option<PathBuf>,
    /// Start the workspace checked out at this full commit ID in the bundle's primary repository.
    #[arg(long, value_name = "SHA")]
    at: Option<String>,
    /// With --at, give the workspace the full history up to that commit and
    /// --base, and nothing later: no later commits, branches, tags, or origin
    /// remote. --base must then be a full commit ID. The session cannot be
    /// resumed or moved.
    #[arg(long, requires = "at")]
    no_later_history: bool,
    /// With --at, the new branch to create there; otherwise the existing branch to check out.
    #[arg(long)]
    branch: Option<String>,
    /// Record this Git revision as the diff base, when it should not be --at.
    #[arg(long, value_name = "REV")]
    base: Option<String>,
    /// Workspace id to create the session in. `--workspace NAME`
    /// names the same workspace by name.
    #[arg(long)]
    workspace_id: Option<String>,
    /// Title shown in session lists. Defaults to the project and profile.
    #[arg(long)]
    title: Option<String>,
    /// Load balance across usable profiles for this model and optional effort.
    #[arg(long, conflicts_with = "profile")]
    model: Option<String>,
    /// Harness reasoning effort to select before the first prompt.
    #[arg(long)]
    effort: Option<String>,
    /// Delegation policy: native, mjolnir, or none (no delegation tools).
    /// Defaults to mjolnir, with a notice when unavailable and native or none is used.
    #[arg(long)]
    subagents: Option<String>,
    /// Oracle availability for this session.
    #[arg(long, value_parser = ["enabled", "disabled"])]
    oracle: Option<String>,
    /// Supervisor sub-agent availability for this session (requires Mjolnir delegation).
    #[arg(long, value_parser = ["enabled", "disabled"])]
    supervisors: Option<String>,
    /// Override the configured model for straightforward Mjolnir sub-agents.
    #[arg(long)]
    subagent_straightforward: Option<String>,
    /// Override straightforward sub-agent effort (requires --subagent-straightforward).
    #[arg(long)]
    subagent_straightforward_effort: Option<String>,
    /// Override the configured model for subtle Mjolnir sub-agents.
    #[arg(long)]
    subagent_subtle: Option<String>,
    /// Override subtle sub-agent effort (requires --subagent-subtle).
    #[arg(long)]
    subagent_subtle_effort: Option<String>,
    /// Oracle model for this session, independent of delegation.
    #[arg(long)]
    subagent_oracle: Option<String>,
    /// Oracle effort; requires --subagent-oracle.
    #[arg(long, requires = "subagent_oracle")]
    subagent_oracle_effort: Option<String>,
    /// Retired; use the four per-type sub-agent flags.
    #[arg(long, hide = true)]
    subagent_model: Option<String>,
    /// Retired; use the four per-type sub-agent flags.
    #[arg(long, hide = true)]
    subagent_effort: Option<String>,
    /// Reviewer model and optional effort; final colon suffix is effort only when a known effort (minimal, low, medium, high, xhigh, max); trailing colon escapes it. Repeat up to three times.
    #[arg(long = "reviewer", value_name = "MODEL[:EFFORT]")]
    reviewers: Vec<String>,
    /// Whether this session is reviewed before push (`push`) or not (`off`).
    /// Omitted follows `[review]`: review runs when reviewers are configured.
    #[arg(long, value_name = "off|push")]
    review: Option<String>,
    /// Start this session with no project memory: nothing from the
    /// workspace's project memory is loaded into the harness, and the harness
    /// is offered no memory directory to write to. Sub-agents of the session
    /// get none either. Without this flag the session loads the workspace's
    /// project memory.
    #[arg(long)]
    no_project_memory: bool,
    /// Exclude this session from GitHub watcher delivery targets.
    /// Sub-agents inherit the choice.
    #[arg(long)]
    no_github_events: bool,
    /// Withhold GitHub credentials from this session and its children.
    #[arg(long)]
    no_github_credentials: bool,
    /// Ignore notifications for this session.
    #[arg(long)]
    ignore_notifications: bool,
    /// Container CPU limit. Omitted takes the target's default size: the
    /// size last chosen for that host, else 4 CPUs, capped at the host's.
    #[arg(long, value_parser = clap::value_parser!(u64).range(1..))]
    cpus: Option<u64>,
    /// Container memory limit in GiB. Omitted takes the target's default
    /// size: the size last chosen for that host, else 8 GiB, capped at the
    /// host's.
    #[arg(long, value_name = "GIB", value_parser = clap::value_parser!(u64).range(1..))]
    memory_gib: Option<u64>,
    /// The first prompt. `-` reads it from standard input.
    prompt: Option<String>,
    /// Read the first prompt from this file instead.
    #[arg(long)]
    prompt_file: Option<PathBuf>,
    /// Print the response as JSON instead of text.
    #[arg(long)]
    json: bool,
    #[command(flatten)]
    pub(crate) workspace: crate::WorkspaceName,
}

#[derive(Debug, Args)]
pub(crate) struct PromptArgs {
    /// Stable producer-chosen identity; repeat it after an ambiguous reply.
    #[arg(long)]
    command_id: Option<String>,
    /// Session id, as `mj sessions` lists it.
    #[arg(long)]
    session: String,
    /// The prompt text. `-` reads it from standard input.
    text: Option<String>,
    /// Read the prompt from this file instead.
    #[arg(long)]
    prompt_file: Option<PathBuf>,
    /// Retired; `mj prompt` always waits.
    #[arg(long, hide = true)]
    #[allow(
        dead_code,
        reason = "accepted so scripts written for the retired flag keep working"
    )]
    wait: bool,
    /// Seconds to wait for the turn to end before answering `timeout`.
    #[arg(long)]
    timeout: Option<u64>,
    /// Return when the harness asks for structured input.
    #[arg(long)]
    return_on_input: bool,
    /// Print the turn's outcome as JSON instead of text.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
pub(crate) struct EventArgs {
    /// Session id, as `mj sessions` lists it.
    #[arg(long)]
    session: String,
    /// Producer idempotency key. Omit it to generate a unique key.
    #[arg(long)]
    key: Option<String>,
    /// Start a turn if the session is idle when the worker receives the event.
    #[arg(long)]
    wake: bool,
    /// Event text shown to the session as untrusted external content.
    text: String,
}

#[derive(Debug, Args)]
pub(crate) struct MessageArgs {
    /// Full session ID or unique case-insensitive hexadecimal prefix of at least 8 characters.
    #[arg(long)]
    session: String,
    /// Stable producer-chosen identity; repeat it after an ambiguous reply.
    #[arg(long)]
    command_id: Option<String>,
    /// Message text. `-` reads it from standard input.
    text: Option<String>,
    /// Read the message from this file instead.
    #[arg(long)]
    prompt_file: Option<PathBuf>,
    /// Print the response as JSON instead of text.
    #[arg(long)]
    json: bool,
}

/// The session this process runs inside, which Mjolnir names in
/// `MJ_SESSION_ID` for every harness session; `None` in the user's own shell.
pub(crate) fn calling_session() -> Result<Option<String>> {
    std::env::var_os("MJ_SESSION_ID")
        .map(|value| {
            value
                .into_string()
                .map_err(|_| anyhow::anyhow!("MJ_SESSION_ID is not valid Unicode"))
        })
        .transpose()
}

/// Send a message and return. From the user's own shell the daemon submits
/// it as the user's prompt, steered into a running turn or queued; from
/// inside a session it is that session's labelled message to another
/// session or to one of its own sub-agents.
pub(crate) async fn message(args: MessageArgs) -> Result<()> {
    let sender_session_id = calling_session()?;
    let text = read_prompt(args.text, args.prompt_file)?
        .context("pass the message text, --prompt-file, or `-` to read standard input")?;
    let from_the_user = sender_session_id.is_none();
    let request = mj_daemon::server::api::SessionMessageRequest {
        request_id: match args.command_id {
            Some(id) => id,
            None => mj_core::state::new_session_id()?,
        },
        text,
        sender_session_id,
    };
    let response = ApiClient::connect()
        .await?
        .message(&args.session, &request)
        .await?;
    if args.json {
        return print_json(&response);
    }
    match response.turn_id {
        Some(turn_id) if from_the_user => println!("turn {turn_id}"),
        Some(turn_id) => println!(
            "message queued for session {} as turn {}",
            response.session_id, turn_id
        ),
        None => println!("message queued for session {}", response.session_id),
    }
    Ok(())
}

pub(crate) async fn event(args: EventArgs) -> Result<()> {
    let key = match args.key {
        Some(key) => key,
        None => format!("cli:{}", mj_core::state::new_session_id()?),
    };
    let client = ApiClient::connect().await?;
    let response = client
        .enqueue_event(
            &args.session,
            &MailboxEventRequest {
                key: key.clone(),
                text: args.text,
                wake: args.wake,
            },
        )
        .await?;
    println!(
        "{} event {} for session {}",
        if response.inserted {
            "queued"
        } else {
            "already queued"
        },
        response.key,
        args.session
    );
    Ok(())
}

#[derive(Debug, Args)]
pub(crate) struct WaitArgs {
    /// Session id, as `mj sessions` lists it.
    #[arg(long)]
    session: String,
    /// Wait until this turn, as `mj new` or `mj message` printed it, or a
    /// later one, has ended, and say how. The wait does not follow what the
    /// session does after the turn (a turn the agent starts on its own,
    /// background work, an automatic continuation); the answer names that as
    /// `next`. It describes the latest turn that ended (a session keeps only
    /// that turn's outcome and reply), and `--json` reports it as `turn_id`
    /// beside `requested_turn_id`. Omit it to wait until the session settles:
    /// idle, with nothing queued, including after turns the agent starts on
    /// its own.
    #[arg(long)]
    turn: Option<u64>,
    /// Seconds to wait before answering `timeout`.
    #[arg(long)]
    timeout: Option<u64>,
    /// Return when the harness asks for structured input.
    #[arg(long)]
    return_on_input: bool,
    /// Print the response as JSON instead of text.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
pub(crate) struct TranscriptArgs {
    /// Session id, as `mj sessions` lists it.
    #[arg(long)]
    session: String,
    /// Start after this sequence; use next_after_seq to resume after a page.
    #[arg(long)]
    after_seq: Option<u64>,
    /// Target number of matching entries in a page; sequence ties stay together.
    #[arg(long)]
    limit: Option<usize>,
    /// Return only items with this role; repeat to select several roles.
    #[arg(long, value_parser = parse_transcript_role)]
    role: Vec<mj_core::transcript::TranscriptRole>,
    /// Return finished agent messages only, with a cursor held before open messages.
    #[arg(long)]
    finished_only: bool,
    /// Print the response as JSON instead of text.
    #[arg(long)]
    json: bool,
}

fn parse_transcript_role(value: &str) -> Result<mj_core::transcript::TranscriptRole, String> {
    serde_json::from_value(serde_json::Value::String(value.into())).map_err(|_| {
        format!(
            "unknown transcript role {value:?}; expected user, agent, message, thought, tool, terminal, plan, plan_proposal, or system"
        )
    })
}

fn requested_transcript_roles(
    args: &TranscriptArgs,
) -> Result<Vec<mj_core::transcript::TranscriptRole>> {
    use mj_core::transcript::TranscriptRole;

    if args.finished_only {
        if args.role.iter().any(|role| *role != TranscriptRole::Agent) {
            bail!("--finished-only can only be combined with --role agent");
        }
        return Ok(vec![TranscriptRole::Agent]);
    }

    if args.role.is_empty() {
        return Ok(vec![
            TranscriptRole::User,
            TranscriptRole::Agent,
            TranscriptRole::Message,
            TranscriptRole::Thought,
            TranscriptRole::Plan,
            TranscriptRole::PlanProposal,
            TranscriptRole::System,
        ]);
    }
    Ok(args.role.clone())
}

#[derive(Debug, Args)]
pub(crate) struct UsageArgs {
    /// Read one session, including accounting retained after cleanup.
    #[arg(long, required_unless_present = "parent", conflicts_with = "parent")]
    session: Option<String>,
    /// Read this session and every descendant, including cleaned-up children.
    #[arg(long, required_unless_present = "session", conflicts_with = "session")]
    parent: Option<String>,
    /// Return entries after this sequence number (one session only).
    #[arg(long, conflicts_with = "parent")]
    after_seq: Option<u64>,
    /// Most entries to return in one page (one session only).
    #[arg(long, conflicts_with = "parent")]
    limit: Option<usize>,
    /// Print the response as JSON instead of text.
    #[arg(long)]
    json: bool,
}

pub(crate) async fn usage(args: UsageArgs) -> Result<()> {
    let client = ApiClient::connect().await?;
    if let Some(parent) = args.parent {
        let tree = client.usage_tree(&parent).await?;
        if args.json {
            return print_json(&tree);
        }
        print_lines(usage_tree_lines(&tree));
    } else {
        let session = args
            .session
            .context("usage requires --session or --parent")?;
        let page = client.usage(&session, args.after_seq, args.limit).await?;
        if args.json {
            return print_json(&page);
        }
        print_lines(usage_lines(&page));
    }
    Ok(())
}

fn model_usage_lines(groups: &[mj_core::storage::UsageModelTotal]) -> Vec<String> {
    let mut lines = Vec::new();
    for group in groups {
        lines.push(format!(
            "model {}; effort {}:",
            group.selection.model.as_deref().unwrap_or("unknown"),
            group.selection.effort.as_deref().unwrap_or("unknown")
        ));
        for (counter, total) in &group.totals {
            lines.push(format!(
                "  {counter}  {}  ({} turns)",
                total.tokens, total.reported_turns
            ));
        }
    }
    lines
}

fn turn_cost_usd(ticks: u64) -> String {
    mj_core::usage::ProviderTurnCost::from_usd_ticks(ticks, false).usd
}

fn usage_tree_lines(tree: &mj_core::storage::UsageTree) -> Vec<String> {
    let coverage = &tree.summary.coverage;
    let mut lines = vec![
        format!(
            "usage tree {}: {} sessions",
            tree.parent_session_id,
            tree.sessions.len()
        ),
        format!(
            "totals from the {} of {} turns that reported a whole turn; {} started turns without a completion report",
            coverage.full_turn_reports, coverage.recorded_turns, coverage.unfinished_turns
        ),
    ];
    lines.extend(model_usage_lines(&tree.summary.by_model));
    lines.push(format!(
        "not in totals: {} last-request, {} unknown-scope, {} missing reports",
        coverage.last_request_reports, coverage.unspecified_reports, coverage.missing_reports
    ));
    lines.push(format!(
        "priced turn cost: {} USD ({} provider, {} estimated, {} missing costs)",
        turn_cost_usd(tree.summary.turn_cost_usd_ticks),
        coverage.provider_cost_turns,
        coverage.estimated_cost_turns,
        coverage.missing_cost_turns
    ));
    for session in &tree.sessions {
        lines.push(format!(
            "session {}: task {}; parent {}; {}",
            session.session_id,
            session.task_name.as_deref().unwrap_or("root"),
            session.parent_session_id.as_deref().unwrap_or("none"),
            if session.operational_session_present {
                "retained session"
            } else {
                "accounting retained after cleanup"
            }
        ));
        if let Some(cost) = &session.summary.provider_session_cost {
            lines.push(format!(
                "  provider session cost: {:.4} {}",
                cost.amount, cost.currency
            ));
        }
        let cost_coverage = &session.summary.coverage;
        lines.push(format!(
            "  priced turn cost: {} USD ({} provider, {} estimated, {} missing costs)",
            turn_cost_usd(session.summary.turn_cost_usd_ticks),
            cost_coverage.provider_cost_turns,
            cost_coverage.estimated_cost_turns,
            cost_coverage.missing_cost_turns
        ));
    }
    lines
}

/// What `mj usage` prints without `--json`.
///
/// The coverage is printed with the totals, never apart from them: a harness
/// that reports only its last request, or nothing, would otherwise make a
/// partial count read as the whole session's.
fn usage_lines(page: &mj_core::storage::UsagePage) -> Vec<String> {
    let coverage = &page.coverage;
    let mut lines = vec![format!(
        "totals from the {} of {} turns that reported a whole turn:",
        coverage.full_turn_reports, coverage.recorded_turns
    )];
    if page.totals.is_empty() {
        lines.push("  no counters".to_owned());
    }
    for (counter, total) in &page.totals {
        lines.push(format!(
            "  {counter}  {}  ({} turns)",
            total.tokens, total.reported_turns
        ));
    }
    let left_out = [
        (
            coverage.last_request_reports,
            "reported only their last request",
        ),
        (coverage.unspecified_reports, "reported an unknown scope"),
        (coverage.missing_reports, "reported nothing"),
    ]
    .into_iter()
    .filter(|(count, _)| *count > 0)
    .map(|(count, what)| format!("{count} {what}"))
    .collect::<Vec<_>>();
    if !left_out.is_empty() {
        lines.push(format!(
            "not in the totals: {} (turns)",
            left_out.join(", ")
        ));
    }
    if let Some(cost) = &page.provider_session_cost {
        lines.push(format!(
            "provider cost for the session so far: {:.4} {}",
            cost.amount, cost.currency
        ));
    }
    lines.push(format!(
        "priced turn cost: {} USD ({} provider, {} estimated, {} missing costs)",
        turn_cost_usd(page.turn_cost_usd_ticks),
        coverage.provider_cost_turns,
        coverage.estimated_cost_turns,
        coverage.missing_cost_turns
    ));
    lines.extend(model_usage_lines(&page.by_model));
    if coverage.unfinished_turns > 0 {
        lines.push(format!(
            "{} started turns without a completion report",
            coverage.unfinished_turns
        ));
    }
    lines.push(format!(
        "next after seq {}; latest seq {}",
        page.next_after_seq, page.latest_seq
    ));
    lines
}

#[derive(Debug, Args)]
pub(crate) struct PutFileArgs {
    /// Session id, as `mj sessions` lists it.
    #[arg(long)]
    session: String,
    /// Destination relative to the session workspace.
    #[arg(long)]
    path: String,
    /// Source file, or - for stdin.
    source: PathBuf,
    /// Replace the file if it already exists.
    #[arg(long)]
    overwrite: bool,
    /// Print the response as JSON instead of text.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
pub(crate) struct ElicitationsArgs {
    /// Session id, as `mj sessions` lists it.
    #[arg(long)]
    session: String,
    /// Print the response as JSON instead of text.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
pub(crate) struct RespondArgs {
    /// Session id, as `mj sessions` lists it.
    #[arg(long)]
    session: String,
    /// Id of the input request to answer, as `mj elicitations` lists it.
    #[arg(long)]
    elicitation: String,
    /// JSON response, or - for stdin.
    response: Option<String>,
    /// Read the JSON response from this file instead.
    #[arg(long)]
    response_file: Option<PathBuf>,
}

pub(crate) async fn put_file(args: PutFileArgs) -> Result<()> {
    let (body, content_length) = if args.source == std::path::Path::new("-") {
        (
            reqwest::Body::wrap_stream(tokio_util::io::ReaderStream::new(tokio::io::stdin())),
            None,
        )
    } else {
        let file = tokio::fs::File::open(&args.source)
            .await
            .with_context(|| format!("open {}", args.source.display()))?;
        let metadata = file
            .metadata()
            .await
            .with_context(|| format!("inspect {}", args.source.display()))?;
        if metadata.is_file() {
            (reqwest::Body::from(file), Some(metadata.len()))
        } else {
            (
                reqwest::Body::wrap_stream(tokio_util::io::ReaderStream::new(file)),
                None,
            )
        }
    };
    let written = ApiClient::connect()
        .await?
        .put_file(
            &args.session,
            &args.path,
            body,
            content_length,
            args.overwrite,
        )
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
    /// Session id, as `mj sessions` lists it.
    #[arg(long)]
    session: String,
    /// Compare against this Git commit or revision instead of the launch base.
    #[arg(long)]
    base: Option<String>,
    /// Print the response as JSON instead of text.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
pub(crate) struct FetchArgs {
    /// Session id, as `mj sessions` lists it.
    #[arg(long)]
    session: String,
    /// File or directory to fetch: relative to the directory the session's
    /// agent runs in, or absolute on the session's target. It must lie in the
    /// session's workspace, its sub-agent report directories, or the target's
    /// temporary directory.
    #[arg(long)]
    path: String,
    /// Write the file, or the directory's zip archive, to standard output
    /// instead of the temporary directory.
    #[arg(long, conflicts_with = "json")]
    stdout: bool,
    /// List the directory, or the file, with each entry's size and
    /// modification time, on standard output. Symlinks are listed, not
    /// followed.
    #[arg(long)]
    ls: bool,
    /// Print the result as JSON: the listing with `--ls`, otherwise where the
    /// copy landed.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
pub(crate) struct ExportArgs {
    /// Session id, as `mj sessions` lists it.
    #[arg(long)]
    session: String,
    /// What to export: a patch, a pushed branch, or a git bundle.
    #[arg(long, value_enum, default_value_t = ExportKindArg::Patch)]
    kind: ExportKindArg,
    /// Branch to push, required by `--kind branch`.
    #[arg(long, required_if_eq("kind", "branch"))]
    branch: Option<String>,
    /// Write the export here instead of standard output.
    #[arg(long)]
    out: Option<PathBuf>,
    /// Print the response as JSON instead of text.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub(crate) enum ExportKindArg {
    Patch,
    Branch,
    Bundle,
}

#[derive(Debug, Args)]
pub(crate) struct SessionsArgs {
    /// Show one session, including how its last turn ended.
    #[arg(long)]
    session: Option<String>,
    /// Include sub-agents as well as top-level sessions.
    #[arg(long)]
    all: bool,
    /// List only sessions on this target, as `mj hosts` names its targets.
    /// A fleet's instances are selected this way.
    #[arg(long, conflicts_with = "session")]
    target: Option<String>,
    /// List only sessions on this machine, as `mj hosts` names its hosts.
    #[arg(long, conflicts_with = "session")]
    host: Option<String>,
    /// Print the response as JSON instead of text.
    #[arg(long)]
    json: bool,
    #[command(flatten)]
    pub(crate) workspace: crate::WorkspaceName,
}

#[derive(Debug, Args)]
pub(crate) struct SessionArgs {
    /// Session id, as `mj sessions` lists it.
    #[arg(long)]
    session: String,
    /// Print the response as JSON instead of text.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
pub(crate) struct StopTaskArgs {
    /// Session id, as `mj sessions` lists it.
    #[arg(long)]
    session: String,
    /// Opaque task id from `mj sessions --session <id> --json`.
    #[arg(
        value_name = "TASK_ID",
        required_unless_present = "all",
        conflicts_with = "all"
    )]
    task_id: Option<String>,
    /// Stop every currently listed task that the worker can stop.
    #[arg(long)]
    all: bool,
    /// Print accepted task ids, skipped task ids and failures as JSON.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, serde::Serialize)]
struct StopTaskReport {
    session_id: String,
    accepted_task_ids: Vec<String>,
    skipped_task_ids: Vec<String>,
    failures: std::collections::BTreeMap<String, String>,
}

pub(crate) async fn stop_task(args: StopTaskArgs) -> Result<()> {
    let client = ApiClient::connect().await?;
    let report = stop_tasks(&client, &args).await?;
    if args.json {
        print_json(&report)?;
    } else {
        for id in &report.accepted_task_ids {
            println!(
                "stop accepted for background task {id} in {}",
                report.session_id
            );
        }
        for id in &report.skipped_task_ids {
            println!("background task {id} cannot be stopped by this worker; skipped");
        }
        for (id, error) in &report.failures {
            eprintln!("background task {id}: {error}");
        }
        if report.accepted_task_ids.is_empty() && report.failures.is_empty() {
            println!("no stoppable background tasks in {}", report.session_id);
        }
    }
    if !report.failures.is_empty() {
        bail!(
            "could not stop {} background task(s)",
            report.failures.len()
        );
    }
    Ok(())
}

async fn stop_tasks(client: &ApiClient, args: &StopTaskArgs) -> Result<StopTaskReport> {
    let mut report = StopTaskReport {
        session_id: args.session.clone(),
        accepted_task_ids: Vec::new(),
        skipped_task_ids: Vec::new(),
        failures: Default::default(),
    };
    let task_ids = if args.all {
        let session = client
            .session_if_known(&args.session)
            .await?
            .with_context(|| format!("unknown session {}", args.session))?;
        let (stoppable, skipped): (Vec<_>, Vec<_>) = session
            .background_tasks
            .into_iter()
            .partition(|task| task.can_stop);
        report.skipped_task_ids = skipped.into_iter().map(|task| task.id).collect();
        stoppable
            .into_iter()
            .map(|task| task.id)
            .collect::<Vec<_>>()
    } else {
        vec![
            args.task_id
                .clone()
                .context("name a task id or use --all")?,
        ]
    };
    let outcomes = futures::future::join_all(
        task_ids
            .iter()
            .map(|id| client.stop_background_task(&args.session, id)),
    )
    .await;
    for (id, outcome) in task_ids.into_iter().zip(outcomes) {
        match outcome {
            Ok(()) => report.accepted_task_ids.push(id),
            Err(error) => {
                report.failures.insert(id, format!("{error:#}"));
            }
        }
    }
    Ok(report)
}

/// Suspend a session, keeping only its Git state (branches, tags, stashes, commits,
/// staged and unstaged changes to tracked files, and linked worktrees inside the checkout).
/// Untracked files are lost unless you `git add` them first; Git-ignored files are never kept.
/// Scratch in `$MJ_KEEP_DIR` travels with Move but is not saved when the session is suspended.
#[derive(Debug, Args)]
pub(crate) struct SuspendArgs {
    /// Session id, as `mj sessions` lists it.
    #[arg(long)]
    session: Option<String>,
    /// Taken only so the command can explain itself. `mj suspend <id>` is a
    /// common mistake, and every session command in this CLI names its
    /// session with `--session`, so clap's "unexpected argument" would leave
    /// the person guessing which option it wanted.
    #[arg(value_name = "SESSION", hide = true)]
    misplaced_session: Option<String>,
    /// Discard untracked files, nested repositories, untracked stashes, and `.mj/keep/` contents.
    #[arg(long)]
    discard_untracked: bool,
    /// Abort any in-progress Git operation (merge, rebase, cherry-pick, revert, am, bisect).
    #[arg(long)]
    abort_operation: bool,
    /// Stop the session without taking a fresh checkpoint.
    #[arg(long)]
    without_checkpoint: bool,
    /// Confirm `--without-checkpoint` when no verified checkpoint exists to resume from.
    #[arg(long)]
    yes: bool,
    /// Inspect what would be saved and what would be lost without changing anything.
    #[arg(long)]
    dry_run: bool,
    /// Print the response as JSON instead of text.
    #[arg(long)]
    json: bool,
}

/// Remove the session even if environment cleanup fails; report remaining resources in a warning.
#[derive(Debug, Args)]
pub(crate) struct DestroyArgs {
    /// Session id, as `mj sessions` lists it.
    #[arg(long)]
    session: String,
    /// Print the response as JSON instead of text.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
pub(crate) struct NoticesArgs {
    /// Session id; notices remain readable after the session is removed.
    #[arg(long)]
    session: String,
    /// Print the response as JSON instead of text.
    #[arg(long)]
    json: bool,
}

pub(crate) async fn notices(args: NoticesArgs) -> Result<()> {
    let notices = ApiClient::connect().await?.notices(&args.session).await?;
    if args.json {
        print_json(&notices)
    } else {
        if notices.is_empty() {
            println!(
                "No cleanup notices are currently retained for {}. Notices can arrive while destruction runs; they are kept in the current daemon's recent notice history.",
                args.session
            );
        }
        for notice in notices {
            println!("{}", sanitize_terminal_text(&notice.text));
        }
        Ok(())
    }
}

#[derive(Debug, Args)]
#[command(group(
    clap::ArgGroup::new("resume-subject")
        .required(true)
        .args(["session", "wiki"])
))]
pub(crate) struct ResumeArgs {
    /// Suspended, lost, or failed session to resume. It keeps its identity,
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
    /// What to do with prompts queued when the session was suspended.
    #[arg(long, value_enum)]
    queue: Option<ResumeQueueArg>,
    /// Print the response as JSON instead of text.
    #[arg(long)]
    json: bool,
    #[command(flatten)]
    pub(crate) workspace: crate::WorkspaceName,
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
    /// Print the response as JSON instead of text.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
#[command(group(
    ArgGroup::new("github_token_target")
        .args(["owner", "repo"])
        .required(true)
        .multiple(false)
))]
pub(crate) struct GithubTokenArgs {
    /// GitHub owner login whose installation should receive a token.
    #[arg(long)]
    owner: Option<String>,
    /// Repository in OWNER/NAME form. May be repeated to limit the token to
    /// several repositories from one installation.
    #[arg(long, action = clap::ArgAction::Append)]
    repo: Vec<String>,
}

#[derive(Debug, Args)]
pub(crate) struct WorkspacesListArgs {
    /// Print the response as JSON instead of text.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
pub(crate) struct WorkspaceCreateArgs {
    /// Workspace name, 1-64 characters. Naming one that already exists selects
    /// it instead of failing, so this is safe to run before every session.
    name: String,
    /// Print the response as JSON instead of text.
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
            workspace.id,
            workspace.session_count,
            sanitize_terminal_text(&workspace.name)
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
        created.workspace.id,
        sanitize_terminal_text(&created.workspace.name)
    );
    Ok(())
}

fn new_subagent_policy(args: &NewArgs) -> Result<Option<mj_core::subagent::SubagentPolicy>> {
    subagent_policy(
        args.subagents.as_deref(),
        args.subagent_straightforward.as_deref(),
        args.subagent_straightforward_effort.as_deref(),
        args.subagent_subtle.as_deref(),
        args.subagent_subtle_effort.as_deref(),
        args.subagent_model.as_deref(),
        args.subagent_effort.as_deref(),
    )
}

pub(crate) fn oracle_override(
    model: Option<&str>,
    effort: Option<&str>,
    enabled: Option<&str>,
) -> Result<Option<mj_core::config::SubagentChoice>> {
    if effort.is_some() && model.is_none() {
        bail!("--subagent-oracle-effort requires --subagent-oracle");
    }
    if effort == Some("adaptive") {
        bail!("adaptive oracle effort is not supported");
    }
    if model.is_some() && enabled == Some("disabled") {
        bail!("--subagent-oracle cannot be used with --oracle disabled");
    }
    if model.is_some_and(|model| model.trim().is_empty()) {
        bail!("--subagent-oracle requires a model name");
    }
    Ok(model.map(|model| mj_core::config::SubagentChoice {
        model: model.to_owned(),
        effort: effort.map(str::to_owned),
    }))
}

pub(crate) fn subagent_policy(
    mode: Option<&str>,
    straightforward: Option<&str>,
    straightforward_effort: Option<&str>,
    subtle: Option<&str>,
    subtle_effort: Option<&str>,
    retired_model: Option<&str>,
    retired_effort: Option<&str>,
) -> Result<Option<mj_core::subagent::SubagentPolicy>> {
    use mj_core::subagent::SubagentPolicy;
    const REMEDY: &str = "Configure [subagents.straightforward] and optionally [subagents.subtle] in config.toml or TUI Settings -> Sub-agents";
    if retired_model.is_some() || retired_effort.is_some() {
        bail!(
            "--subagent-model and --subagent-effort are retired; use --subagent-straightforward, --subagent-straightforward-effort, --subagent-subtle, and --subagent-subtle-effort"
        );
    }
    if straightforward_effort.is_some() && straightforward.is_none() {
        bail!("--subagent-straightforward-effort requires --subagent-straightforward");
    }
    if subtle_effort.is_some() && subtle.is_none() {
        bail!("--subagent-subtle-effort requires --subagent-subtle");
    }
    if straightforward_effort == Some("adaptive") || subtle_effort == Some("adaptive") {
        bail!("adaptive sub-agent effort is not supported");
    }
    if straightforward.is_some() || subtle.is_some() {
        if mode.is_some_and(|mode| mode != "mjolnir") {
            bail!(
                "per-type sub-agent overrides require --subagents mjolnir or an omitted --subagents"
            );
        }
        return Ok(Some(SubagentPolicy::Mjolnir {
            orchestrators: false,
            straightforward: straightforward.map(|model| {
                Box::new(mj_core::config::SubagentChoice {
                    model: model.to_owned(),
                    effort: straightforward_effort.map(str::to_owned),
                })
            }),
            subtle: subtle.map(|model| {
                Box::new(mj_core::config::SubagentChoice {
                    model: model.to_owned(),
                    effort: subtle_effort.map(str::to_owned),
                })
            }),
        }));
    }
    Ok(match mode {
        Some("native") => Some(SubagentPolicy::Native),
        Some("mjolnir") => Some(SubagentPolicy::Mjolnir {
            orchestrators: false,
            straightforward: None,
            subtle: None,
        }),
        // Not offered in Create or Move: benchmarks use it for solo lanes.
        Some("none") => Some(SubagentPolicy::None),
        None => None,
        Some(other) => {
            bail!("unknown subagents mode {other:?}; use native, mjolnir or none. {REMEDY}")
        }
    })
}

/// Said when a session asks for the review mode that was removed.
const TURN_REVIEW_REMOVED: &str = "Per-turn review was removed. Use `--review push`, or omit `--review` to review before push when reviewers are configured.";

/// The session's review overrides, or `None` to follow all of `[review]`.
fn new_session_review(args: &NewArgs) -> Result<Option<mj_core::config::SessionReview>> {
    session_review(args.review.as_deref(), &args.reviewers)
}

/// The review flags shared by `mj new` and `mj import`.
pub(crate) fn session_review(
    trigger: Option<&str>,
    choices: &[String],
) -> Result<Option<mj_core::config::SessionReview>> {
    use mj_core::config::SessionReview;
    ensure!(
        choices.len() <= mj_core::config::MAX_REVIEWERS,
        "at most three --reviewer values are allowed"
    );
    let reviewers = choices
        .iter()
        .map(|value| {
            let (model, effort) =
                value
                    .rsplit_once(':')
                    .map_or((value.as_str(), None), |(model, suffix)| {
                        if matches!(
                            suffix,
                            "minimal" | "low" | "medium" | "high" | "xhigh" | "max"
                        ) {
                            (model, Some(suffix))
                        } else if suffix.is_empty() {
                            (model, None)
                        } else {
                            (value.as_str(), None)
                        }
                    });
            ensure!(!model.trim().is_empty(), "reviewer model must not be empty");
            Ok(mj_core::config::SubagentChoice {
                model: model.to_owned(),
                effort: effort.map(str::to_owned),
            })
        })
        .collect::<Result<Vec<_>>>()?;
    let reviewers = (!reviewers.is_empty()).then_some(reviewers);
    let Some(trigger) = trigger else {
        return Ok(reviewers.map(|reviewers| SessionReview::Inherit {
            reviewers: Some(reviewers),
        }));
    };
    Ok(Some(match trigger {
        "off" => SessionReview::Off,
        "push" => SessionReview::Push { reviewers },
        "turn" => bail!("{TURN_REVIEW_REMOVED}"),
        other => bail!("unknown --review value {other:?}; use off or push"),
    }))
}

/// Create a session and deliver its optional first prompt.
pub(crate) async fn new_session(args: NewArgs, requested_workspace: Option<String>) -> Result<()> {
    let prompt = read_prompt(args.prompt.clone(), args.prompt_file.clone())?;
    let (client, workspace_id, target_id, project_directory, options) =
        infer_new_session_inputs(&args, requested_workspace.as_deref()).await?;
    let (bundle_id, project_directory) = resolve_new_session_project_target(
        &client,
        args.bundle.clone(),
        project_directory,
        target_id.as_deref(),
        options,
        |dir| async move { crate::go::quick_bundle_for_directory(&dir).await },
    )
    .await?;
    let request = StartSessionRequest {
        remember_container_size: false,
        subagents: new_subagent_policy(&args)?,
        oracle: args.oracle.as_deref().map(|choice| choice == "enabled"),
        supervisors: args
            .supervisors
            .as_deref()
            .map(|choice| choice == "enabled"),
        oracle_choice: oracle_override(
            args.subagent_oracle.as_deref(),
            args.subagent_oracle_effort.as_deref(),
            args.oracle.as_deref(),
        )?,
        review: new_session_review(&args)?,
        no_project_memory: args.no_project_memory,
        no_github_events: args.no_github_events,
        no_github_credentials: args.no_github_credentials,
        notifications_ignored: args.ignore_notifications,
        create_managed_worktree: None,
        at: args.at.clone(),
        branch: args.branch.clone(),
        base: args.base.clone(),
        no_later_history: args.no_later_history,
        workspace_id: Some(workspace_id),
        profile_id: args.profile.clone(),
        target_id,
        bundle_id,
        project_directory,
        title: args.title.clone(),
        model: args.model.clone(),
        effort: args.effort.clone(),
        prompt,
        cpus: args.cpus,
        memory_bytes: args
            .memory_gib
            .map(|gib| {
                gib.checked_mul(1 << 30)
                    .context("--memory-gib is too large")
            })
            .transpose()?,
    };
    let response = client.start(&request).await.map_err(name_launch_flags)?;
    if let Some(notice) = &response.notice {
        write_start_notice(std::io::stderr().lock(), notice)?;
    }
    if args.json {
        return print_json(&response);
    }
    println!("session {}", response.session_id);
    if let Some(turn_id) = response.turn_id {
        println!("turn {turn_id}");
    }
    Ok(())
}

async fn resolve_new_session_project_target<F, Fut>(
    client: &ApiClient,
    bundle_id: Option<String>,
    project_directory: Option<PathBuf>,
    target_id: Option<&str>,
    options: Option<mj_daemon::server::api::LaunchOptions>,
    create_quick_bundle: F,
) -> Result<(Option<String>, Option<PathBuf>)>
where
    F: FnOnce(PathBuf) -> Fut,
    Fut: std::future::Future<Output = Result<String>>,
{
    if bundle_id.is_none()
        && let (Some(directory), Some(target_id)) = (project_directory.as_ref(), target_id)
    {
        let options = match options {
            Some(options) => options,
            None => client.launch_options().await?,
        };
        if options
            .targets
            .iter()
            .any(|target| target.id == target_id && !target.requires_project_directory)
        {
            return Ok((Some(create_quick_bundle(directory.clone()).await?), None));
        }
    }
    Ok((bundle_id, project_directory))
}

async fn infer_new_session_inputs(
    args: &NewArgs,
    requested_workspace: Option<&str>,
) -> Result<(
    ApiClient,
    String,
    Option<String>,
    Option<PathBuf>,
    Option<mj_daemon::server::api::LaunchOptions>,
)> {
    use mj_daemon::server::api::resolve_launch_choices;
    let mut open = Vec::new();
    let mut workspace_id = args.workspace_id.clone();
    if workspace_id.is_none()
        && let Some(name) = requested_workspace
    {
        crate::refuse_unknown_workspace(name).await?;
        workspace_id = Some(crate::resolve_store_workspace(Some(name)).await?);
    }
    if let Some(workspace_id) = &workspace_id
        && args.target.is_some()
        && (args.profile.is_some() || args.model.is_some())
        && args
            .project_directory
            .as_ref()
            .map_or(args.bundle.is_some(), |path| path.is_absolute())
    {
        return Ok((
            ApiClient::connect().await?,
            workspace_id.clone(),
            args.target.clone(),
            args.project_directory.clone(),
            None,
        ));
    }
    let mut listed = if workspace_id.is_none() {
        crate::listed_workspaces_without_starting().await
    } else {
        None
    };
    let client = if listed.as_ref().is_some_and(Vec::is_empty) {
        ApiClient::connect_running().await.ok()
    } else {
        Some(ApiClient::connect().await?)
    };
    if workspace_id.is_none() && listed.is_none() {
        listed = crate::listed_workspaces_without_starting().await;
    }

    let options = match &client {
        Some(client) => Some(client.launch_options().await?),
        None => None,
    };
    let selected_target = options.as_ref().and_then(|options| {
        let auto_primary = args.profile.is_none()
            && args.model.is_none()
            && args.effort.is_none()
            && options.primary_configured;
        let resolve = |need_profile| {
            resolve_launch_choices(
                args.profile.as_deref(),
                args.target.as_deref(),
                need_profile,
                options.profiles.iter().map(|profile| profile.id.as_str()),
                options
                    .targets
                    .iter()
                    .filter(|target| !target.runtime_missing)
                    .map(|target| target.id.as_str()),
                options.default.as_ref(),
            )
        };
        if let Err(choices) = resolve(args.model.is_none() && !auto_primary) {
            open.extend(choices.into_iter().map(|choice| {
                choice.replacen("a profile_id", "--profile ID", 1).replacen(
                    "a target_id",
                    "--target ID",
                    1,
                )
            }));
        }
        let (_, target_id) = resolve(false).ok()?;
        options.targets.iter().find(|target| target.id == target_id)
    });
    if options.is_none() {
        if args.profile.is_none() && args.model.is_none() {
            open.push("--profile ID".to_owned());
        }
        if args.target.is_none() {
            open.push("--target ID".to_owned());
        }
    }
    let colocated = client
        .as_ref()
        .is_some_and(ApiClient::daemon_on_client_filesystem);
    let local = colocated && selected_target.is_some_and(|target| target.kind != "ssh-bare");
    let canonical_folder = std::cell::OnceCell::new();
    let folder = || {
        canonical_folder
            .get_or_init(|| {
                let resolve = || -> Result<PathBuf> {
                    let path = match &args.project_directory {
                        Some(path) => mj_core::path_input::expand_local(path)?,
                        None => std::env::current_dir().context("read the current folder")?,
                    };
                    path.canonicalize()
                        .with_context(|| format!("open folder {}", path.display()))
                };
                resolve().map_err(|error| format!("{error:#}"))
            })
            .clone()
            .map_err(anyhow::Error::msg)
    };

    if workspace_id.is_none() {
        let known_empty = listed.as_ref().is_some_and(Vec::is_empty);
        let workspaces: Vec<_> = listed
            .unwrap_or_default()
            .into_iter()
            .map(|workspace| crate::daemon::WorkspaceListing { workspace })
            .collect();
        if local && let Ok(directory) = folder() {
            workspace_id = crate::go::inferred_workspace(&workspaces, &directory).await?;
        }
        if workspace_id.is_none() {
            if let [only] = workspaces.as_slice() {
                workspace_id = Some(only.workspace.id.clone());
            } else if known_empty {
                open.push("--workspace NAME (this instance has none yet. Create one with `mj workspaces create NAME`)".to_owned());
            } else if workspaces.is_empty() {
                open.push("--workspace NAME (see `mj workspaces`)".to_owned());
            } else {
                let names = workspaces
                    .iter()
                    .map(|entry| entry.workspace.name.as_str())
                    .collect::<Vec<_>>();
                open.push(format!("--workspace NAME (one of: {})", names.join(", ")));
            }
        }
    }
    let mut project_directory = args.project_directory.clone();
    if project_directory.is_none() && args.bundle.is_none() {
        match selected_target {
            Some(target) if target.requires_project_directory && local => match folder() {
                Ok(path) => project_directory = Some(path),
                Err(error) => open.push(format!("--project-directory PATH ({error:#})")),
            },
            Some(target) if target.requires_project_directory => {
                let host = target.host.as_deref().unwrap_or("the target host");
                open.push(format!("--project-directory PATH (the folder on {host})"));
            }
            _ => open.push("--bundle ID or --project-directory PATH".to_owned()),
        }
    }
    if !open.is_empty() {
        bail!(
            "mj new needs more than it was given. Pass:\n  {}",
            open.join("\n  ")
        );
    }
    if local && args.project_directory.is_some() {
        project_directory = Some(folder()?);
    }
    // Pin the target used to interpret the directory even if the saved
    // default changes between reading launch options and creating a session.
    let target_id = selected_target
        .map(|target| target.id.clone())
        .or_else(|| args.target.clone());
    Ok((
        client.context("launch options are unavailable")?,
        workspace_id.context("no workspace was selected")?,
        target_id,
        project_directory,
        options,
    ))
}

pub(crate) fn write_start_notice(
    mut output: impl std::io::Write,
    notice: &str,
) -> std::io::Result<()> {
    writeln!(output, "{}", sanitize_terminal_text(notice))
}

/// Name the command-line flags where a refused start names the API's fields.
///
/// The API speaks to every client, so its refusals name request fields such
/// as `profile_id`, `target_id` and `at`; someone at a shell typed
/// `--profile`, `--target` and `--at`, and that is what they need to read
/// (F-9). Refusals quote the one-word fields in backticks, so only field
/// names are replaced.
pub(crate) fn name_launch_flags(error: anyhow::Error) -> anyhow::Error {
    let message = format!("{error:#}");
    let named = message
        .replace("profile_id", "--profile")
        .replace("target_id", "--target")
        .replace("bundle_id", "--bundle")
        .replace("project_directory", "--project-directory")
        .replace("`no_later_history`", "--no-later-history")
        .replace("`at`", "--at")
        .replace("`branch`", "--branch")
        .replace("`base`", "--base")
        .replace("`cpus`", "--cpus")
        .replace("`memory_bytes`", "--memory-gib");
    if named == message {
        return error;
    }
    anyhow::anyhow!(named)
}

/// Name the `mj suspend` flag where a refusal names the API field, as
/// [`name_launch_flags`] does for `mj new` (launch finding R2-3).
pub(crate) fn name_suspend_flags(error: anyhow::Error) -> anyhow::Error {
    let message = format!("{error:#}");
    let named = message.replace(
        "acknowledge_unpublished_work=true",
        "--acknowledge-unpublished-work",
    );
    if named == message {
        return error;
    }
    anyhow::anyhow!(named)
}

#[derive(Debug, Args)]
pub(crate) struct ClearQueueArgs {
    /// Session id, as `mj sessions` lists it.
    #[arg(long)]
    session: String,
    /// Stable producer-chosen identity; repeat it after an ambiguous reply.
    #[arg(long)]
    command_id: Option<String>,
    /// Print the API response as JSON.
    #[arg(long)]
    json: bool,
}

pub(crate) async fn clear_queue(args: ClearQueueArgs) -> Result<()> {
    let result = ApiClient::connect()
        .await?
        .clear_queue(&args.session, args.command_id)
        .await?;
    if args.json {
        print_json(&result)
    } else {
        println!("queue cancellation {} accepted", result.turn_id);
        Ok(())
    }
}

/// Send a prompt as the user and wait for the turn it became.
pub(crate) async fn prompt(args: PromptArgs) -> Result<()> {
    prompt_from(args, calling_session()?).await
}

async fn prompt_from(args: PromptArgs, calling_session: Option<String>) -> Result<()> {
    // A prompt sent this way reads exactly as the user's own. An agent sends
    // as itself instead, and its message is labelled with its session.
    ensure!(
        calling_session.is_none(),
        "`mj prompt` sends as the user, so it cannot run inside a session \
         (MJ_SESSION_ID is set). Send with `mj message`, then follow with `mj wait`."
    );
    let text = read_prompt(args.text.clone(), args.prompt_file.clone())?
        .context("pass the prompt text, --prompt-file, or `-` to read standard input")?;
    let client = ApiClient::connect().await?;
    let accepted = match args.command_id {
        Some(id) => client.prompt_with_id(&args.session, text, Some(id)).await?,
        None => client.prompt(&args.session, text).await?,
    };
    // Named before the wait, so a caller whose wait is cut off can go on
    // with `mj wait --turn`.
    if !args.json {
        eprintln!("turn {}", accepted.turn_id);
    }
    // Only the wait follows the daemon across an upgrade; the prompt was
    // accepted once and is never sent again.
    let response = wait_following_handoffs(
        client,
        &args.session,
        WaitRequest {
            return_on_input: args.return_on_input,
            turn_id: Some(accepted.turn_id),
            timeout_secs: args.timeout,
        },
        |request: &WaitRequest| {
            follow_handoff(wait_continuation(&args.session, request, args.json))
        },
    )
    .await?;
    report_wait(&response, args.json)
}

pub(crate) async fn wait(args: WaitArgs) -> Result<()> {
    let client = ApiClient::connect().await?;
    let response = wait_following_handoffs(
        client,
        &args.session,
        WaitRequest {
            return_on_input: args.return_on_input,
            turn_id: args.turn,
            timeout_secs: args.timeout,
        },
        |request: &WaitRequest| {
            follow_handoff(wait_continuation(&args.session, request, args.json))
        },
    )
    .await?;
    report_wait(&response, args.json)
}

/// Connect to the daemon that replaced one being upgraded. When that daemon is
/// a newer protocol or release, which this client cannot speak to, finish the
/// command under the daemon's own build, as dashboards do. `continuation`
/// names only what is left of the command, so nothing is repeated.
async fn follow_handoff(continuation: Vec<String>) -> Result<ApiClient> {
    let error = match ApiClient::connect_after_handoff().await {
        Ok(client) => return Ok(client),
        Err(error) => error,
    };
    let Some(target) = crate::daemon::upgraded_daemon_executable().await? else {
        return Err(error);
    };
    Err(crate::daemon::continue_under_upgraded_build(
        &target,
        &continuation,
    ))
}

/// The rest of a wait: the same session and turn, the time left, and the
/// same output form.
fn wait_continuation(session: &str, request: &WaitRequest, json: bool) -> Vec<String> {
    let mut args = vec![
        "wait".to_owned(),
        "--session".to_owned(),
        session.to_owned(),
    ];
    if let Some(turn) = request.turn_id {
        args.extend(["--turn".to_owned(), turn.to_string()]);
    }
    if let Some(timeout) = request.timeout_secs {
        args.extend(["--timeout".to_owned(), timeout.to_string()]);
    }
    if request.return_on_input {
        args.push("--return-on-input".to_owned());
    }
    if json {
        args.push("--json".to_owned());
    }
    args
}

/// The rest of an event stream: the same filter, after the last event seen.
fn events_continuation(
    filter: &mj_controller::database::ApiEventFilter,
    after_seq: Option<u64>,
) -> Vec<String> {
    let mut args = vec!["events".to_owned()];
    if let Some(session) = &filter.session_id {
        args.extend(["--session".to_owned(), session.clone()]);
    }
    if let Some(workspace) = &filter.workspace_id {
        args.extend(["--workspace-id".to_owned(), workspace.clone()]);
    }
    if let Some(after_seq) = after_seq {
        args.extend(["--after-seq".to_owned(), after_seq.to_string()]);
    }
    args
}

/// Wait on a session, following the daemon across automatic upgrades.
///
/// A handoff ends the wait the old daemon was serving. A wait is a read, so
/// the next daemon is asked again for the same turn, with what is left of the
/// time budget: the handoff neither extends the wait nor resends a prompt.
/// Each retry follows a daemon that closed admission and is exiting, and
/// `reconnect` waits for the daemon that replaces it.
async fn wait_following_handoffs<Reconnect, Connecting>(
    first: ApiClient,
    session: &str,
    mut request: WaitRequest,
    mut reconnect: Reconnect,
) -> Result<WaitResponse>
where
    Reconnect: FnMut(&WaitRequest) -> Connecting,
    Connecting: std::future::Future<Output = Result<ApiClient>>,
{
    let started = std::time::Instant::now();
    let budget = request
        .timeout_secs
        .unwrap_or(mj_daemon::server::api::DEFAULT_WAIT_SECS);
    let mut client = first;
    loop {
        match client.wait(session, &request).await {
            Err(error) if crate::api_client::is_daemon_handoff(&error) => {
                eprintln!(
                    "The Mjolnir daemon is being replaced by an upgrade; waiting on the new daemon."
                );
                request.timeout_secs =
                    Some(budget.saturating_sub(started.elapsed().as_secs()).max(1));
                client = reconnect(&request).await?;
            }
            answered => return answered,
        }
    }
}

/// Print a wait result, and fail the process when the turn did not finish so a
/// shell script can branch on it.
fn report_wait(response: &WaitResponse, json: bool) -> Result<()> {
    if json {
        print_json(response)?;
    } else {
        print_lines(wait_report_lines(response));
        if !response.pending_elicitations.is_empty() {
            print_json(&response.pending_elicitations)?;
        }
    }
    match response.outcome {
        WaitOutcome::Finished | WaitOutcome::InputRequired => Ok(()),
        _ if suspended_cleanly(response) => Ok(()),
        outcome => bail!("the turn ended as {}", outcome_name(outcome)),
    }
}

/// Whether a wait ended because the session was suspended as asked, rather
/// than because a resume or a close failed. `mj suspend` tells the user to
/// watch with `mj wait`, so this ending is the success it was waiting for.
fn suspended_cleanly(response: &WaitResponse) -> bool {
    response.outcome == WaitOutcome::Stopped
        && response.session.lifecycle == ViewerLifecycleCategory::Suspended
        && response.session.error.is_none()
}

/// What `mj wait` prints, one line per entry.
///
/// Separate from the printing so the content can be tested: a turn that failed
/// used to print the bare word "error" and leave the reason in the transcript,
/// where automation never saw it (#1020).
fn wait_report_lines(response: &WaitResponse) -> Vec<String> {
    if suspended_cleanly(response) {
        return vec!["session suspended".to_owned()];
    }
    let mut lines = Vec::new();
    if response.outcome == WaitOutcome::Error
        && let Some(error) = &response.session.error
    {
        lines.push(format!("session error: {error}"));
    }
    // No number: `turn_id` is where the turn's prompt sits in the transcript,
    // not a count of anything a reader can use, so it stays in --json (I2-5).
    let mut summary = format!("turn {}", outcome_name(response.outcome));
    // How the turn ended, in the words `mj sessions` and the sub-agent notice
    // use; the harness's own spelling (`EndTurn`) stays in --json (R12-1).
    if let Some(stop_reason) = &response.stop_reason {
        let ended = mj_core::state::TurnOutcomeKind::Completed {
            stop_reason: stop_reason.clone(),
        };
        summary.push_str(&format!(" ({ended})"));
    }
    if let Some(elapsed_ms) = response.elapsed_ms {
        summary.push_str(&format!(" in {:.1}s", elapsed_ms.max(0) as f64 / 1000.0));
    }
    if let Some(tool_calls) = response.tool_calls.filter(|count| *count > 0) {
        summary.push_str(&format!(
            " · {tool_calls} tool call{}",
            if tool_calls == 1 { "" } else { "s" }
        ));
    }
    // A wait on a turn answers when that turn ends, whatever the session does
    // after it; say what that is, in the dashboard's words.
    if let Some(next) = &response.next {
        let mut activity = mj_client::usage_format::SessionActivity::of_state(next.clone());
        activity.capacity_retry =
            response
                .capacity_retry
                .as_ref()
                .map(|retry| mj_core::relay::CapacityRetry {
                    attempt: retry.attempt,
                    retry_at_ms: retry.retry_at_ms,
                    command_id: String::new(),
                    submitted: false,
                });
        activity.quota_recovery.clone_from(&response.quota_recovery);
        if let Some(background) = &response.session.background_work {
            activity.background_commands.clone_from(&background.tasks);
        }
        let now = u64::try_from(mj_core::clock::epoch_millis() / 1_000).unwrap_or_default();
        summary.push_str(&format!(
            "; the session is not idle: {}",
            mj_client::usage_format::format_activity_clock(now, None, &activity)
        ));
    }
    lines.push(summary);
    // A wait for turn N answers with the latest turn that ended, which can be
    // another turn: a later one, an earlier one that ended after N was
    // removed from the queue, or one the session ran without a prompt. Only
    // an answer about a finished turn has a turn number.
    if let Some(requested) = response.requested_turn_id
        && response.turn_number.is_some()
        && response.turn_id != Some(requested)
    {
        let answered = response.turn_id.map_or_else(
            || "a turn the session ran without a prompt".to_owned(),
            |turn| format!("turn {turn}"),
        );
        lines.push(format!("this is the outcome of {answered}, not of turn {requested}; a session keeps only its latest turn's outcome and reply"));
    }
    // The wait's message, the diagnostic and the agent's last message are
    // often one sentence (a Codex quota error was printed three times,
    // J-25), so each is printed only when it says something new.
    let said = |lines: &[String], text: &str| lines.iter().any(|line| line.trim() == text.trim());
    if let Some(message) = &response.message {
        lines.push(message.clone());
    }
    if response.outcome == WaitOutcome::Timeout && !response.pending_elicitations.is_empty() {
        let session_id = &response.session.id;
        lines.push(format!(
            "input is pending; inspect it with `mj elicitations --session {session_id}`, answer with `mj respond --session {session_id} --elicitation <id> '<json>'`, or use `mj prompt --return-on-input`"
        ));
    }
    // Why the turn ended, when the worker recorded a reason.
    if let Some(diagnostic) = &response.diagnostic
        && response.outcome != WaitOutcome::Finished
        && !said(&lines, &diagnostic.message)
    {
        lines.push(diagnostic.message.clone());
    }
    if let Some(recovery) = &response.quota_recovery {
        lines.push(recovery.notice.clone());
    }
    if let Some(retry) = &response.capacity_retry {
        lines.push(format!(
            "a server retry is armed (attempt {}); do not send another prompt yet",
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
    if let Some(final_message) = &response.final_message
        && !said(&lines, final_message)
    {
        lines.push(String::new());
        lines.push(final_message.clone());
    }
    lines
}

/// What `mj sessions --session` prints for one session, one line per entry.
fn session_report_lines(session: &ApiSession, now_ms: i64) -> Vec<String> {
    let mut lines = vec![format!(
        "{}  {}  {}",
        session.id,
        listed_state(session),
        session.title
    )];
    if let Some(operation) = &session.operation {
        use mj_daemon::server::ViewerOperationKind;
        let kind = match operation.kind {
            ViewerOperationKind::Create => "create",
            ViewerOperationKind::Resume => "resume",
            ViewerOperationKind::Move => "move",
            ViewerOperationKind::Suspend => "suspend",
            ViewerOperationKind::Destroy => "destroy",
            ViewerOperationKind::Stop => "stop",
            ViewerOperationKind::Cleanup => "cleanup",
            ViewerOperationKind::Checkpoint => "checkpoint",
        };
        lines.push(format!(
            "{kind} started {}",
            operation_start_time(operation.started_at_epoch_seconds)
        ));
        lines.extend(operation.stages.iter().map(|stage| {
            format!(
                "  {} since {}",
                stage.label,
                operation_start_time(stage.started_at_epoch_seconds)
            )
        }));
    }
    if let Some(relay) = &session.relay {
        lines.push(format!("worker relay is {}", relay_state_name(relay.state)));
    }
    // Silence is reported, never acted on. A turn waiting on a long build
    // is quiet and healthy, so this says what is true and leaves the
    // decision — keep waiting, or `mj interrupt-turn` — to the reader.
    if let Some(note) = session
        .activity_state
        .as_ref()
        .and_then(|state| mj_core::activity::silence_note(state, now_ms))
    {
        lines.push(format!("running, {note}"));
    }
    if let Some(error) = &session.error {
        lines.push(format!("error: {error}"));
    }
    if let Some(problem) = &session.storage_problem {
        lines.push(problem.clone());
    }
    if let Some(error) = &session.profile_login_error {
        lines.push(format!("profile {}: {error}", session.profile_id));
    }
    if let Some(outcome) = &session.last_turn_outcome {
        lines.push(format!("last turn {}", outcome.outcome));
    }
    lines
}

fn operation_start_time(seconds: u64) -> String {
    i64::try_from(seconds)
        .ok()
        .and_then(|seconds| chrono::DateTime::from_timestamp(seconds, 0))
        .map(|time| time.to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
        .unwrap_or_else(|| format!("{seconds} seconds since the Unix epoch"))
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
    let roles = requested_transcript_roles(&args)?;
    let client = ApiClient::connect().await?;
    let page = client
        .transcript(
            &args.session,
            args.after_seq,
            args.limit,
            &roles,
            args.finished_only,
        )
        .await?;
    if args.json {
        return print_json(&page);
    }
    for item in &page.items {
        println!("[{}] {}", item.seq, item.role);
        if !item.text.trim().is_empty() {
            println!("{}", sanitize_terminal_text(item.text.trim_end()));
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
    // The metadata form is always requested: the plain form prints only the
    // patch, but the divergence warning comes from the same answer.
    let body = client
        .diff(&args.session, args.base.as_deref(), true)
        .await?;
    let details = serde_json::from_str::<mj_checkpoint::archive::SessionDiff>(&body)
        .context("decode session diff metadata")?;
    if args.json {
        return print_json(&details);
    }
    // A redirected patch keeps its exact bytes (CRLF, control characters) so
    // it still applies; only a terminal gets the inert rendering.
    if std::io::stdout().is_terminal() {
        print!("{}", sanitize_terminal_text(&details.diff));
    } else {
        print!("{}", details.diff);
    }
    std::io::stdout()
        .flush()
        .context("write the session diff")?;
    if details.head_descends_from_base == Some(false) {
        let head = details.head.chars().take(12).collect::<String>();
        let base = details.base.chars().take(12).collect::<String>();
        eprintln!(
            "warning: HEAD {head} does not descend from base {base}; the diff includes history changes, not only session work. Pass --base to pick another base."
        );
    }
    Ok(())
}

pub(crate) async fn export(args: ExportArgs) -> Result<()> {
    let client = ApiClient::connect().await?;
    let kind = match args.kind {
        ExportKindArg::Patch => ExportKind::Patch,
        ExportKindArg::Branch => ExportKind::Branch,
        ExportKindArg::Bundle => ExportKind::Bundle,
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
                // A push carries commits and nothing else, and the agent may
                // have left work it never committed (F-13).
                println!(
                    "only committed work was pushed; uncommitted changes stay in the session (`mj diff --session {}` shows all of its work)",
                    args.session
                );
                Ok(())
            }
        },
        ExportResult::Bytes(bytes) => {
            let format = match args.kind {
                ExportKindArg::Bundle => "bundle",
                _ => "patch",
            };
            write_bytes(&bytes, args.out.as_deref(), args.json, format)
        }
    }
}

pub(crate) async fn fetch(args: FetchArgs) -> Result<()> {
    let client = ApiClient::connect().await?;
    let fetched = client.fetch(&args.session, &args.path, args.ls).await?;
    if args.ls {
        let listing: mj_checkpoint::archive::SessionListing =
            mj_core::runtime::spawn_blocking(move || {
                serde_json::from_reader(fetched.reader()).context("decode the listing")
            })
            .await??;
        if args.json {
            return print_json(&listing);
        }
        print_lines(listing_lines(&listing));
        return Ok(());
    }
    let session = args.session.clone();
    let saved = mj_core::runtime::spawn_blocking(
        move || -> Result<Option<mj_checkpoint::archive::SavedFetch>> {
            if args.stdout {
                let mut stdout = std::io::stdout().lock();
                std::io::copy(&mut fetched.reader(), &mut stdout)
                    .context("write the fetched path")?;
                stdout.flush().context("write the fetched path")?;
                return Ok(None);
            }
            Ok(Some(fetched.save(&std::env::temp_dir(), &session)?))
        },
    )
    .await??;
    let Some(saved) = saved else {
        return Ok(());
    };
    if args.json {
        return print_json(&saved);
    }
    println!("{}", saved.path.display());
    Ok(())
}

/// One line per entry, as `ls -l` puts it: the size right-aligned, the
/// modification time in UTC, then the name, with `/` after a directory and the
/// target after a symlink. A directory's own size says nothing about what it
/// holds and differs between filesystems, so it shows as `-`.
fn listing_lines(listing: &mj_checkpoint::archive::SessionListing) -> Vec<String> {
    use mj_checkpoint::archive::ListedKind;
    let size = |entry: &mj_checkpoint::archive::ListedEntry| match entry.kind {
        ListedKind::Directory => "-".to_owned(),
        _ => entry.size.to_string(),
    };
    let width = listing
        .entries
        .iter()
        .map(|entry| size(entry).len())
        .max()
        .unwrap_or(0);
    listing
        .entries
        .iter()
        .map(|entry| {
            let name = match (entry.kind, &entry.target) {
                (ListedKind::Directory, _) => format!("{}/", entry.name),
                (ListedKind::Symlink, Some(target)) => format!("{} -> {target}", entry.name),
                _ => entry.name.clone(),
            };
            format!(
                "{:>width$}  {:<20}  {name}",
                size(entry),
                entry.modified.as_deref().unwrap_or("-"),
            )
        })
        .collect()
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
        print_lines(session_report_lines(
            &session,
            mj_core::clock::epoch_millis(),
        ));
        return Ok(());
    }
    let workspace = match requested_workspace {
        Some(name) => Some(crate::resolve_store_workspace(Some(&name)).await?),
        None => None,
    };
    let mut list = client.sessions_in_workspace(workspace, args.all).await?;
    // The daemon's host table is the one place that says which machine a
    // target runs on. Unfiltered JSON does not use it.
    let rows = if args.json && args.target.is_none() && args.host.is_none() {
        Vec::new()
    } else {
        crate::daemon::connect_or_start()
            .await?
            .host_capacity()
            .await?
    };
    list.sessions = select_sessions(
        std::mem::take(&mut list.sessions),
        &rows,
        args.target.as_deref(),
        args.host.as_deref(),
    )?;
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
        .map(|session| listed_state(session).chars().count())
        .chain(std::iter::once(5))
        .max()
        .unwrap_or(5);
    println!("{:id_width$}  {:state_width$}  TITLE", "ID", "STATE");
    for session in &list.sessions {
        println!(
            "{:id_width$}  {:state_width$}  {}",
            session.id,
            listed_state(session),
            sanitize_terminal_text(&session.title)
        );
        println!(
            "  target {}  host {}  profile {}",
            sanitize_terminal_text(&session.target_id),
            sanitize_terminal_text(host_of_target(&rows, &session.target_id).unwrap_or("-")),
            sanitize_terminal_text(&session.profile_id)
        );
        if let Some(error) = &session.profile_login_error {
            println!(
                "  profile {}: {}",
                sanitize_terminal_text(&session.profile_id),
                sanitize_terminal_text(error)
            );
        }
        if session.attention == mj_core::attention::SessionAttention::Failed {
            println!(
                "  error details: run `mj sessions --session {}`",
                session.id
            );
        }
    }
    Ok(())
}

/// The host `mj hosts` lists `target_id` under; a fleet's own name for its
/// instances.
fn host_of_target<'a>(
    rows: &'a [mj_core::targets::HostCapacityRow],
    target_id: &str,
) -> Option<&'a str> {
    rows.iter()
        .find(|row| row.target_ids.iter().any(|id| id == target_id))
        .map(|row| row.host.as_str())
}

/// Keep the sessions on `target` and `host`. A name `mj hosts` does not know
/// is an error that lists the names it does, not an empty list.
fn select_sessions(
    sessions: Vec<ApiSession>,
    rows: &[mj_core::targets::HostCapacityRow],
    target: Option<&str>,
    host: Option<&str>,
) -> Result<Vec<ApiSession>> {
    let known = |names: Vec<&str>| {
        let names: std::collections::BTreeSet<_> = names.into_iter().collect();
        let joined = names.into_iter().collect::<Vec<_>>().join(", ");
        sanitize_terminal_text(&joined)
    };
    if let Some(target) = target
        && !rows
            .iter()
            .any(|row| row.target_ids.iter().any(|id| id == target))
    {
        let names = rows
            .iter()
            .flat_map(|row| row.target_ids.iter().map(String::as_str));
        bail!(
            "no session matched: no target {target:?}; known targets: {}",
            known(names.collect())
        );
    }
    if let Some(host) = host
        && !rows.iter().any(|row| row.host == host)
    {
        bail!(
            "no session matched: no host {host:?}; known hosts: {}",
            known(rows.iter().map(|row| row.host.as_str()).collect())
        );
    }
    Ok(sessions
        .into_iter()
        .filter(|session| target.is_none_or(|target| session.target_id == target))
        .filter(|session| {
            host.is_none_or(|host| host_of_target(rows, &session.target_id) == Some(host))
        })
        .collect())
}

/// The STATE column: the session's state, and "disk full" when its target
/// cannot take writes. `--session` prints the whole reason.
fn listed_state(session: &ApiSession) -> String {
    use mj_core::attention::SessionAttention;
    let attention = match session.attention {
        SessionAttention::NeedsInput => Some("needs input"),
        SessionAttention::Important => Some("important"),
        SessionAttention::Failed => Some("failed"),
        SessionAttention::Unreachable => Some("unreachable"),
        _ => None,
    };
    let mut state = session.state.clone();
    if let Some(attention) = attention {
        state.push_str(&format!(" ({attention})"));
    }
    if session.storage_problem.is_some() {
        state.push_str(" (disk full)");
    }
    if session.profile_login_error.is_some() {
        state.push_str(" (login required)");
    }
    state
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
    print_lines(wiki_session_lines(&info));
    Ok(())
}

/// What `mj sessions --session` prints for a SessionWiki row.
fn wiki_session_lines(info: &mj_client::daemon::WikiSessionInfo) -> Vec<String> {
    let status = match info.status {
        WikiSessionStatus::Mine => "mine",
        WikiSessionStatus::Archived => "archived",
        WikiSessionStatus::Native => "native",
    };
    let mut lines = vec![
        format!("{}  {status}  {}", info.wiki_id, info.title),
        format!("{} at {}", info.tool, info.path.display()),
    ];
    if let Some(session_id) = &info.mjolnir_session_id {
        lines.push(format!("Mjolnir session {session_id}"));
    }
    lines.push(match info.nothing_to_restore {
        true => "it cannot be continued: it never received a prompt".to_owned(),
        false => format!("continue it with `mj resume --wiki {}`", info.wiki_id),
    });
    lines
}

/// Save a recovery copy and release the environment. Acceptance is not completion.
pub(crate) async fn suspend(args: SuspendArgs) -> Result<()> {
    let session = suspend_session_id(&args)?;
    let client = ApiClient::connect().await?;
    if args.dry_run {
        let preflight = client
            .suspend_preflight(session, true, args.without_checkpoint)
            .await?;
        if args.json {
            return print_json(&preflight);
        }
        print!("{}", format_suspend_dry_run(&preflight));
        return Ok(());
    }
    if args.without_checkpoint {
        let preflight = client.suspend_preflight(session, false, true).await?;
        let has_verified = preflight
            .existing_checkpoint
            .as_ref()
            .is_some_and(|cp| cp.verified);
        if !has_verified && !args.yes {
            bail!(
                "Session {session} has no verified checkpoint, so suspending without a checkpoint cannot be resumed. The session is still running; pass `--yes` to confirm stopping {session} with no resumable checkpoint."
            );
        }
    }
    let consent = mj_core::suspend_consent::SuspendConsent {
        discard_untracked: if args.discard_untracked {
            mj_core::suspend_consent::UntrackedConsent::Any
        } else {
            mj_core::suspend_consent::UntrackedConsent::None
        },
        abort_operations: if args.abort_operation {
            mj_core::suspend_consent::OperationConsent::Any
        } else {
            mj_core::suspend_consent::OperationConsent::None
        },
        without_checkpoint: args.without_checkpoint,
        confirm_no_checkpoint: args.yes,
    };
    let accepted = client
        .suspend(session, &consent)
        .await
        .map_err(name_suspend_flags)?;
    if args.json {
        print_json(&serde_json::json!({
            "session_id": session,
            "accepted": true,
            "operation": "suspend",
            "stopped_subagents": accepted.stopped_subagents,
            "subagents_not_handed_back": accepted.subagents_not_handed_back,
            "warning": accepted.warning,
        }))
    } else {
        if let Some(warning) = &accepted.warning {
            eprintln!("warning: {warning}");
        }
        println!("suspension accepted for {session}");
        println!("`mj wait --session {session}` returns once the session is suspended");
        Ok(())
    }
}

/// Remove legacy untracked checkpoint payloads from stopped sessions.
#[derive(Debug, Args)]
pub(crate) struct PurgeUntrackedArgs {
    /// Session whose legacy untracked checkpoint payloads should be removed.
    #[arg(long, required_unless_present = "all", conflicts_with = "all")]
    session: Option<String>,
    /// Purge every session the daemon holds.
    #[arg(long)]
    all: bool,
    /// Report on-disk bytes that would be reclaimed without rewriting archives.
    #[arg(long)]
    dry_run: bool,
    /// Print the response as JSON.
    #[arg(long)]
    json: bool,
}

pub(crate) async fn purge_untracked(args: PurgeUntrackedArgs) -> Result<()> {
    let response = ApiClient::connect()
        .await?
        .purge_untracked(&PurgeUntrackedRequest {
            session_id: args.session,
            all: args.all,
            dry_run: args.dry_run,
        })
        .await?;
    let (report, status) = purge_untracked_conclusion(&response, args.json);
    if !report.is_empty() {
        if args.json {
            println!("{report}");
        } else {
            print_lines(report.lines().map(str::to_owned));
        }
    }
    status
}

/// The text or JSON of a purge report, and whether the process should fail.
///
/// A batch that rewrote some sessions and failed others still produces the
/// whole report. The error comes after that report so a script sees every
/// row and a non-zero status.
fn purge_untracked_conclusion(
    response: &PurgeUntrackedResponse,
    json: bool,
) -> (String, Result<()>) {
    let report = if json {
        match serde_json::to_string_pretty(response) {
            Ok(text) => text,
            Err(error) => {
                return (
                    String::new(),
                    Err(error).context("serialize the API response"),
                );
            }
        }
    } else {
        purge_untracked_lines(response).join("\n")
    };
    (report, refuse_failed_purge(response))
}

fn refuse_failed_purge(response: &PurgeUntrackedResponse) -> Result<()> {
    let failed = response
        .sessions
        .iter()
        .filter(|session| session.status == PurgeUntrackedStatus::Failed)
        .count();
    if failed > 0 {
        bail!("{failed} session(s) failed to purge untracked payloads");
    }
    Ok(())
}

/// One line per session, then the total. A dry run says what it would reclaim.
pub(crate) fn purge_untracked_lines(response: &PurgeUntrackedResponse) -> Vec<String> {
    let mut lines = Vec::new();
    for session in &response.sessions {
        lines.push(match session.status {
            PurgeUntrackedStatus::Purged => {
                format!(
                    "{}: {} reclaimed",
                    session.session_id, session.bytes_reclaimed
                )
            }
            PurgeUntrackedStatus::DryRun => {
                format!(
                    "{}: would reclaim {}",
                    session.session_id, session.bytes_reclaimed
                )
            }
            PurgeUntrackedStatus::Skipped => format!(
                "{}: skipped ({})",
                session.session_id,
                session.reason.as_deref().unwrap_or("skipped")
            ),
            PurgeUntrackedStatus::Failed => format!(
                "{}: failed ({})",
                session.session_id,
                session.reason.as_deref().unwrap_or("failed")
            ),
        });
        for note in &session.stash_untracked_notes {
            lines.push(format!(
                "  stash {} {} ({})",
                note.repository_id, note.commit, note.message
            ));
        }
        for warning in &session.warnings {
            lines.push(format!("  warning: {warning}"));
        }
    }
    lines.push(if response.dry_run {
        format!(
            "Total: {} would be reclaimed",
            response.total_bytes_reclaimed
        )
    } else {
        format!("Total: {} reclaimed", response.total_bytes_reclaimed)
    });
    lines
}

fn format_human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut size = bytes as f64;
    let mut unit = 0;
    while size >= 1024.0 && unit + 1 < UNITS.len() {
        size /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{size:.1} {}", UNITS[unit])
    }
}

pub(crate) fn format_suspend_dry_run(
    preflight: &mj_core::suspend_consent::SuspendPreflight,
) -> String {
    use std::fmt::Write;
    let mut out = String::new();
    let _ = writeln!(out, "dry run for session {}", preflight.session_id);
    if let Some(warning) = mj_core::suspend_consent::owning_children_suspend_warning(
        preflight.owning_children_to_suspend,
    ) {
        let _ = writeln!(out, "{warning}");
    }
    if let Some(est) = &preflight.dry_run {
        match (est.git_delta_bytes, &est.git_delta_unavailable_reason) {
            (Some(bytes), _) => {
                let _ = writeln!(out, "git delta: {}", format_human_bytes(bytes));
            }
            (None, Some(reason)) => {
                let _ = writeln!(out, "git delta: {reason}");
            }
            (None, None) => {
                let _ = writeln!(out, "git delta: unavailable");
            }
        }
        match est.session_state_bytes {
            Some(bytes) => {
                let _ = writeln!(out, "session state: {}", format_human_bytes(bytes));
            }
            None => {
                let _ = writeln!(out, "session state: unavailable");
            }
        }
        // Suspend saves only Git state, so never present the keep directory's
        // size as kept here (#1645).
        if est.keep_dir_bytes == 0 {
            let _ = writeln!(out, ".mj/keep/: 0 B");
        } else {
            let _ = writeln!(
                out,
                ".mj/keep/: {}, not saved by suspend; copy out anything you need first, for example with `mj fetch`",
                format_human_bytes(est.keep_dir_bytes)
            );
        }
        if let Some(route) = &est.transfer_route {
            let _ = writeln!(out, "transfer route: {route}");
        }
    }
    if !preflight.blockers.is_empty() {
        let _ = writeln!(out, "blockers:");
        for b in &preflight.blockers {
            let _ = writeln!(out, "  - {}: {}", b.kind, b.remedy);
        }
    }
    if !preflight.operations.is_empty() {
        let _ = writeln!(out, "in-progress operations (would be aborted):");
        for op in &preflight.operations {
            let _ = writeln!(out, "  - {} ({})", op.kind, op.checkout.repository_id);
        }
    }
    let lost = preflight.untracked_display_paths();
    if !lost.is_empty() {
        let _ = writeln!(out, "would be lost (ignored files are never kept):");
        for path in &lost {
            let _ = writeln!(out, "  - {path}");
        }
    }
    out
}

/// Permanently destroy the session, environment, and recovery archive.
pub(crate) async fn destroy(args: DestroyArgs) -> Result<()> {
    ApiClient::connect().await?.destroy(&args.session).await?;
    if args.json {
        print_json(
            &serde_json::json!({"session_id": args.session, "accepted": true, "operation": "destroy", "notices_url": format!("/api/v1/sessions/{}/notices", args.session)}),
        )
    } else {
        println!("destruction accepted for {}", args.session);
        println!(
            "it finishes in the background; check removal with `mj sessions --session {}`",
            args.session
        );
        println!(
            "read cleanup warnings with `mj notices --session {}`",
            args.session
        );
        Ok(())
    }
}

/// Which session this suspension is for, or a message naming the option that says
/// so.
fn suspend_session_id(args: &SuspendArgs) -> Result<&str> {
    match (args.session.as_deref(), args.misplaced_session.as_deref()) {
        (Some(session), None) => Ok(session),
        (None, Some(session)) => {
            bail!("name the session as an option: `mj suspend --session {session}`")
        }
        (Some(_), Some(_)) => bail!("name the session once, with --session"),
        (None, None) => bail!("name the session to suspend with --session <id>"),
    }
}

/// Resume a suspended session from its checkpoint.
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
    let response = resume_with_warnings(
        &client,
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

async fn resume_with_warnings(
    client: &ApiClient,
    session_id: &str,
    request: &ResumeSessionRequest,
) -> Result<mj_daemon::server::api::ResumeSessionResponse> {
    let mut warnings = crate::LaunchWarnings::new().await;
    let response = client.resume(session_id, request).await?;
    warnings.report(session_id).await;
    Ok(response)
}

/// Continue the session a SessionWiki id names, whoever ran it.
///
/// The branch is the daemon's: a Mjolnir session Mjolnir still has a record of
/// is resumed, one it has archived is restored from the indexed transcript,
/// and another tool's session is imported and then resumed. The output says
/// which of the three ran, so the caller can follow with `mj prompt` or
/// `mj message`.
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
            let response = resume_with_warnings(
                &client,
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
            let mut warnings = crate::LaunchWarnings::new().await;
            let response = client
                .wiki_restore(
                    &wiki_id,
                    &mj_daemon::server::api::WikiRestoreBody {
                        workspace_id: args.workspace_id.clone(),
                        profile_id,
                        target_id,
                        project_directory: None,
                        model: None,
                        effort: None,
                    },
                )
                .await?;
            warnings.report(&response.session_id).await;
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
                None => crate::resolve_store_workspace(args.workspace.name.as_deref()).await?,
            };
            let indexed_at = info.path.clone();
            let imported = {
                let native_session_id = native_session_id.clone();
                let tool = info.tool.clone();
                mj_core::runtime::spawn_blocking(move || {
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
            let response = resume_with_warnings(
                &client,
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

pub(crate) async fn interrupt_turn(args: SessionArgs) -> Result<()> {
    let client = ApiClient::connect().await?;
    client.interrupt_turn(&args.session).await?;
    match args.json {
        true => print_json(&serde_json::json!({ "session_id": args.session, "accepted": true })),
        false => {
            println!("cancelling the turn on {}", args.session);
            Ok(())
        }
    }
}

pub(crate) async fn restart_session(args: SessionArgs) -> Result<()> {
    let client = ApiClient::connect().await?;
    let payload = client.restart_session(&args.session).await?;
    match args.json {
        true => print_json(&payload),
        false => {
            println!("restarting agent for {}", args.session);
            Ok(())
        }
    }
}

#[derive(Debug, Args)]
pub(crate) struct ReviewArgs {
    #[command(subcommand)]
    command: ReviewCommand,
}

#[derive(Debug, clap::Subcommand)]
enum ReviewCommand {
    /// Review the turn the session just finished, with the session's reviewer.
    /// The session must be idle with no queued prompts.
    Start(SessionArgs),
    /// Show the review the session has open: what it is doing, each reviewing
    /// role, and its verdict once it has one.
    Status(SessionArgs),
    /// Show a finished review's per-reviewer output: each reviewer's model,
    /// effort and profile, its full handback, its verdict, and its usage.
    /// Works after the reviewer sessions have been destroyed.
    Show(ReviewShowArgs),
}

#[derive(Debug, Args)]
pub(crate) struct ReviewShowArgs {
    #[command(flatten)]
    session: SessionArgs,
    /// Review id, as a verdict message names it. Defaults to the newest
    /// finished review for the session.
    #[arg(long)]
    review: Option<String>,
}

pub(crate) async fn review(args: ReviewArgs) -> Result<()> {
    let client = ApiClient::connect().await?;
    match args.command {
        ReviewCommand::Start(args) => {
            let started = client.start_review(&args.session).await?;
            if args.json {
                return print_json(&started);
            }
            println!("review started on {}", args.session);
            println!(
                "follow it with `mj review status --session {}`",
                args.session
            );
            Ok(())
        }
        ReviewCommand::Status(args) => {
            let status = client.review_status(&args.session).await?;
            if args.json {
                return print_json(&status);
            }
            println!("Review trigger: {}", status.trigger);
            print_lines(review_lines(status.review.as_ref()));
            Ok(())
        }
        ReviewCommand::Show(args) => {
            let response = client
                .finished_review(&args.session.session, args.review.as_deref())
                .await?;
            if args.session.json {
                return print_json(&response);
            }
            print_lines(finished_review_lines(&response));
            Ok(())
        }
    }
}

/// A review as plain lines: its status, one line per role, then the verdict
/// and its text.
fn review_lines(review: Option<&mj_daemon::server::ViewerTurnReview>) -> Vec<String> {
    let Some(review) = review else {
        return vec!["no review is open".to_owned()];
    };
    let mut lines = vec![format!("review: {}", review.status)];
    for role in &review.roles {
        lines.push(format!("  {}: {}", role.label, role.state));
    }
    if let Some(verdict) = &review.verdict {
        lines.push(format!("verdict: {}", verdict.kind));
        if !verdict.text.is_empty() {
            lines.push(String::new());
            lines.push(verdict.text.clone());
        }
    }
    lines
}

/// A finished review as plain lines, including each reviewer's full handback
/// and the usage its deleted child session still accounts for.
fn finished_review_lines(response: &mj_daemon::server::api::FinishedReviewResponse) -> Vec<String> {
    let Some(review) = &response.review else {
        return vec![format!(
            "no finished review is recorded for {}",
            response.session_id
        )];
    };
    let resolution = match review.resolution {
        mj_controller::database::FinishedReviewResolution::Completed => "completed",
        mj_controller::database::FinishedReviewResolution::Failed => "failed",
        mj_controller::database::FinishedReviewResolution::Cancelled => "cancelled",
    };
    let finished = chrono::DateTime::from_timestamp_millis(review.finished_at_ms)
        .map(|time| time.to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
        .unwrap_or_else(|| format!("{} ms", review.finished_at_ms));
    let mut lines = vec![format!(
        "review {}: {resolution}, finished {finished}",
        review.review_id
    )];
    for role in &review.roles {
        lines.push(format!(
            "  {} ({}) profile {}; model {}; effort {}",
            role.label,
            role.role,
            if role.profile.is_empty() {
                "unknown"
            } else {
                &role.profile
            },
            role.model.as_deref().unwrap_or("harness default"),
            role.effort.as_deref().unwrap_or("default effort"),
        ));
        match role.verdict.as_ref() {
            Some(mj_core::review::verdict::ReviewVerdict::Clean) => {
                lines.push("    verdict: clean".to_owned());
            }
            Some(mj_core::review::verdict::ReviewVerdict::Findings { synthesis, .. }) => {
                lines.push("    verdict: findings".to_owned());
                for line in synthesis.lines() {
                    lines.push(format!("      {line}"));
                }
            }
            Some(mj_core::review::verdict::ReviewVerdict::Failed { reason }) => {
                lines.push(format!("    verdict: failed: {reason}"));
            }
            None => lines.push("    verdict: none (the reviewer did not settle)".to_owned()),
        }
        if let Some(turn) = &role.turn {
            let result = turn.result();
            let kind = match result.kind {
                mj_core::event_outcome::TurnResultKind::Completed => "completed",
                mj_core::event_outcome::TurnResultKind::InputRequired => "input required",
                mj_core::event_outcome::TurnResultKind::Cancelled => "cancelled",
                mj_core::event_outcome::TurnResultKind::Rejected => "rejected",
                mj_core::event_outcome::TurnResultKind::Interrupted => "interrupted",
                mj_core::event_outcome::TurnResultKind::Failed => "failed",
            };
            let detail = result
                .message
                .as_deref()
                .or(result.stop_reason.as_deref())
                .map(|detail| format!(": {detail}"))
                .unwrap_or_default();
            lines.push(format!("    turn: {kind}{detail}"));
            if let Some(usage) = &turn.usage {
                let mut counters = vec![
                    format!("total {}", usage.total_tokens),
                    format!("input {}", usage.input_tokens),
                    format!("output {}", usage.output_tokens),
                ];
                for (name, value) in [
                    ("thought", usage.thought_tokens),
                    ("cached read", usage.cached_read_tokens),
                    ("cached write", usage.cached_write_tokens),
                ] {
                    if let Some(value) = value {
                        counters.push(format!("{name} {value}"));
                    }
                }
                lines.push(format!("    usage: {}", counters.join(", ")));
            }
        }
        if let Some(handback) = &role.handback {
            lines.push("    handback:".to_owned());
            if handback.is_empty() {
                lines.push("      (empty)".to_owned());
            }
            for line in handback.lines() {
                lines.push(format!("      {line}"));
            }
        }
    }
    lines
}

/// Print the API location without exposing credentials.
pub(crate) async fn api_info(args: ApiInfoArgs) -> Result<()> {
    let client = ApiClient::connect().await?;
    if args.json {
        return print_json(&serde_json::json!({
            "base_url": format!("{}/api/v1", client.base_url()),
        }));
    }
    println!("base url   {}/api/v1", client.base_url());
    println!("version    Mjolnir API 1");
    Ok(())
}

/// Explicit credential output for callers authenticating outside the CLI.
pub(crate) async fn api_token() -> Result<()> {
    let mut client = crate::daemon::connect_or_start().await?;
    // The daemon sets the credential before it publishes viewer readiness;
    // a daemon this command just started may not have reached that point.
    crate::daemon::wait_for_web_access(&mut client).await?;
    println!("{}", client.api_credential().await?);
    Ok(())
}

/// Print a valid GitHub App installation token for a selected owner or repo.
pub(crate) async fn github_token(args: GithubTokenArgs) -> Result<()> {
    let (owner, repositories) = match (args.owner, args.repo) {
        (Some(owner), repositories) if repositories.is_empty() => (Some(owner), Vec::new()),
        (None, repositories) if !repositories.is_empty() => {
            for repository in &repositories {
                let parsed = mj_core::remote_git::github_owner_repo(repository)
                    .context("--repo values must use OWNER/NAME form")?;
                ensure!(
                    format!("{}/{}", parsed.0, parsed.1) == repository.as_str(),
                    "--repo values must use OWNER/NAME form"
                );
            }
            (None, repositories)
        }
        _ => bail!("supply exactly one of --owner or one or more --repo values"),
    };
    let token = ApiClient::connect()
        .await?
        .github_token(owner.as_deref(), &repositories)
        .await?;
    println!("{token}");
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
/// Writes an export to `out`, or to standard output when no file is named.
/// With `--json` and a file, the confirmation is one JSON object rather than
/// a sentence; without a file the bytes are the output and stay unwrapped.
fn write_bytes(
    bytes: &[u8],
    out: Option<&std::path::Path>,
    json: bool,
    format: &str,
) -> Result<()> {
    write_bytes_to(&mut std::io::stdout().lock(), bytes, out, json, format)
}

fn write_bytes_to(
    stdout: &mut impl Write,
    bytes: &[u8],
    out: Option<&std::path::Path>,
    json: bool,
    format: &str,
) -> Result<()> {
    match out {
        Some(path) => {
            std::fs::write(path, bytes).with_context(|| format!("write {}", path.display()))?;
            if json {
                let report = serde_json::json!({
                    "path": path,
                    "bytes": bytes.len(),
                    "format": format,
                });
                writeln!(stdout, "{}", serde_json::to_string_pretty(&report)?)
                    .context("report the export")
            } else {
                writeln!(stdout, "wrote {} bytes to {}", bytes.len(), path.display())
                    .context("report the export")
            }
        }
        None => {
            stdout.write_all(bytes).context("write the export")?;
            stdout.flush().context("write the export")
        }
    }
}

/// Print report lines built from daemon answers. Titles, notices and other
/// text in them can come from an agent, so terminal controls are removed here,
/// where that text reaches the operator's terminal.
fn print_lines(lines: impl IntoIterator<Item = String>) {
    for line in lines {
        println!("{}", sanitize_terminal_text(&line));
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
    /// Profile whose harness to ask. Omitted lists the models every enabled
    /// profile offers, with the profiles supporting each.
    #[arg(long)]
    profile: Option<String>,
    /// List the efforts this model offers instead of the default model's.
    #[arg(long, requires = "profile")]
    model: Option<String>,
    /// List every model the harness advertises, ignoring the profile's
    /// `models` allow-list. Use it to find the exact values to put there.
    #[arg(long, conflicts_with = "model")]
    unrestricted: bool,
    /// With no --profile, include disabled profiles in the aggregate.
    #[arg(long)]
    with_disabled: bool,
    /// Print the response as JSON instead of text.
    #[arg(long)]
    json: bool,
}
#[derive(Debug, Args)]
pub(crate) struct SetConfigArgs {
    /// Session id, as `mj sessions` lists it.
    #[arg(long)]
    session: String,
    /// Setting to change, such as `model` or `effort`. Omit it and --value to
    /// list the settings this session's agent offers, with their choices.
    #[arg(long, requires = "value")]
    key: Option<String>,
    /// Value to set: one of the choices the setting lists.
    #[arg(long, requires = "key")]
    value: Option<String>,
    /// Print the response as JSON instead of text.
    #[arg(long)]
    json: bool,
}
pub(crate) async fn models(args: ModelsArgs) -> Result<()> {
    let mut daemon = crate::daemon::connect_or_start().await?;
    let mj_client::runtime_feed::RuntimeFrame::Snapshot { projection, .. } =
        daemon.runtime_changes(None, false).await?
    else {
        bail!("daemon did not return a runtime snapshot for cached quota reports");
    };
    let client = ApiClient::connect().await?;
    let Some(profile) = args.profile.as_deref() else {
        let config = projection.metadata.installed_config();
        let ids: Vec<String> = config
            .profiles
            .iter()
            .filter(|(_, profile)| args.with_disabled || profile.enabled)
            .map(|(id, _)| id.clone())
            .collect();
        let results = futures::future::join_all(ids.iter().map(|id| {
            let client = &client;
            async move {
                (
                    id.clone(),
                    client
                        .models(id, None, args.unrestricted, args.with_disabled)
                        .await,
                )
            }
        }))
        .await;
        let rendered = render_models_aggregate(&results, args.json)?;
        println!("{rendered}");
        return Ok(());
    };
    let quota = projection.metadata.quotas.reports.get(profile);
    let billing = quota
        .map(mj_client::quota::ProfileQuota::billing_class)
        .unwrap_or(mj_client::quota::QuotaBilling::Unknown);
    let choices = client
        .models(profile, args.model, args.unrestricted, args.with_disabled)
        .await?;
    let rendered = render_models(&choices, billing, args.json)?;
    println!("{rendered}");
    Ok(())
}

/// The aggregated `mj models` view: one row per normalized model identity,
/// naming every profile that offers it. Provider qualifiers fold away, so
/// `openai/gpt-5.3-codex` and `gpt-5.3-codex` share a row.
fn render_models_aggregate(
    results: &[(String, Result<mj_core::worker_launch::ProfileConfig>)],
    json: bool,
) -> Result<String> {
    let mut by_model: std::collections::BTreeMap<String, (String, Vec<String>)> =
        Default::default();
    let mut unavailable: Vec<(String, String)> = Vec::new();
    for (profile, result) in results {
        match result {
            Ok(choices) => {
                for model in &choices.models {
                    let entry = by_model
                        .entry(mj_core::model_catalog::normalize_model(&model.value))
                        .or_insert_with(|| (model.name.clone(), Vec::new()));
                    entry.1.push(profile.clone());
                }
            }
            Err(error) => unavailable.push((profile.clone(), format!("{error:#}"))),
        }
    }
    if json {
        return serde_json::to_string_pretty(&serde_json::json!({
            "models": by_model
                .iter()
                .map(|(value, (name, profiles))| serde_json::json!({
                    "value": value,
                    "name": name,
                    "profiles": profiles,
                }))
                .collect::<Vec<_>>(),
            "unavailable": unavailable
                .iter()
                .map(|(profile, error)| (profile.clone(), error.clone()))
                .collect::<std::collections::BTreeMap<_, _>>(),
        }))
        .context("serialize the aggregated model catalog");
    }
    let mut lines: Vec<String> = by_model
        .iter()
        .map(|(value, (name, profiles))| format!("{value}  {name}: {}", profiles.join(", ")))
        .collect();
    if by_model.is_empty() {
        lines.push("no profile reports any models".to_owned());
    }
    for (profile, error) in &unavailable {
        lines.push(format!("{profile}: {error}"));
    }
    Ok(lines.join("\n"))
}

fn render_models(
    choices: &mj_core::worker_launch::ProfileConfig,
    billing: mj_client::quota::QuotaBilling,
    json: bool,
) -> Result<String> {
    let rates = |model: &str| {
        (billing == mj_client::quota::QuotaBilling::Usage)
            .then(|| mj_core::usage::pricing::rates(model))
            .flatten()
    };
    if json {
        let mut value = serde_json::to_value(choices).context("serialize model catalog")?;
        value["billing"] = serde_json::Value::String(
            match billing {
                mj_client::quota::QuotaBilling::Usage => "usage",
                mj_client::quota::QuotaBilling::Subscription => "subscription",
                mj_client::quota::QuotaBilling::Unknown => "unknown",
            }
            .to_owned(),
        );
        value["models"] = serde_json::Value::Array(
            choices
                .models
                .iter()
                .map(|model| {
                    let mut item = serde_json::to_value(model).expect("model choice serializes");
                    item["price"] = rates(&model.value)
                        .map(|r| {
                            serde_json::json!({
                                "input": r[0] as f64 / 1000.0,
                                "output": r[1] as f64 / 1000.0,
                                "cached_input": r[2] as f64 / 1000.0,
                                "cache_write": r[3] as f64 / 1000.0
                            })
                        })
                        .unwrap_or(serde_json::Value::Null);
                    item
                })
                .collect(),
        );
        return serde_json::to_string_pretty(&value).context("serialize model catalog");
    }
    let mut lines = Vec::new();
    if billing != mj_client::quota::QuotaBilling::Usage {
        lines.push(format!(
            "billing: {}",
            match billing {
                mj_client::quota::QuotaBilling::Subscription => "subscription",
                _ => "unknown",
            }
        ));
    }
    if choices.models.is_empty() && choices.efforts.is_empty() {
        lines.push("this profile's harness reports no models or efforts".to_owned());
    }
    for model in &choices.models {
        let price = rates(&model.value)
            .map(|r| {
                format!(
                    "  ${:.3} in / ${:.3} cached / ${:.3} out per 1M",
                    r[0] as f64 / 1000.0,
                    r[2] as f64 / 1000.0,
                    r[1] as f64 / 1000.0
                )
            })
            .unwrap_or_default();
        lines.push(format!("{}  {}{}", model.value, model.name, price));
    }
    // A harness without efforts, or one whose efforts are not known yet,
    // printed a bare "effort (default):" line, which reads as a value that
    // failed to print (F-16).
    if !choices.efforts.is_empty() {
        lines.push(format!(
            "effort ({}): {}",
            choices.model.as_deref().unwrap_or("default model"),
            choices
                .efforts
                .iter()
                .map(|c| c.value.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    Ok(lines.join("\n"))
}

#[derive(Debug, Args)]
pub(crate) struct QuotaArgs {
    /// Show only this profile; default lists every enabled profile.
    #[arg(long)]
    profile: Option<String>,
    /// Ask the daemon to probe again, and wait for the fresh cycle's readings.
    #[arg(long)]
    refresh: bool,
    /// Include disabled profiles, probing once for any without a report.
    #[arg(long)]
    with_disabled: bool,
    /// Print the response as JSON instead of text.
    #[arg(long)]
    json: bool,
}

/// How long `mj quota --refresh` waits for the cycle it asked for before
/// printing what the daemon already has. Quota probes launch each profile's
/// agent, so a fresh cycle can take a while on a loaded host.
const QUOTA_REFRESH_WAIT: std::time::Duration = std::time::Duration::from_secs(120);

/// Pump runtime deltas until `done` holds or the deadline passes; returns the
/// latest cursor and metadata seen. One waiter feeds `--refresh` (a new
/// cycle) and `--with-disabled` (a report for every profile just probed).
/// Own a dedicated connection: a deadline abandons its in-flight exchange,
/// while quota control requests continue on their original connection.
async fn pump_quota_feed(
    mut daemon: mj_client::daemon::DaemonClient,
    mut cursor: Option<mj_client::runtime_feed::RuntimeCursor>,
    mut metadata: mj_client::runtime_feed::RuntimeMetadata,
    deadline: std::time::Instant,
    done: impl Fn(&mj_client::runtime_feed::RuntimeMetadata) -> bool,
) -> Result<(
    Option<mj_client::runtime_feed::RuntimeCursor>,
    mj_client::runtime_feed::RuntimeMetadata,
)> {
    use mj_client::runtime_feed::RuntimeFrame;
    while !done(&metadata) {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        let Ok(frame) =
            tokio::time::timeout(remaining, daemon.runtime_changes(cursor.clone(), true)).await
        else {
            break;
        };
        match frame? {
            RuntimeFrame::Snapshot {
                cursor: next,
                projection,
            } => {
                cursor = Some(next);
                metadata = projection.metadata;
            }
            RuntimeFrame::Delta {
                cursor: next,
                changes,
                ..
            } => {
                cursor = Some(next);
                if let Some(changed) = changes.metadata {
                    metadata = changed;
                }
            }
            RuntimeFrame::ResetRequired => cursor = None,
        }
    }
    Ok((cursor, metadata))
}

pub(crate) async fn quota(args: QuotaArgs) -> Result<()> {
    let mut daemon = crate::daemon::connect_or_start().await?;
    let mj_client::runtime_feed::RuntimeFrame::Snapshot {
        cursor: first,
        projection,
    } = daemon.runtime_changes(None, false).await?
    else {
        bail!("daemon did not return a runtime snapshot for quota reports");
    };
    let mut cursor = Some(first);
    let mut metadata = projection.metadata;
    if args.refresh {
        // The daemon coalesces this with any probe already running; the
        // cycle count advancing past the request's is the refresh done.
        let pending = metadata.quotas.cycles;
        daemon.refresh_quota().await?;
        let deadline = std::time::Instant::now() + QUOTA_REFRESH_WAIT;
        let pumped = pump_quota_feed(
            mj_client::daemon::connect_existing().await?,
            cursor,
            metadata,
            deadline,
            |metadata| metadata.quotas.cycles > pending,
        )
        .await?;
        cursor = pumped.0;
        metadata = pumped.1;
    }
    if args.with_disabled {
        let config = metadata.installed_config();
        // A disabled profile with a served report shows it; one without gets
        // a single daemon probe, and this waits for the answer.
        let probe: Vec<String> = config
            .profiles
            .iter()
            .filter(|(id, profile)| {
                !profile.enabled
                    && args.profile.as_deref().is_none_or(|wanted| wanted == *id)
                    && !metadata.quotas.reports.contains_key(*id)
            })
            .map(|(id, _)| id.clone())
            .collect();
        if !probe.is_empty() {
            daemon.probe_quota_profiles(probe.clone(), false).await?;
            let deadline = std::time::Instant::now() + QUOTA_REFRESH_WAIT;
            let pumped = pump_quota_feed(
                mj_client::daemon::connect_existing().await?,
                cursor,
                metadata,
                deadline,
                |metadata| {
                    probe
                        .iter()
                        .all(|id| metadata.quotas.reports.contains_key(id))
                },
            )
            .await?;
            metadata = pumped.1;
        }
    }
    let rendered = render_quota(
        &metadata,
        args.profile.as_deref(),
        args.with_disabled,
        args.json,
    )?;
    println!("{rendered}");
    Ok(())
}

/// One profile's row for `mj quota`, or the reason its quota cannot be read.
fn quota_row(
    id: &str,
    profile: &mj_core::config::HarnessProfile,
    snapshot: &mj_client::quota::QuotaSnapshot,
    now: u64,
) -> (String, serde_json::Value) {
    use mj_client::quota::{ProfileQuota, QuotaBilling};
    let report = snapshot.reports.get(id);
    let billing = ProfileQuota::billing_for(profile.kind, report);
    let billing_label = match billing {
        QuotaBilling::Usage => "usage",
        QuotaBilling::Subscription => "subscription",
        QuotaBilling::Unknown => "unknown",
    };
    let probing = snapshot.probing.contains(id);
    let mut entry = serde_json::json!({
        "profile": id,
        "harness": profile.kind.id(),
        "billing": billing_label,
        "enabled": profile.enabled,
        "probing": probing,
        "report": report,
    });
    // The quota text, and the reason a missing or failed reading shows.
    let status = match report {
        None if probing => "probing now".to_owned(),
        None => "no quota report yet".to_owned(),
        Some(report) => {
            if let Some(error) = report.error_label() {
                let held = report
                    .rate_limit_label(now)
                    .map(|label| format!(" ({label})"))
                    .unwrap_or_default();
                format!("error: {error}{held}")
            } else if let Some(label) = report.windowless_label() {
                label.to_owned()
            } else {
                report
                    .windows
                    .iter()
                    .map(|window| {
                        let usage = match (window.remaining_percent, window.used, window.limit) {
                            (Some(remaining), _, _) => format!("{remaining}% left"),
                            (_, Some(used), Some(limit)) => format!("{used}/{limit} used"),
                            _ => "available".to_owned(),
                        };
                        let reset =
                            window.reset_display(now, report.banked_resets_for_window(window));
                        let label = report.window_label(window);
                        if reset.is_empty() {
                            format!("{label} {usage}")
                        } else {
                            format!("{label} {usage}, resets {reset}")
                        }
                    })
                    .collect::<Vec<_>>()
                    .join(" · ")
            }
        }
    };
    let credits = match report.and_then(|report| report.paid_credits.as_ref()) {
        Some(mj_client::quota::PaidCredits::Limited(balance)) => format!("credits ${balance} left"),
        Some(mj_client::quota::PaidCredits::Unlimited) => "unlimited credits".into(),
        Some(mj_client::quota::PaidCredits::Available) => "credits available".into(),
        None => String::new(),
    };
    let status = [status, credits]
        .into_iter()
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(" · ");
    entry["detail"] = serde_json::Value::String(status.clone());
    let mut text = format!(
        "{id} ({}, {billing_label}{}): {status}",
        profile.kind.display_name(),
        if profile.enabled { "" } else { ", disabled" }
    );
    if let Some(report) = report {
        text.push_str(&format!(
            " · checked {} ago",
            quota_age(now, report.refreshed_at_epoch_seconds)
        ));
    }
    (text, entry)
}

/// A compact age for a reading: `45s`, `4m`, `2h`, `3d`.
fn quota_age(now: u64, epoch_seconds: u64) -> String {
    let age = now.saturating_sub(epoch_seconds);
    if age < 60 {
        format!("{age}s")
    } else if age < 3600 {
        format!("{}m", age / 60)
    } else if age < 24 * 3600 {
        format!("{}h", age / 3600)
    } else {
        format!("{}d", age / (24 * 3600))
    }
}

fn render_quota(
    metadata: &mj_client::runtime_feed::RuntimeMetadata,
    only: Option<&str>,
    with_disabled: bool,
    json: bool,
) -> Result<String> {
    render_quota_at(
        metadata,
        only,
        with_disabled,
        json,
        mj_core::clock::epoch_seconds(),
    )
}

fn render_quota_at(
    metadata: &mj_client::runtime_feed::RuntimeMetadata,
    only: Option<&str>,
    with_disabled: bool,
    json: bool,
    now: u64,
) -> Result<String> {
    let config = metadata.installed_config();
    let mut rows = Vec::new();
    for (id, profile) in &config.profiles {
        if !profile.enabled && !with_disabled {
            continue;
        }
        if only.is_some_and(|wanted| wanted != id) {
            continue;
        }
        rows.push(quota_row(id, profile, &metadata.quotas, now));
    }
    if let Some(wanted) = only
        && rows.is_empty()
    {
        if with_disabled {
            bail!("no profile {wanted:?}");
        }
        bail!("no enabled profile {wanted:?}");
    }
    if json {
        let value = serde_json::json!({
            "cycles": metadata.quotas.cycles,
            "profiles": rows.iter().map(|(_, entry)| entry.clone()).collect::<Vec<_>>(),
        });
        return serde_json::to_string_pretty(&value).context("serialize quota reports");
    }
    if rows.is_empty() {
        return Ok("no enabled profiles".to_owned());
    }
    Ok(rows
        .iter()
        .map(|(text, _)| text.clone())
        .collect::<Vec<_>>()
        .join("\n"))
}
#[derive(Debug, Args)]
pub(crate) struct HostsArgs {
    /// Show only this host or fleet, named by its host or by a target it
    /// contains; default lists every host.
    #[arg(long)]
    host: Option<String>,
    /// Print the response as JSON instead of text.
    #[arg(long)]
    json: bool,
}

/// Read the daemon capacity service's latest readings, shared with the
/// terminal dashboard and the web viewer.
pub(crate) async fn hosts(args: HostsArgs) -> Result<()> {
    let mut daemon = crate::daemon::connect_or_start().await?;
    let rows = daemon.host_capacity().await?;
    println!(
        "{}",
        render_hosts(
            &rows,
            args.host.as_deref(),
            args.json,
            mj_core::clock::epoch_seconds()
        )?
    );
    Ok(())
}

fn render_hosts(
    rows: &[mj_core::targets::HostCapacityRow],
    only: Option<&str>,
    json: bool,
    now: u64,
) -> Result<String> {
    let selected: Vec<_> = match only {
        None => rows.to_vec(),
        Some(name) => {
            let found: Vec<_> = rows
                .iter()
                .filter(|row| row.matches(name))
                .cloned()
                .collect();
            if found.is_empty() {
                let known: std::collections::BTreeSet<&str> = rows
                    .iter()
                    .flat_map(|row| {
                        std::iter::once(row.host.as_str())
                            .chain(row.target_ids.iter().map(String::as_str))
                    })
                    .collect();
                if known.is_empty() {
                    bail!("no host or target {name:?}: the daemon has no hosts");
                }
                bail!(
                    "no host or target {name:?}; known names: {}",
                    known.into_iter().collect::<Vec<_>>().join(", ")
                );
            }
            found
        }
    };
    if json {
        return serde_json::to_string_pretty(&serde_json::json!({
            "now_epoch_seconds": now,
            "hosts": selected,
        }))
        .context("serialize host capacity");
    }
    Ok(mj_core::targets::render_host_capacity(&selected, now))
}

pub(crate) async fn set_config(args: SetConfigArgs) -> Result<()> {
    let client = ApiClient::connect().await?;
    let session = match (args.key, args.value) {
        (Some(key), Some(value)) => {
            client
                .set_config(
                    &args.session,
                    &mj_daemon::server::api::SetConfigRequest { key, value },
                )
                .await?
        }
        // The settings are the agent's, so they differ by harness and model;
        // listing them is how a user learns which keys this command takes.
        (None, None) => client
            .session_if_known(&args.session)
            .await?
            .with_context(|| format!("no session {}", args.session))?,
        (Some(_), None) => bail!("pass --value with --key"),
        (None, Some(_)) => bail!("pass --key with --value"),
    };
    if args.json {
        return print_json(&session);
    }
    if session.config_options.is_empty() {
        println!("this session's agent offers no settings right now");
    }
    print_lines(config_option_lines(&session.config_options));
    Ok(())
}

/// One line per setting: its key, its current value, and what it accepts.
fn config_option_lines(options: &[mj_daemon::server::ViewerConfigOption]) -> Vec<String> {
    options
        .iter()
        .map(|option| {
            format!(
                "{}  {}  (choices: {})",
                option.key,
                option.current.as_deref().unwrap_or("unknown"),
                option
                    .choices
                    .iter()
                    .map(|choice| choice.value.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Cli, Command};
    use clap::Parser as _;

    #[cfg(unix)]
    #[test]
    fn fetch_ls_renders_a_real_directory_listing() {
        use mj_checkpoint::archive::{FetchMode, SessionListing, fetch_session_path};
        let root = tempfile::tempdir().unwrap();
        let probes = root.path().join("rust-veto-cf");
        std::fs::create_dir_all(probes.join("nested")).unwrap();
        std::fs::write(probes.join("report.md"), b"# Report\n").unwrap();
        std::fs::write(probes.join("probe.log"), vec![b'x'; 120_000]).unwrap();
        std::os::unix::fs::symlink("report.md", probes.join("latest")).unwrap();
        let at = |seconds| std::time::UNIX_EPOCH + std::time::Duration::from_secs(seconds);
        for (name, time) in [
            ("nested", at(1_792_656_000)),
            ("report.md", at(1_792_742_130)),
            ("probe.log", at(1_792_742_160)),
        ] {
            std::fs::File::open(probes.join(name))
                .unwrap()
                .set_modified(time)
                .unwrap();
        }
        let list = |path: &std::path::Path| -> SessionListing {
            let fetched = fetch_session_path(
                path,
                &[root.path().to_path_buf()],
                false,
                FetchMode::Listing,
                tempfile::tempfile().unwrap(),
            )
            .unwrap();
            serde_json::from_reader(fetched.reader()).unwrap()
        };
        let mut directory = list(&probes);
        // The standard library cannot set a symlink's own time, so the link's
        // creation time is replaced; everything else is what the target read.
        for entry in &mut directory.entries {
            if entry.name == "latest" {
                entry.modified = Some("2026-10-22T09:00:00Z".to_owned());
            }
        }
        let output = format!(
            "=== mj fetch --ls <directory> ===\n{}\n=== mj fetch --ls <file> ===\n{}",
            listing_lines(&directory).join("\n"),
            listing_lines(&list(&probes.join("probe.log"))).join("\n"),
        );
        mj_core::golden::assert_golden(env!("CARGO_MANIFEST_DIR"), "fetch-ls", &output);
    }

    #[test]
    fn github_token_requires_one_owner_or_repository_selector() {
        let Some(Command::GithubToken(owner)) =
            Cli::try_parse_from(["mj", "github-token", "--owner", "acme"])
                .unwrap()
                .command
        else {
            panic!("expected github-token command");
        };
        assert_eq!(owner.owner.as_deref(), Some("acme"));
        assert!(owner.repo.is_empty());

        let Some(Command::GithubToken(repo)) =
            Cli::try_parse_from(["mj", "github-token", "--repo", "acme/project"])
                .unwrap()
                .command
        else {
            panic!("expected github-token command");
        };
        assert_eq!(repo.owner, None);
        assert_eq!(repo.repo, ["acme/project"]);
        let Some(Command::GithubToken(repositories)) = Cli::try_parse_from([
            "mj",
            "github-token",
            "--repo",
            "acme/project",
            "--repo",
            "acme/tools",
        ])
        .unwrap()
        .command
        else {
            panic!("expected github-token command");
        };
        assert_eq!(repositories.repo, ["acme/project", "acme/tools"]);
        assert!(Cli::try_parse_from(["mj", "github-token"]).is_err());
        assert!(
            Cli::try_parse_from([
                "mj",
                "github-token",
                "--owner",
                "acme",
                "--repo",
                "acme/project",
            ])
            .is_err()
        );
    }

    #[test]
    fn review_parses_start_and_status_with_a_session() {
        for (verb, json) in [("start", false), ("status", true)] {
            let mut argv = vec!["mj", "review", verb, "--session", "s1"];
            if json {
                argv.push("--json");
            }
            let cli = Cli::try_parse_from(argv).expect("review parses");
            let Some(Command::Review(args)) = cli.command else {
                panic!("expected the review command");
            };
            let session = match args.command {
                ReviewCommand::Start(session) | ReviewCommand::Status(session) => session,
                ReviewCommand::Show(_) => panic!("expected start or status"),
            };
            assert_eq!(session.session, "s1");
            assert_eq!(session.json, json);
        }
        let cli = Cli::try_parse_from([
            "mj",
            "review",
            "show",
            "--session",
            "s1",
            "--review",
            "r1",
            "--json",
        ])
        .expect("review show parses");
        let Some(Command::Review(args)) = cli.command else {
            panic!("expected the review command");
        };
        let ReviewCommand::Show(args) = args.command else {
            panic!("expected review show");
        };
        assert_eq!(args.session.session, "s1");
        assert_eq!(args.review.as_deref(), Some("r1"));
        assert!(args.session.json);
        let cli = Cli::try_parse_from(["mj", "review", "show", "--session", "s1"])
            .expect("review show defaults to the latest finished review");
        let Some(Command::Review(args)) = cli.command else {
            panic!("expected the review command");
        };
        let ReviewCommand::Show(args) = args.command else {
            panic!("expected review show");
        };
        assert!(args.review.is_none());
        for verb in ["forward", "dismiss", "cancel"] {
            assert!(Cli::try_parse_from(["mj", "review", verb, "--session", "s1"]).is_err());
        }
        assert!(Cli::try_parse_from(["mj", "review", "start"]).is_err());
    }

    #[test]

    fn stop_task_requires_a_session_and_exactly_one_task_selection() {
        for argv in [
            vec!["mj", "stop-task", "--session", "s1"],
            vec![
                "mj",
                "stop-task",
                "--session",
                "s1",
                "terminal:one",
                "--all",
            ],
            vec!["mj", "stop-task", "--all"],
        ] {
            assert!(Cli::try_parse_from(argv).is_err());
        }
        for selection in ["terminal:one", "--all"] {
            let cli =
                Cli::try_parse_from(["mj", "stop-task", "--session", "s1", selection, "--json"])
                    .unwrap();
            let Some(Command::StopTask(args)) = cli.command else {
                panic!("expected stop-task")
            };
            assert_eq!(args.session, "s1");
            assert_eq!(args.all, selection == "--all");
            assert!(args.json);
        }
    }

    #[tokio::test]
    async fn stop_task_sends_opaque_ids_and_all_reports_partial_failures() {
        use axum::http::{HeaderMap, StatusCode};
        use axum::response::IntoResponse;
        use axum::routing::{get, post};
        use axum::{Json, Router};
        use std::sync::{Arc, Mutex};

        let mut session = wait_response("finished", serde_json::json!({})).session;
        session.background_tasks = [
            ("terminal:one / opaque", true),
            ("terminal:failed", true),
            ("native:observer", false),
            ("terminal:two", true),
        ]
        .into_iter()
        .map(|(id, can_stop)| mj_daemon::server::ViewerBackgroundTask {
            id: id.into(),
            command: "sleep 60".into(),
            started_at_ms: 0,
            can_stop,
        })
        .collect();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let requests = seen.clone();
        let app = Router::new()
            .route(
                "/api/v1/sessions/s1",
                get(move || {
                    let session = session.clone();
                    async move { Json(session) }
                }),
            )
            .route(
                "/api/v1/sessions/s1/background-tasks/stop",
                post(
                    move |headers: HeaderMap,
                          Json(request): Json<
                        mj_daemon::server::api::StopBackgroundTaskRequest,
                    >| {
                        let requests = requests.clone();
                        async move {
                            assert_eq!(headers["authorization"], "Bearer test-token");
                            let id = request.background_task_id;
                            requests.lock().unwrap().push(id.clone());
                            if id == "terminal:failed" {
                                (
                                    StatusCode::CONFLICT,
                                    Json(serde_json::json!({"error":"task already ended"})),
                                )
                                    .into_response()
                            } else {
                                StatusCode::ACCEPTED.into_response()
                            }
                        }
                    },
                ),
            )
            .layer(axum::middleware::from_fn(
                |request: axum::extract::Request, next: axum::middleware::Next| async move {
                    let mut response = next.run(request).await;
                    response
                        .headers_mut()
                        .insert("mj-api-version", "1".parse().unwrap());
                    response
                },
            ));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = mj_core::runtime::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let client = ApiClient::new(format!("http://{address}"), "test-token".into()).unwrap();
        let mut args = StopTaskArgs {
            session: "s1".into(),
            task_id: Some("terminal:one / opaque".into()),
            all: false,
            json: true,
        };
        let single = stop_tasks(&client, &args).await.unwrap();
        assert_eq!(single.accepted_task_ids, ["terminal:one / opaque"]);
        assert_eq!(*seen.lock().unwrap(), ["terminal:one / opaque"]);
        seen.lock().unwrap().clear();
        args.task_id = None;
        args.all = true;
        let all = stop_tasks(&client, &args).await.unwrap();
        assert_eq!(
            all.accepted_task_ids,
            ["terminal:one / opaque", "terminal:two"]
        );
        assert_eq!(all.skipped_task_ids, ["native:observer"]);
        assert!(all.failures["terminal:failed"].contains("task already ended"));
        let mut requests = seen.lock().unwrap().clone();
        requests.sort();
        assert_eq!(
            requests,
            ["terminal:failed", "terminal:one / opaque", "terminal:two"]
        );
        let json = serde_json::to_value(&all).unwrap();
        assert_eq!(json["session_id"], "s1");
        assert_eq!(json["accepted_task_ids"].as_array().unwrap().len(), 2);
        server.abort();
        assert!(server.await.unwrap_err().is_cancelled());
    }

    // Hard-won: db7d3a78: --json export with --out printed prose instead of a JSON object.
    #[test]
    fn export_json_with_out_reports_one_object_and_text_stays_a_sentence() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("work.patch");
        let mut printed = Vec::new();
        write_bytes_to(&mut printed, b"diff", Some(&path), true, "patch").unwrap();
        let report: serde_json::Value = serde_json::from_slice(&printed).unwrap();
        assert_eq!(report["bytes"], 4);
        assert_eq!(report["format"], "patch");
        assert_eq!(report["path"], path.to_str().unwrap());
        assert_eq!(std::fs::read(&path).unwrap(), b"diff");

        let mut printed = Vec::new();
        write_bytes_to(&mut printed, b"diff", Some(&path), false, "patch").unwrap();
        assert!(
            String::from_utf8(printed)
                .unwrap()
                .starts_with("wrote 4 bytes to ")
        );

        let mut printed = Vec::new();
        write_bytes_to(&mut printed, b"diff", None, true, "patch").unwrap();
        assert_eq!(printed, b"diff");
    }

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

    /// Serve `app` on a loopback port and return its base URL.
    async fn serve_api(app: axum::Router) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        mj_core::runtime::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        url
    }

    /// A daemon being replaced ends a wait with the handoff mark. The wait
    /// asks the next daemon for the same turn, with what is left of its
    /// budget, and reports that daemon's answer instead of failing.
    // Hard-won: c87e5e88: wait ended with an error instead of following its accepted turn after daemon replacement.
    #[tokio::test]
    async fn a_wait_follows_the_daemon_across_an_upgrade_handoff() {
        use axum::http::StatusCode;
        use std::sync::{Arc, Mutex};
        let old = serve_api(axum::Router::new().route(
            "/api/v1/sessions/{session_id}/wait",
            axum::routing::post(|| async {
                (
                    StatusCode::SERVICE_UNAVAILABLE,
                    [
                        (mj_daemon::server::UPGRADE_HEADER, "pending"),
                        ("retry-after", "1"),
                    ],
                    axum::Json(serde_json::json!({
                        "error": "the Mjolnir daemon is being replaced by an upgrade; ask again",
                        "code": mj_daemon::server::api::DAEMON_HANDOFF_CODE,
                    })),
                )
            }),
        ))
        .await;
        let asked: Arc<Mutex<Vec<WaitRequest>>> = Arc::default();
        let new = serve_api(
            axum::Router::new()
                .route(
                    "/api/v1/sessions/{session_id}/wait",
                    axum::routing::post(
                        |axum::extract::State(asked): axum::extract::State<
                            Arc<Mutex<Vec<WaitRequest>>>,
                        >,
                         axum::Json(request): axum::Json<WaitRequest>| async move {
                            asked.lock().unwrap().push(request);
                            (
                                [(mj_daemon::server::api::API_VERSION_HEADER, "1")],
                                axum::Json(wait_response("finished", serde_json::json!({}))),
                            )
                        },
                    ),
                )
                .with_state(asked.clone()),
        )
        .await;
        let reconnects = Arc::new(Mutex::new(0_usize));
        let response = wait_following_handoffs(
            ApiClient::new(old, "token".into()).unwrap(),
            "s1",
            WaitRequest {
                return_on_input: true,
                turn_id: Some(4),
                timeout_secs: Some(3600),
            },
            |_: &WaitRequest| {
                *reconnects.lock().unwrap() += 1;
                let new = new.clone();
                async move { ApiClient::new(new, "token".into()) }
            },
        )
        .await
        .unwrap();
        assert_eq!(response.outcome, WaitOutcome::Finished);
        assert_eq!(*reconnects.lock().unwrap(), 1);
        let asked = asked.lock().unwrap();
        assert_eq!(asked.len(), 1);
        assert!(asked[0].return_on_input);
        assert_eq!(asked[0].turn_id, Some(4), "the same turn, not a new prompt");
        let remaining = asked[0].timeout_secs.unwrap();
        assert!(
            (3590..=3600).contains(&remaining),
            "the next daemon gets what is left of the budget: {remaining}"
        );
    }

    /// A command continued under the daemon's newer build must parse there as
    /// the rest of the same command: same turn, time left, same filter.
    // Hard-won: 4ce83357: wait and events did not continue under the replacing daemon after a protocol bump.
    #[test]
    fn continuations_parse_as_the_rest_of_the_same_command() {
        let request = WaitRequest {
            return_on_input: true,
            turn_id: Some(4),
            timeout_secs: Some(1200),
        };
        let argv = std::iter::once("mj".to_owned()).chain(wait_continuation("s1", &request, true));
        let Some(Command::Wait(wait)) = Cli::try_parse_from(argv).unwrap().command else {
            panic!("expected the wait command");
        };
        assert_eq!(
            (wait.session.as_str(), wait.turn, wait.timeout),
            ("s1", Some(4), Some(1200))
        );
        assert!(wait.return_on_input && wait.json);

        let filter = mj_controller::database::ApiEventFilter {
            session_id: Some("s1".into()),
            workspace_id: Some("w1".into()),
        };
        let argv = std::iter::once("mj".to_owned()).chain(events_continuation(&filter, Some(9)));
        let Some(Command::Events(events)) = Cli::try_parse_from(argv).unwrap().command else {
            panic!("expected the events command");
        };
        assert_eq!(events.session.as_deref(), Some("s1"));
        assert_eq!(events.workspace_id.as_deref(), Some("w1"));
        assert_eq!(events.after_seq, Some(9));
    }

    #[test]
    fn new_opt_out_flags_are_off_unless_named() {
        let parse = |extra: &[&str]| {
            let mut argv = vec!["mj", "new", "--bundle", "product"];
            argv.extend_from_slice(extra);
            match Cli::try_parse_from(argv).unwrap().command {
                Some(Command::New(args)) => (
                    args.no_project_memory,
                    args.no_github_events,
                    args.no_github_credentials,
                ),
                _ => panic!("expected the new command"),
            }
        };
        assert!(Cli::try_parse_from(["mj", "new", "--bundle", "product", "--no-mailbox"]).is_err());
        assert_eq!(parse(&[]), (false, false, false));
        assert_eq!(parse(&["--no-project-memory"]), (true, false, false));
        assert_eq!(parse(&["--no-github-events"]), (false, true, false));
        assert_eq!(parse(&["--no-github-credentials"]), (false, false, true));
        assert_eq!(
            parse(&["--no-github-events", "--no-project-memory"]),
            (true, true, false)
        );
    }

    #[test]
    fn new_delivers_notifications_unless_ignored() {
        let parse = |extra: &[&str]| {
            let mut argv = vec!["mj", "new", "--bundle", "product"];
            argv.extend_from_slice(extra);
            let Some(Command::New(args)) = Cli::try_parse_from(argv).unwrap().command else {
                panic!("expected the new command");
            };
            args.ignore_notifications
        };
        assert!(!parse(&[]));
        assert!(parse(&["--ignore-notifications"]));
    }

    #[test]
    fn no_later_history_needs_at() {
        let sha = "0123456789abcdef0123456789abcdef01234567";
        let parse = |extra: &[&str]| {
            let mut argv = vec!["mj", "new", "--bundle", "product"];
            argv.extend_from_slice(extra);
            Cli::try_parse_from(argv).map(|cli| match cli.command {
                Some(Command::New(args)) => (args.at, args.no_later_history),
                _ => panic!("expected the new command"),
            })
        };
        assert_eq!(parse(&[]).unwrap(), (None, false));
        assert_eq!(parse(&["--at", sha]).unwrap(), (Some(sha.into()), false));
        assert_eq!(
            parse(&["--at", sha, "--no-later-history"]).unwrap(),
            (Some(sha.into()), true)
        );
        assert!(parse(&["--no-later-history"]).is_err(), "requires --at");
        let refused = name_launch_flags(anyhow::anyhow!("`no_later_history` requires `at`"));
        assert_eq!(refused.to_string(), "--no-later-history requires --at");
    }

    // Hard-won: #1615: mj new --project-directory on a container target creates a quick bundle and allows --at/--no-later-history
    #[tokio::test]
    async fn new_project_directory_on_container_target_resolves_quick_bundle_and_allows_at() {
        let sha = "0123456789abcdef0123456789abcdef01234567";
        let cli = Cli::try_parse_from([
            "mj",
            "new",
            "--project-directory",
            "/work/repo",
            "--target",
            "podman",
            "--at",
            sha,
            "--no-later-history",
        ])
        .expect("--project-directory should allow --at and --no-later-history");
        let Some(Command::New(args)) = cli.command else {
            panic!("expected the new command");
        };
        assert_eq!(args.at.as_deref(), Some(sha));
        assert!(args.no_later_history);
        assert_eq!(
            args.project_directory.as_deref(),
            Some(std::path::Path::new("/work/repo"))
        );

        let options = mj_daemon::server::api::LaunchOptions {
            revision: 1,
            primary_configured: false,
            profiles: Vec::new(),
            targets: vec![
                mj_daemon::server::api::LaunchTarget {
                    id: "podman".into(),
                    kind: "podman".into(),
                    requires_project_directory: false,
                    availability: mj_daemon::server::api::LaunchAvailability::Ready,
                    unavailable_reason: None,
                    runtime_missing: false,
                    default_candidate: false,
                    host: None,
                },
                mj_daemon::server::api::LaunchTarget {
                    id: "localhost".into(),
                    kind: "local-bare".into(),
                    requires_project_directory: true,
                    availability: mj_daemon::server::api::LaunchAvailability::Ready,
                    unavailable_reason: None,
                    runtime_missing: false,
                    default_candidate: false,
                    host: None,
                },
            ],
            bundles: Vec::new(),
            hosts: Vec::new(),
            default: None,
        };
        let client = ApiClient::new("http://127.0.0.1:1".into(), "test-token".into()).unwrap();

        let (bundle_id, project_dir) = resolve_new_session_project_target(
            &client,
            None,
            Some(PathBuf::from("/work/repo")),
            Some("podman"),
            Some(options.clone()),
            |dir| async move {
                assert_eq!(dir, PathBuf::from("/work/repo"));
                Ok("quick-bundle".to_owned())
            },
        )
        .await
        .expect("container target should resolve quick bundle");
        assert_eq!(bundle_id.as_deref(), Some("quick-bundle"));
        assert_eq!(project_dir, None);

        let (bare_bundle, bare_dir) = resolve_new_session_project_target(
            &client,
            None,
            Some(PathBuf::from("/work/repo")),
            Some("localhost"),
            Some(options),
            |_dir| async { panic!("bare target must not create a quick bundle") },
        )
        .await
        .expect("bare target should keep project_directory");
        assert_eq!(bare_bundle, None);
        assert_eq!(bare_dir, Some(PathBuf::from("/work/repo")));
    }

    // Hard-won: 06cfc1cfe: reviewer-only flags preserve the global trigger.
    #[test]
    fn new_review_flags_choose_the_sessions_own_review() {
        use mj_core::config::{ReviewConfig, ReviewTrigger, SessionReview, SubagentChoice};
        let parse = |extra: &[&str]| {
            let mut argv = vec!["mj", "new", "--bundle", "product"];
            argv.extend_from_slice(extra);
            Cli::try_parse_from(argv).map(|cli| match cli.command {
                Some(Command::New(args)) => new_session_review(&args),
                _ => panic!("expected new command"),
            })
        };
        assert_eq!(parse(&[]).unwrap().unwrap(), None);
        let inherited = parse(&[
            "--reviewer",
            "gpt-6-terra:xhigh",
            "--reviewer",
            "gpt-6-sol:medium",
        ])
        .unwrap()
        .unwrap()
        .unwrap();
        let choices = vec![
            SubagentChoice {
                model: "gpt-6-terra".into(),
                effort: Some("xhigh".into()),
            },
            SubagentChoice {
                model: "gpt-6-sol".into(),
                effort: Some("medium".into()),
            },
        ];
        assert_eq!(
            inherited,
            SessionReview::Inherit {
                reviewers: Some(choices.clone())
            }
        );
        // Session reviewer flags enable review even without global reviewers.
        for (global_reviewers, trigger) in [
            (Vec::new(), ReviewTrigger::Push),
            (vec![choices[1].clone()], ReviewTrigger::Push),
        ] {
            let global = ReviewConfig {
                reviewers: global_reviewers,
                ..Default::default()
            };
            assert_eq!(global.for_session(Some(&inherited)).trigger(), trigger);
            assert_eq!(global.for_session(Some(&inherited)).reviewers, choices);
        }
        assert_eq!(
            parse(&["--review", "off"]).unwrap().unwrap(),
            Some(SessionReview::Off)
        );
        assert_eq!(
            parse(&["--review", "push"]).unwrap().unwrap(),
            Some(SessionReview::Push { reviewers: None })
        );
        // Per-turn review was removed; the refusal says what to use instead.
        let refused = parse(&["--review", "turn"])
            .unwrap()
            .unwrap_err()
            .to_string();
        assert!(refused.contains("--review push"), "{refused}");
        assert!(
            parse(&[
                "--reviewer",
                "a",
                "--reviewer",
                "b",
                "--reviewer",
                "c",
                "--reviewer",
                "d"
            ])
            .unwrap()
            .is_err()
        );
        assert!(parse(&["--reviewer", ":high"]).unwrap().is_err());
        for (input, model, effort) in [
            ("openai/gpt-oss-120b:free", "openai/gpt-oss-120b:free", None),
            ("qwen:latest", "qwen:latest", None),
            ("x:0:high", "x:0", Some("high")),
            ("foo:high:", "foo:high", None),
        ] {
            let parsed = parse(&["--reviewer", input]).unwrap().unwrap().unwrap();
            let SessionReview::Inherit {
                reviewers: Some(reviewers),
            } = parsed
            else {
                panic!("review override expected")
            };
            assert_eq!(reviewers[0].model, model);
            assert_eq!(reviewers[0].effort.as_deref(), effort);
        }

        let parsed = parse(&[
            "--reviewer",
            "anthropic.claude-haiku:0",
            "--reviewer",
            "x:0:high",
            "--reviewer",
            "m:high",
        ])
        .unwrap()
        .unwrap()
        .unwrap();
        assert_eq!(
            parsed,
            SessionReview::Inherit {
                reviewers: Some(vec![
                    SubagentChoice {
                        model: "anthropic.claude-haiku:0".into(),
                        effort: None
                    },
                    SubagentChoice {
                        model: "x:0".into(),
                        effort: Some("high".into())
                    },
                    SubagentChoice {
                        model: "m".into(),
                        effort: Some("high".into())
                    },
                ])
            }
        );
        assert!(parse(&["--review-count", "2"]).is_err());
    }

    #[test]
    fn new_and_move_parse_subagent_overrides_and_reject_conflicts() {
        let parse_command = |command: &str, extra: &[&str]| {
            let mut argv = vec!["mj", command, "--profile", "codex", "--target", "raw"];
            if command == "new" {
                argv.extend(["--project-directory", "/srv/project"]);
            } else {
                argv.extend(["--session", "session"]);
            }
            argv.extend_from_slice(extra);
            let cli = Cli::try_parse_from(argv).map_err(anyhow::Error::from)?;
            match cli.command {
                Some(Command::New(args)) => new_subagent_policy(&args),
                Some(Command::Move(args)) => subagent_policy(
                    args.subagents.as_deref(),
                    args.subagent_straightforward.as_deref(),
                    args.subagent_straightforward_effort.as_deref(),
                    args.subagent_subtle.as_deref(),
                    args.subagent_subtle_effort.as_deref(),
                    args.subagent_model.as_deref(),
                    args.subagent_effort.as_deref(),
                ),
                _ => panic!("new or move command"),
            }
        };
        for command in ["new", "move"] {
            let parse = |extra: &[&str]| parse_command(command, extra);
            let oracle = |extra: &[&str]| {
                let mut argv = vec!["mj", command, "--profile", "codex", "--target", "raw"];
                if command == "new" {
                    argv.extend(["--project-directory", "/srv/project"]);
                } else {
                    argv.extend(["--session", "session"]);
                }
                argv.extend_from_slice(extra);
                let cli = Cli::try_parse_from(argv).map_err(anyhow::Error::from)?;
                match cli.command {
                    Some(Command::New(args)) => oracle_override(
                        args.subagent_oracle.as_deref(),
                        args.subagent_oracle_effort.as_deref(),
                        args.oracle.as_deref(),
                    ),
                    Some(Command::Move(args)) => oracle_override(
                        args.subagent_oracle.as_deref(),
                        args.subagent_oracle_effort.as_deref(),
                        args.oracle.as_deref(),
                    ),
                    _ => unreachable!(),
                }
            };
            assert_eq!(
                oracle(&[
                    "--subagents",
                    "native",
                    "--subagent-oracle",
                    "advisor",
                    "--subagent-oracle-effort",
                    "high"
                ])
                .unwrap(),
                Some(mj_core::config::SubagentChoice {
                    model: "advisor".into(),
                    effort: Some("high".into())
                })
            );
            assert!(oracle(&["--subagent-oracle-effort", "high"]).is_err());
            assert!(
                oracle(&[
                    "--subagent-oracle",
                    "advisor",
                    "--subagent-oracle-effort",
                    "adaptive"
                ])
                .is_err()
            );
            assert!(oracle(&["--oracle", "disabled", "--subagent-oracle", "advisor"]).is_err());
            assert!(oracle(&["--oracle", "invalid"]).is_err());
            use mj_core::subagent::SubagentPolicy;
            assert_eq!(parse(&[]).unwrap(), None);
            assert_eq!(
                parse(&["--subagents", "native"]).unwrap(),
                Some(SubagentPolicy::Native)
            );
            assert_eq!(
                parse(&["--subagents", "mjolnir"]).unwrap(),
                Some(SubagentPolicy::Mjolnir {
                    orchestrators: false,
                    straightforward: None,
                    subtle: None
                })
            );
            assert_eq!(
                parse(&["--subagents", "none"]).unwrap(),
                Some(SubagentPolicy::None)
            );
            for mode in [None, Some("mjolnir")] {
                let mut args = vec![
                    "--subagent-subtle",
                    "gpt-6-luna",
                    "--subagent-subtle-effort",
                    "max",
                ];
                if let Some(mode) = mode {
                    args.extend(["--subagents", mode]);
                }
                assert_eq!(
                    parse(&args).unwrap(),
                    Some(SubagentPolicy::Mjolnir {
                        orchestrators: false,
                        straightforward: None,
                        subtle: Some(Box::new(mj_core::config::SubagentChoice {
                            model: "gpt-6-luna".into(),
                            effort: Some("max".into())
                        })),
                    })
                );
            }
            for args in [
                &["--subagent-straightforward-effort", "max"][..],
                &[
                    "--subagent-subtle",
                    "model",
                    "--subagent-subtle-effort",
                    "adaptive",
                ],
                &["--subagent-model", "model"],
                &["--mj-subagents"],
                &["--native-subagents"],
                &["--subagents", "all-models"],
                &["--subagents", "single-model"],
                &[
                    "--subagents",
                    "native",
                    "--subagent-straightforward",
                    "model",
                ],
                &["--subagents", "native", "--subagent-subtle-effort", "high"],
                &["--subagents", "none", "--subagent-straightforward", "model"],
            ] {
                assert!(parse(args).is_err());
            }
            let retired = parse(&["--subagent-model", "model"]).unwrap_err();
            for flag in [
                "--subagent-straightforward",
                "--subagent-straightforward-effort",
                "--subagent-subtle",
                "--subagent-subtle-effort",
            ] {
                assert!(retired.to_string().contains(flag), "{retired}");
            }
        }
    }

    /// A turn the worker failed for going quiet has to say why, where a script
    /// waiting on it can see it. Before this the reason lived only in the
    /// transcript and `mj wait` printed the bare word "error" (#1020).
    /// Launch finding J-25: `mj prompt --wait` printed the Codex quota
    /// sentence three times, as the wait's message, the diagnostic, and the
    /// agent's final message. Each distinct line is printed once.
    // Hard-won: cd2f1b6d: a Codex quota reason was printed three times and surfaced as an ordinary error.
    #[test]
    fn a_reason_repeated_in_the_final_message_is_printed_once() {
        let sentence = "You’ve hit your usage limit. Try again at Sep 29th, 2026 10:20 PM.";
        let response = wait_response(
            "quota_limit",
            serde_json::json!({
                "stop_reason": "QuotaLimit",
                "turn_id": 8,
                "message": sentence,
                "diagnostic": {"message": sentence, "code": "usageLimitExceeded"},
                "final_message": format!("{sentence}\n"),
            }),
        );
        let lines = wait_report_lines(&response);
        assert_eq!(lines[0], "turn quota_limit (failed: quota limit reached)");
        assert_eq!(
            lines
                .iter()
                .filter(|line| line.contains("usage limit"))
                .count(),
            1,
            "{lines:?}"
        );
    }

    // Hard-won: 4a875bad: wait showed only error and hid the diagnostic already recorded by the worker.
    #[test]
    fn a_failed_turn_reports_the_reason_the_worker_recorded() {
        let response = wait_response(
            "error",
            serde_json::json!({
                "stop_reason": "harness_inactive",
                "diagnostic": {
                    "message": "The Muse turn stopped responding: the tool call job_output-7 ran for about 241 minutes.",
                    "code": "harness_inactive"
                }
            }),
        );
        let lines = wait_report_lines(&response);
        assert_eq!(lines[0], "turn error (failed: harness inactive)");

        // I2-5: the transcript position of the turn's prompt is not a turn
        // count, so the wait prints no number for it.
        let numbered = wait_response(
            "finished",
            serde_json::json!({"turn_id": 8, "turn_number": 1}),
        );
        assert_eq!(wait_report_lines(&numbered)[0], "turn finished");
        assert!(
            lines.iter().any(|line| line.contains("job_output-7")),
            "the reason is printed: {lines:?}"
        );

        // Hard-won: DSH campaign C-2: failed startup details were present in the session but absent from `mj wait`.
        let mut startup_failure = wait_response("error", serde_json::json!({}));
        startup_failure.session.error =
            Some("install the harness in the target environment".into());
        assert!(wait_report_lines(&startup_failure).iter().any(|line| {
            line == "session error: install the harness in the target environment"
        }));

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

    /// precision-3260: sessions on a full disk read as "unreachable" with
    /// nothing saying why. The list marks them and the report says why.
    // Hard-won: 540c9202: a real full filesystem stopped workers while users saw only unreachable.
    #[test]
    fn a_session_on_a_full_disk_says_so_in_the_list_and_the_report() {
        let mut session = wait_response("finished", serde_json::json!({})).session;
        assert_eq!(listed_state(&session), "running");
        session.storage_problem = Some("disk full: precision-3260 has 0 B free on /".to_owned());
        assert_eq!(listed_state(&session), "running (disk full)");
        assert_eq!(
            session_report_lines(&session, 0),
            [
                "s1  running (disk full)  t",
                "disk full: precision-3260 has 0 B free on /"
            ]
        );
        // Served severity wins even when the older raw fields disagree.
        session.attention = mj_core::attention::SessionAttention::Important;
        assert_eq!(listed_state(&session), "running (important) (disk full)");
        session.attention = mj_core::attention::SessionAttention::Unreachable;
        assert_eq!(listed_state(&session), "running (unreachable) (disk full)");
    }

    // Hard-won: #1209: session detail could not show startup progress or loss of worker contact.
    #[test]
    fn session_detail_renders_lifecycle_and_worker_relay() {
        let mut session = wait_response("finished", serde_json::json!({})).session;
        session.state = "launching".into();
        session.operation = Some(
            serde_json::from_value(serde_json::json!({
                "id": "create-s1", "session_id": "s1", "kind": "create",
                "started_at_epoch_seconds": 100, "cancellable": true,
                "stages": [{"label": "Sync", "started_at_epoch_seconds": 110}],
            }))
            .unwrap(),
        );
        session.relay = Some(mj_daemon::server::api::RelayHealth {
            state: RelayState::Disconnected,
            detail: None,
        });
        assert_eq!(
            session_report_lines(&session, 0),
            [
                "s1  launching  t",
                "create started 1970-01-01T00:01:40Z",
                "  Sync since 1970-01-01T00:01:50Z",
                "worker relay is not attached",
            ]
        );

        session.state = "running".into();
        session.operation = None;
        session.relay.as_mut().unwrap().state = RelayState::Unreachable;
        assert_eq!(
            session_report_lines(&session, 0),
            ["s1  running  t", "worker relay is unreachable",]
        );
    }

    // Hard-won: #1529: refused unexpired logins were absent from session and doctor diagnostics.
    #[test]
    fn golden_refused_profile_login_diagnostics() {
        let failure = mj_client::quota::refused_login_message("claude");
        let mut session = wait_response(
            "error",
            serde_json::json!({
                "diagnostic": {"message": "Authentication required", "code": "-32000"}
            }),
        )
        .session;
        session.profile_id = "claude".into();
        session.profile_login_error = Some(failure.clone());
        let mut output = String::from("mj sessions --session s1\n");
        output.push_str(&session_report_lines(&session, 0).join("\n"));
        output.push_str("\n\nmj doctor\n");
        let checks = mj_setup::doctor::login_checks(&[("claude".into(), failure)].into());
        let mut doctor = Vec::new();
        mj_setup::doctor::render_human(&checks, &mut doctor).unwrap();
        output.push_str(&String::from_utf8(doctor).unwrap());
        output.push_str("\nSession JSON login status: ");
        output
            .push_str(&serde_json::to_value(&session).unwrap()["profile_login_error"].to_string());
        output.push('\n');
        mj_core::golden::assert_golden(
            env!("CARGO_MANIFEST_DIR"),
            "profile-login-refused",
            &output,
        );
    }

    /// Launch finding R11-3: `mj sessions --session` printed `last turn
    /// Completed { stop_reason: "EndTurn" }`, Rust's debug form. It says how
    /// the turn ended in words.
    // Hard-won: 1018b0f7: session summaries exposed Rust debug spellings instead of user-readable turn outcomes.
    #[test]
    fn one_session_names_how_its_last_turn_ended_in_words() {
        let with_outcome = |outcome: serde_json::Value| {
            let mut session = wait_response("finished", serde_json::json!({})).session;
            session.last_turn_outcome = Some(
                serde_json::from_value(serde_json::json!({
                    "command_id": "prompt-1",
                    "accepted_ordinal": 16,
                    "completed_ordinal": 46,
                    "completed_at_ms": 0,
                    "outcome": outcome,
                }))
                .unwrap(),
            );
            session_report_lines(&session, 0)
        };
        let finished =
            with_outcome(serde_json::json!({"kind": "completed", "stop_reason": "EndTurn"}));
        assert_eq!(
            finished,
            ["s1  running  t", "last turn completed, end of turn"]
        );
        for (outcome, words) in [
            (
                serde_json::json!({"kind": "completed", "stop_reason": "end_turn"}),
                "completed, end of turn",
            ),
            (
                serde_json::json!({"kind": "completed", "stop_reason": "Cancelled"}),
                "interrupted",
            ),
            (
                serde_json::json!({"kind": "interrupted", "message": "Interrupted by the user"}),
                "interrupted",
            ),
            (
                serde_json::json!({"kind": "completed", "stop_reason": "MaxTokens"}),
                "failed: max tokens",
            ),
            (
                serde_json::json!({"kind": "completed", "stop_reason": "max_turn_requests"}),
                "failed: max turn requests",
            ),
            (
                serde_json::json!({"kind": "rejected", "message": "the session is closing\nmore detail"}),
                "failed: the session is closing",
            ),
        ] {
            assert_eq!(with_outcome(outcome)[1], format!("last turn {words}"),);
        }
    }

    /// Launch finding R12-1: `mj wait` printed the harness's stop reason as
    /// the API returns it, "finished (EndTurn) turn 16 in 5.3s", while `mj
    /// sessions` and the parent's sub-agent notice said "completed, end of
    /// turn". `mj wait` and `mj prompt` use the same words, placed as
    /// the notice places them; `--json` keeps the stop reason unchanged.
    // Hard-won: 5a09877c: wait exposed raw harness stop reasons unlike the existing session summary.
    #[test]
    fn a_wait_says_how_the_turn_ended_in_the_words_mj_sessions_uses() {
        let finished = wait_response(
            "finished",
            serde_json::json!({"stop_reason": "EndTurn", "turn_id": 16, "elapsed_ms": 5300}),
        );
        assert_eq!(
            wait_report_lines(&finished)[0],
            "turn finished (completed, end of turn) in 5.3s"
        );
        let worked = wait_response(
            "finished",
            serde_json::json!({
                "stop_reason": "EndTurn", "turn_id": 16, "elapsed_ms": 5300, "tool_calls": 4
            }),
        );
        assert_eq!(
            wait_report_lines(&worked)[0],
            "turn finished (completed, end of turn) in 5.3s · 4 tool calls"
        );
        let one_call = wait_response("finished", serde_json::json!({"tool_calls": 1}));
        assert_eq!(
            wait_report_lines(&one_call)[0],
            "turn finished · 1 tool call"
        );
        let no_calls = wait_response("finished", serde_json::json!({"tool_calls": 0}));
        assert_eq!(wait_report_lines(&no_calls)[0], "turn finished");
        // A wait for an earlier turn answers about the later one, and says so.
        let later = wait_response(
            "finished",
            serde_json::json!({"turn_id": 87, "requested_turn_id": 82, "turn_number": 5}),
        );
        assert!(
            wait_report_lines(&later)[1].contains("outcome of turn 87, not of turn 82"),
            "{:?}",
            wait_report_lines(&later)
        );
        // Hard-won: #1450 review: autonomous outcomes hid which turn answered.
        let autonomous = wait_response(
            "finished",
            serde_json::json!({"requested_turn_id":82, "turn_number":5}),
        );
        assert_eq!(
            wait_report_lines(&autonomous)[1],
            "this is the outcome of a turn the session ran without a prompt, not of turn 82; a session keeps only its latest turn's outcome and reply"
        );
        // An answer with no finished turn, such as a failed session, is not one.
        let earlier = wait_response(
            "finished",
            serde_json::json!({"turn_id": 80, "requested_turn_id":82, "turn_number":5}),
        );
        assert!(wait_report_lines(&earlier)[1].contains("outcome of turn 80, not of turn 82"));
        let failed = wait_response("error", serde_json::json!({"requested_turn_id":82}));
        assert!(
            !wait_report_lines(&failed)
                .iter()
                .any(|line| line.contains("this is the outcome")),
            "{:?}",
            wait_report_lines(&failed)
        );
        let same = wait_response(
            "finished",
            serde_json::json!({"turn_id": 82, "requested_turn_id": 82}),
        );
        assert_eq!(wait_report_lines(&same).len(), 1);
        // What the session does after the turn, in the dashboard's words.
        let busy = wait_response(
            "finished",
            serde_json::json!({
                "stop_reason": "EndTurn",
                "next": {"state": "expecting", "since_ms": 1}
            }),
        );
        assert_eq!(
            wait_report_lines(&busy)[0],
            "turn finished (completed, end of turn); the session is not idle: expecting the agent to continue"
        );
        let mut waiting_for_answer = wait_response(
            "timeout",
            serde_json::json!({
                "pending_elicitations": [{"id": "question-1", "message": "Approve?", "fields": []}]
            }),
        );
        waiting_for_answer.session.id = "session-1".into();
        // Hard-won: DSH campaign E-2: a prompt timeout hid the pending permission request and its response options.
        assert!(wait_report_lines(&waiting_for_answer).iter().any(|line| {
            line.contains("mj elicitations --session session-1")
                && line.contains("mj respond --session session-1")
                && line.contains("mj prompt --return-on-input")
        }));
        assert_eq!(
            serde_json::to_value(&finished).unwrap()["stop_reason"],
            "EndTurn",
            "--json prints the stop reason as the API returns it"
        );
        for (outcome, stop_reason, words) in [
            ("finished", "end_turn", "completed, end of turn"),
            (
                "input_required",
                "awaiting_input",
                "completed, waiting for input",
            ),
            ("cancelled", "Cancelled", "interrupted"),
            ("quota_limit", "QuotaLimit", "failed: quota limit reached"),
            ("error", "MaxTokens", "failed: max tokens"),
            ("error", "prompt_unanswered", "failed: prompt unanswered"),
        ] {
            let response = wait_response(
                outcome,
                serde_json::json!({"stop_reason": stop_reason, "turn_id": 2}),
            );
            assert_eq!(
                wait_report_lines(&response)[0],
                format!("turn {outcome} ({words})")
            );
            // The same words `mj sessions --session` prints for that turn.
            let mut session = response.session.clone();
            session.last_turn_outcome = Some(
                serde_json::from_value(serde_json::json!({
                    "command_id": "prompt-1",
                    "completed_ordinal": 3,
                    "completed_at_ms": 0,
                    "outcome": {"kind": "completed", "stop_reason": stop_reason},
                }))
                .unwrap(),
            );
            assert_eq!(
                session_report_lines(&session, 0)[1],
                format!("last turn {words}")
            );
        }
    }

    /// F-6: `mj suspend` says to watch with `mj wait`, which then failed with
    /// "the turn ended as stopped" once the suspension had succeeded.
    // Hard-won: 2a521fc5: successful suspension made mj wait report an error and exit nonzero.
    #[test]
    fn a_wait_that_sees_a_finished_suspension_reports_success() {
        let suspended = |error: serde_json::Value| {
            let mut response = wait_response(
                "stopped",
                serde_json::json!({"message": "the session is stopped or stopping"}),
            );
            response.session.lifecycle = ViewerLifecycleCategory::Suspended;
            response.session.error = serde_json::from_value(error).unwrap();
            response
        };

        let clean = suspended(serde_json::Value::Null);
        assert_eq!(wait_report_lines(&clean), ["session suspended"]);
        assert!(report_wait(&clean, false).is_ok());

        // A resume that failed also leaves the session stopped, with its
        // reason recorded, and that is still a failure.
        let failed_resume = suspended(serde_json::json!("the archive was missing"));
        assert!(report_wait(&failed_resume, false).is_err());
    }

    // Hard-won: 0877ce6d: the refusal named API fields instead of the --profile and --target flags.
    #[test]
    fn a_refused_start_names_the_flags_the_user_typed() {
        let error = name_launch_flags(anyhow::anyhow!(
            "the Mjolnir API answered 400 Bad Request: name a profile_id; this instance has no saved default to fall back on"
        ));
        assert_eq!(
            error.to_string(),
            "the Mjolnir API answered 400 Bad Request: name a --profile; this instance has no saved default to fall back on"
        );
    }

    /// R2-3: the `mj suspend` refusal told the user to "retry with
    /// acknowledge_unpublished_work=true", the API field. The CLI's flag is
    /// `--acknowledge-unpublished-work`.
    // Hard-won: d7b2f496: the unpublished-work refusal named the API field instead of the CLI flag.
    #[test]
    fn a_refused_suspend_names_the_flag_the_user_types() {
        for (api, cli) in [
            (
                "the Mjolnir API answered 409 Conflict: publication status is unverified for this live clone; retry with acknowledge_unpublished_work=true to suspend it",
                "the Mjolnir API answered 409 Conflict: publication status is unverified for this live clone; retry with --acknowledge-unpublished-work to suspend it",
            ),
            (
                "the Mjolnir API answered 409 Conflict: the checkout has unpublished or unverified work; confirm suspension with acknowledge_unpublished_work=true",
                "the Mjolnir API answered 409 Conflict: the checkout has unpublished or unverified work; confirm suspension with --acknowledge-unpublished-work",
            ),
        ] {
            assert_eq!(name_suspend_flags(anyhow::anyhow!(api)).to_string(), cli);
        }
    }

    /// F-11: a destroyed session that never took a prompt was offered
    /// `mj resume --wiki`, which then failed with "no prompt to restore from".
    // Hard-won: 94305629: a never-prompted archived session was offered a resume that always failed.
    #[test]
    fn an_archived_row_is_offered_a_resume_only_when_it_has_something_to_restore() {
        let row = |nothing_to_restore: bool| mj_client::daemon::WikiSessionInfo {
            wiki_id: "w1".into(),
            tool: "mjolnir".into(),
            path: PathBuf::from("/data/sessions/w1"),
            status: WikiSessionStatus::Archived,
            mjolnir_session_id: Some("w1".into()),
            profile_id: None,
            target_template_id: None,
            harness: None,
            title: "burst3".into(),
            project: String::new(),
            nothing_to_restore,
        };
        assert!(
            wiki_session_lines(&row(false))
                .iter()
                .any(|line| line.contains("mj resume --wiki w1"))
        );
        let lines = wiki_session_lines(&row(true));
        assert!(
            !lines.iter().any(|line| line.contains("mj resume")),
            "{lines:?}"
        );
        assert!(
            lines
                .iter()
                .any(|line| line.contains("never received a prompt"))
        );
    }

    /// `mj prompt` sends as the user. Inside a session that would let an
    /// agent's prompt read exactly as the user's own, so it refuses before it
    /// reads any input or reaches the daemon (this call has no prompt text,
    /// so anything past the refusal stops at asking for it).
    #[tokio::test]
    async fn a_prompt_from_inside_a_session_is_refused_before_it_is_sent() {
        let cli = Cli::try_parse_from(["mj", "prompt", "--session", "peer-1"]).unwrap();
        let Some(Command::Prompt(args)) = cli.command else {
            panic!("expected the prompt subcommand");
        };
        let error = prompt_from(args, Some("caller-1".into()))
            .await
            .unwrap_err();
        assert_eq!(
            format!("{error:#}"),
            "`mj prompt` sends as the user, so it cannot run inside a session \
             (MJ_SESSION_ID is set). Send with `mj message`, then follow with `mj wait`."
        );
    }

    // Hard-won: 4209cb10: branch export refusal did not name the required --branch flag.
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
        assert_eq!(args.timeout, Some(30));
        // `mj prompt` always waits. Scripts written for the retired `--wait`
        // keep working, and help no longer offers it.
        let command = <Cli as clap::CommandFactory>::command();
        let retired = command
            .find_subcommand("prompt")
            .expect("the prompt command")
            .get_arguments()
            .find(|argument| argument.get_long() == Some("wait"))
            .expect("the retired --wait");
        assert!(retired.is_hide_set());

        let cli = Cli::try_parse_from([
            "mj",
            "message",
            "--session",
            "s1",
            "--command-id",
            "steer-1",
            "--prompt-file",
            "steer.md",
            "--json",
        ])
        .unwrap();
        let Some(Command::Message(args)) = cli.command else {
            panic!("expected the message subcommand");
        };
        assert_eq!(args.command_id.as_deref(), Some("steer-1"));
        assert_eq!(args.prompt_file, Some(PathBuf::from("steer.md")));
        assert_eq!(args.text, None);
        assert!(args.json);
        assert!(
            Cli::try_parse_from(["mj", "message", "--session", "s1", "--wait", "go"]).is_err(),
            "`mj message` sends and returns; it has no --wait"
        );

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
        assert!(args.role.is_empty());

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

        // Finding R3-11: without a branch name the API answered "a branch
        // export needs a branch name", which named no flag. The command line
        // refuses it first and names `--branch`.
        let Err(error) =
            Cli::try_parse_from(["mj", "export", "--session", "s1", "--kind", "branch"])
        else {
            panic!("a branch export without --branch is refused");
        };
        assert!(error.to_string().contains("--branch"), "{error}");

        let cli = Cli::try_parse_from(["mj", "export", "--session", "s1"]).unwrap();
        let Some(Command::Export(args)) = cli.command else {
            panic!("expected the export subcommand");
        };
        assert_eq!(args.kind, ExportKindArg::Patch);

        let cli = Cli::try_parse_from(["mj", "destroy", "--session", "s1"]).unwrap();
        let Some(Command::Destroy(args)) = cli.command else {
            panic!("expected the destroy subcommand");
        };
        assert_eq!(args.session, "s1");
        assert!(
            Cli::try_parse_from(["mj", "destroy", "--session", "s1", "--delete-branch"]).is_err()
        );

        let cli = Cli::try_parse_from(["mj", "suspend", "--session", "s1"]).unwrap();
        let Some(Command::Suspend(args)) = cli.command else {
            panic!("expected the suspend subcommand");
        };
        assert_eq!(args.session.as_deref(), Some("s1"));
        assert!(Cli::try_parse_from(["mj", "suspend", "--session", "s1", "--force"]).is_err());
        let cli = Cli::try_parse_from(["mj", "close", "--session", "s1"]).unwrap();
        assert!(crate::replacement_notice(cli.command.as_ref()).is_some());

        let cli = Cli::try_parse_from(["mj", "resume", "--session", "s1"]).unwrap();
        let Some(Command::Resume(args)) = cli.command else {
            panic!("expected the resume subcommand");
        };
        assert_eq!(args.session.as_deref(), Some("s1"));

        {
            let cli = Cli::try_parse_from(["mj", "restart", "--session", "s1"]).unwrap();
            let Some(Command::Restart(args)) = cli.command else {
                panic!("expected the restart subcommand");
            };
            assert_eq!(args.session, "s1");

            let cli = Cli::try_parse_from([
                "mj",
                "move",
                "--session",
                "s1",
                "--target",
                "remote",
                "--skip",
                "cache/big.bin",
                "--dry-run",
            ])
            .unwrap();
            let Some(Command::Move(args)) = cli.command else {
                panic!("expected the move subcommand");
            };
            assert_eq!(args.skip_keep_paths, vec![PathBuf::from("cache/big.bin")]);
            assert!(args.dry_run);
            assert!(Cli::try_parse_from(["mj", "move", "--session", "s1", "--prepare"]).is_err());
            assert!(
                Cli::try_parse_from(["mj", "move", "--session", "s1", "--allow-large-transfer"])
                    .is_err()
            );
            assert!(
                Cli::try_parse_from(["mj", "move", "--session", "s1", "--exclude", "repo:a"])
                    .is_err()
            );
        }
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

        let cli = Cli::try_parse_from([
            "mj",
            "diff",
            "--session",
            "s1",
            "--base",
            "HEAD~2",
            "--json",
        ])
        .unwrap();
        let Some(Command::Diff(args)) = cli.command else {
            panic!("expected the diff subcommand");
        };
        assert_eq!(args.base.as_deref(), Some("HEAD~2"));
        assert!(args.json);

        for (argv, matched) in [
            (vec!["mj", "diff", "--session", "s1"], "diff"),
            (vec!["mj", "resume", "--session", "s1"], "resume"),
            (vec!["mj", "sessions", "--json"], "sessions"),
            (vec!["mj", "suspend", "--session", "s1"], "suspend"),
            (
                vec!["mj", "interrupt-turn", "--session", "s1"],
                "interrupt-turn",
            ),
            (vec!["mj", "api-info"], "api-info"),
        ] {
            let cli = Cli::try_parse_from(argv.clone()).unwrap_or_else(|error| {
                panic!("{matched} should parse: {error}");
            });
            assert_eq!(crate::command_name(cli.command.as_ref()), matched);
        }
    }

    #[test]
    fn transcript_role_selection_is_repeatable_and_defaults_omit_verbose_roles() {
        use mj_core::transcript::TranscriptRole;

        let cli = Cli::try_parse_from([
            "mj",
            "transcript",
            "--session",
            "s1",
            "--role",
            "agent",
            "--role",
            "user",
        ])
        .expect("repeated role filters parse");
        let Some(Command::Transcript(args)) = cli.command else {
            panic!("expected the transcript subcommand");
        };
        assert_eq!(args.role, [TranscriptRole::Agent, TranscriptRole::User]);
        assert_eq!(requested_transcript_roles(&args).unwrap(), args.role);

        let cli = Cli::try_parse_from(["mj", "transcript", "--session", "s1"])
            .expect("the default role selection parses");
        let Some(Command::Transcript(args)) = cli.command else {
            panic!("expected the transcript subcommand");
        };
        assert!(args.role.is_empty());
        assert_eq!(
            requested_transcript_roles(&args).unwrap(),
            [
                TranscriptRole::User,
                TranscriptRole::Agent,
                TranscriptRole::Message,
                TranscriptRole::Thought,
                TranscriptRole::Plan,
                TranscriptRole::PlanProposal,
                TranscriptRole::System,
            ]
        );

        assert!(
            Cli::try_parse_from(["mj", "transcript", "--session", "s1", "--all-roles"]).is_err()
        );
        let error =
            Cli::try_parse_from(["mj", "transcript", "--session", "s1", "--role", "unknown"])
                .unwrap_err()
                .to_string();
        assert!(
            error.contains("unknown transcript role \"unknown\""),
            "{error}"
        );

        let cli = Cli::try_parse_from(["mj", "transcript", "--session", "s1", "--finished-only"])
            .expect("finished-only parses without an explicit role");
        let Some(Command::Transcript(args)) = cli.command else {
            panic!("expected the transcript subcommand");
        };
        assert!(args.finished_only);
        assert_eq!(
            requested_transcript_roles(&args).unwrap(),
            [TranscriptRole::Agent]
        );

        let cli = Cli::try_parse_from([
            "mj",
            "transcript",
            "--session",
            "s1",
            "--role",
            "agent",
            "--finished-only",
        ])
        .expect("finished-only accepts an explicit agent role");
        let Some(Command::Transcript(args)) = cli.command else {
            panic!("expected the transcript subcommand");
        };
        assert_eq!(
            requested_transcript_roles(&args).unwrap(),
            [TranscriptRole::Agent]
        );

        let cli = Cli::try_parse_from([
            "mj",
            "transcript",
            "--session",
            "s1",
            "--role",
            "user",
            "--finished-only",
        ])
        .expect("role options parse before their combination is validated");
        let Some(Command::Transcript(args)) = cli.command else {
            panic!("expected the transcript subcommand");
        };
        assert_eq!(
            requested_transcript_roles(&args).unwrap_err().to_string(),
            "--finished-only can only be combined with --role agent"
        );
    }

    /// F-16: `mj usage` added text output while keeping partial coverage visible.
    #[test]
    fn usage_text_keeps_the_coverage_beside_the_totals() {
        let page: mj_core::storage::UsagePage = serde_json::from_value(serde_json::json!({
            "session_id": "s1",
            "turns": [],
            "next_after_seq": 4,
            "latest_seq": 4,
            "totals": {"input_tokens": {"tokens": 1200, "reported_turns": 2}},
            "coverage": {
                "recorded_turns": 3, "full_turn_reports": 2, "last_request_reports": 1,
                "unspecified_reports": 0, "missing_reports": 0
            }
        }))
        .unwrap();
        let lines = usage_lines(&page);
        assert_eq!(
            lines[..3],
            [
                "totals from the 2 of 3 turns that reported a whole turn:",
                "  input_tokens  1200  (2 turns)",
                "not in the totals: 1 reported only their last request (turns)",
            ]
        );
    }

    #[test]
    fn golden_api_usage_tree() {
        let tree: mj_core::storage::UsageTree = serde_json::from_value(serde_json::json!({
            "parent_session_id": "session-parent",
            "sessions": [
                {
                    "session_id": "session-parent",
                    "parent_session_id": null,
                    "task_name": null,
                    "operational_session_present": true,
                    "totals": {},
                    "coverage": {
                        "recorded_turns": 0,
                        "full_turn_reports": 0,
                        "last_request_reports": 0,
                        "unspecified_reports": 0,
                        "missing_reports": 0,
                        "unfinished_turns": 0
                    },
                    "by_model": [],
                    "provider_session_cost": {
                        "amount": 0.0123,
                        "currency": "USD",
                        "observed_at_ms": 1
                    }
                },
                {
                    "session_id": "session-reviewer",
                    "parent_session_id": "session-parent",
                    "task_name": "Review the migration plan",
                    "operational_session_present": false,
                    "totals": {},
                    "coverage": {
                        "recorded_turns": 0,
                        "full_turn_reports": 0,
                        "last_request_reports": 0,
                        "unspecified_reports": 0,
                        "missing_reports": 0,
                        "unfinished_turns": 0
                    },
                    "by_model": [],
                    "provider_session_cost": {
                        "amount": 0.0045,
                        "currency": "USD",
                        "observed_at_ms": 2
                    }
                }
            ],
            "totals": {
                "input_tokens": { "tokens": 2400, "reported_turns": 3 },
                "output_tokens": { "tokens": 650, "reported_turns": 3 }
            },
            "coverage": {
                "recorded_turns": 5,
                "full_turn_reports": 3,
                "last_request_reports": 1,
                "unspecified_reports": 0,
                "missing_reports": 1,
                "unfinished_turns": 1
            },
            "by_model": [
                {
                    "model": "gpt-5.4",
                    "effort": "high",
                    "totals": {
                        "input_tokens": { "tokens": 1800, "reported_turns": 2 },
                        "output_tokens": { "tokens": 500, "reported_turns": 2 }
                    }
                },
                {
                    "model": null,
                    "effort": null,
                    "totals": {
                        "cache_read_tokens": { "tokens": 600, "reported_turns": 1 }
                    }
                }
            ]
        }))
        .expect("usage tree fixture");

        let output = format!(
            "=== parent session usage tree (CLI text) ===\n{}",
            usage_tree_lines(&tree).join("\n")
        );
        mj_core::golden::assert_golden(env!("CARGO_MANIFEST_DIR"), "api-usage-tree", &output);
    }

    // Hard-won: #1501: Terra was missing from the shared pricing table.
    #[test]
    fn api_models_matches_cli_golden() {
        let choices = mj_core::worker_launch::ProfileConfig {
            model: Some("deepseek-flash".to_owned()),
            models: vec![
                mj_core::acp::SessionConfigChoice {
                    value: "gpt-6.1-terra".to_owned(),
                    name: "GPT-6.1 Terra".to_owned(),
                    description: None,
                },
                mj_core::acp::SessionConfigChoice {
                    value: "deepseek-flash".to_owned(),
                    name: "DeepSeek Flash".to_owned(),
                    description: None,
                },
                mj_core::acp::SessionConfigChoice {
                    value: "unknown-model".to_owned(),
                    name: "Unknown Model".to_owned(),
                    description: None,
                },
            ],
            efforts: vec![],
            observed_at: 1,
        };
        let mut output = String::from("=== mj models text and JSON output ===");
        for (name, billing) in [
            ("usage", mj_client::quota::QuotaBilling::Usage),
            ("subscription", mj_client::quota::QuotaBilling::Subscription),
            ("unknown", mj_client::quota::QuotaBilling::Unknown),
        ] {
            output.push_str(&format!("\n--- {name} text ---\n"));
            output.push_str(&render_models(&choices, billing, false).unwrap());
            output.push_str(&format!("\n--- {name} JSON ---\n"));
            output.push_str(&render_models(&choices, billing, true).unwrap());
        }
        mj_core::golden::assert_golden(env!("CARGO_MANIFEST_DIR"), "api-models", &output);
    }

    #[test]
    fn api_models_aggregate_matches_cli_golden() {
        let choices = |models: &[(&str, &str)]| mj_core::worker_launch::ProfileConfig {
            model: None,
            models: models
                .iter()
                .map(|(value, name)| mj_core::acp::SessionConfigChoice {
                    value: (*value).to_owned(),
                    name: (*name).to_owned(),
                    description: None,
                })
                .collect(),
            efforts: vec![],
            observed_at: 1,
        };
        let results = vec![
            (
                "claude-main".to_owned(),
                Ok(choices(&[
                    ("claude-opus-4-7", "Claude Opus 4.7"),
                    ("claude-sonnet-4-6", "Claude Sonnet 4.6"),
                ])),
            ),
            (
                "claude-backup".to_owned(),
                Ok(choices(&[("anthropic.claude-opus-4-7", "Opus")])),
            ),
            (
                "codex".to_owned(),
                Ok(choices(&[("gpt-5.3-codex", "GPT-5.3 Codex")])),
            ),
            (
                "grok".to_owned(),
                Err(anyhow::anyhow!("profile discovery failed: login expired")),
            ),
        ];
        let mut output = String::from("=== mj models text ===\n");
        output.push_str(&render_models_aggregate(&results, false).unwrap());
        output.push_str("\n=== mj models --json ===\n");
        output.push_str(&render_models_aggregate(&results, true).unwrap());
        output.push_str("\n=== mj models, nothing offered ===\n");
        output.push_str(&render_models_aggregate(&[], false).unwrap());
        mj_core::golden::assert_golden(env!("CARGO_MANIFEST_DIR"), "api-models-aggregate", &output);
    }

    // Hard-won: #1566: a timed-out quota feed left the control client unusable for disabled-profile probes.
    #[tokio::test]
    async fn quota_feed_timeout_preserves_the_control_connection() {
        use mj_client::daemon::{
            DaemonAction, DaemonClient, DaemonMetadata, DaemonPlacement, DaemonReply,
            PROTOCOL_VERSION, RequestEnvelope, ResponseEnvelope, read_frame, write_frame,
        };
        use tokio::io::AsyncReadExt;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let metadata = DaemonMetadata {
            protocol_version: PROTOCOL_VERSION,
            pid: std::process::id(),
            address: listener.local_addr().unwrap(),
            token: "quota-feed-timeout".into(),
            started_at: "test".into(),
            build_version: env!("CARGO_PKG_VERSION").into(),
            instance_id: String::new(),
            placement: DaemonPlacement::Host,
        };
        let (received, waiting) = tokio::sync::oneshot::channel();
        let server = mj_core::runtime::spawn(async move {
            let (mut control, _) = listener.accept().await.unwrap();
            let (mut feed, _) = listener.accept().await.unwrap();
            let request: RequestEnvelope = read_frame(&mut feed).await.unwrap();
            assert!(matches!(
                request.action,
                DaemonAction::RuntimeChanges { .. }
            ));
            received.send(()).unwrap();
            let mut byte = [0];
            assert_eq!(feed.read(&mut byte).await.unwrap(), 0);
            let request: RequestEnvelope = read_frame(&mut control).await.unwrap();
            assert!(matches!(
                request.action,
                DaemonAction::ProbeQuotaProfiles { .. }
            ));
            write_frame(
                &mut control,
                &ResponseEnvelope {
                    protocol_version: request.protocol_version,
                    request_id: request.request_id,
                    result: Ok(DaemonReply::Done),
                },
            )
            .await
            .unwrap();
        });
        let mut control = DaemonClient::connect(metadata.clone()).await.unwrap();
        let feed = DaemonClient::connect(metadata).await.unwrap();
        let observed = mj_client::runtime_feed::RuntimeMetadata::default();
        let pump = pump_quota_feed(
            feed,
            None,
            observed.clone(),
            std::time::Instant::now() + QUOTA_REFRESH_WAIT,
            |_| false,
        );
        tokio::pin!(pump);
        tokio::select! {
            result = &mut pump => panic!("feed reply remains pending: {result:?}"),
            result = waiting => result.unwrap(),
        }
        tokio::time::pause();
        tokio::time::advance(QUOTA_REFRESH_WAIT + std::time::Duration::from_secs(1)).await;
        let (_, latest) = pump.await.unwrap();
        assert_eq!(latest, observed);
        control
            .probe_quota_profiles(vec!["disabled".into()], false)
            .await
            .unwrap();
        server.await.unwrap();
    }

    #[test]
    fn api_quota_matches_cli_golden() {
        use mj_client::quota::{PaidCredits, ProfileQuota, QuotaSnapshot, QuotaWindow};
        use mj_core::config::{HarnessKind, HarnessProfile};
        let now = 1_800_000_000u64;
        let window = |label: &str,
                      remaining: Option<u8>,
                      used: Option<i64>,
                      limit: Option<i64>,
                      resets_in: Option<u64>| QuotaWindow {
            label: label.into(),
            remaining_percent: remaining,
            duration_seconds: None,
            model_scope: None,
            used,
            limit,
            resets: None,
            resets_at_epoch_seconds: resets_in.map(|delta| (now + delta) as i64),
        };
        let report = |profile: &str,
                      harness: HarnessKind,
                      windows: Vec<QuotaWindow>,
                      extra: Option<&str>,
                      error: Option<&str>,
                      age: u64| ProfileQuota {
            paid_credits: None,
            monthly_price_usd: None,
            banked_resets: None,
            profile_id: profile.into(),
            harness,
            windows,
            extra: extra.map(str::to_owned),
            error: error.map(str::to_owned),
            refreshed_at_epoch_seconds: now - age,
            rate_limited_until_epoch_seconds: None,
            next_check_at_epoch_seconds: None,
        };
        let mut config = mj_core::config::Config::default();
        for (id, kind, enabled) in [
            ("agy", HarnessKind::Antigravity, true),
            ("claude", HarnessKind::Claude, true),
            ("codex", HarnessKind::Codex, true),
            ("kimi", HarnessKind::Kimi, true),
            ("muse", HarnessKind::Muse, true),
            ("opencode", HarnessKind::OpenCode, true),
            ("dsh", HarnessKind::Dsh, false),
        ] {
            config.profiles.insert(
                id.into(),
                HarnessProfile {
                    enabled,
                    ..HarnessProfile::new(kind, format!("/home/agent/.{id}"))
                },
            );
        }
        let mut claude = report(
            "claude",
            HarnessKind::Claude,
            vec![
                window("5H", Some(62), None, None, Some(2 * 3600 + 41 * 60)),
                window(
                    "Week",
                    None,
                    Some(340),
                    Some(500),
                    Some(3 * 24 * 3600 + 4 * 3600),
                ),
            ],
            None,
            None,
            240,
        );
        claude.monthly_price_usd = Some(100);
        // A Pro plan reports Argon apart, and an unused pool has no reset.
        let pool_window = |pool: mj_core::config::AntigravityQuotaPool,
                           label: &str,
                           remaining: u8,
                           resets_in: Option<u64>| QuotaWindow {
            model_scope: Some(pool.reported_scope(true).into()),
            ..window(label, Some(remaining), None, None, resets_in)
        };
        let agy = {
            use mj_core::config::AntigravityQuotaPool::{Argon, Claude, Gemini};
            report(
                "agy",
                HarnessKind::Antigravity,
                vec![
                    pool_window(Gemini, "Week", 73, Some(6 * 24 * 3600)),
                    pool_window(Gemini, "5H", 100, Some(5 * 3600)),
                    pool_window(Claude, "Week", 100, None),
                    pool_window(Claude, "5H", 100, None),
                    pool_window(Argon, "Week", 40, Some(6 * 24 * 3600 + 23 * 3600)),
                ],
                None,
                None,
                90,
            )
        };
        let mut codex = report(
            "codex",
            HarnessKind::Codex,
            vec![],
            Some(mj_client::quota::API_LABEL),
            None,
            60,
        );
        codex.paid_credits = PaidCredits::limited(43.2);
        let kimi = report(
            "kimi",
            HarnessKind::Kimi,
            vec![],
            Some(mj_client::quota::NOT_REPORTED_LABEL),
            None,
            30,
        );
        let mut grok_profile = HarnessProfile::new(HarnessKind::Grok, "/home/agent/.grok");
        grok_profile.enabled = true;
        config.profiles.insert("grok".into(), grok_profile);
        let grok = report(
            "grok",
            HarnessKind::Grok,
            vec![],
            None,
            Some("login expired"),
            600,
        );
        let snapshot = QuotaSnapshot {
            reports: [
                ("agy".to_owned(), agy),
                ("claude".to_owned(), claude),
                ("codex".to_owned(), codex),
                ("kimi".to_owned(), kimi),
                ("grok".to_owned(), grok),
            ]
            .into_iter()
            .collect(),
            probing: ["opencode".to_owned()].into_iter().collect(),
            cycles: 7,
        };
        let metadata = mj_client::runtime_feed::RuntimeMetadata {
            config: config.into(),
            quotas: snapshot.into(),
            ..Default::default()
        };
        let mut output = String::from("=== mj quota text ===\n");
        output.push_str(&render_quota_at(&metadata, None, false, false, now).unwrap());
        output.push_str("\n=== mj quota --profile claude ===\n");
        output.push_str(&render_quota_at(&metadata, Some("claude"), false, false, now).unwrap());
        output.push_str("\n=== mj quota --json ===\n");
        output.push_str(&render_quota_at(&metadata, None, false, true, now).unwrap());
        output.push_str("\n=== mj quota --with-disabled ===\n");
        output.push_str(&render_quota_at(&metadata, None, true, false, now).unwrap());
        output.push_str("\n=== mj quota --profile missing ===\n");
        output.push_str(
            &render_quota_at(&metadata, Some("missing"), false, false, now)
                .unwrap_err()
                .to_string(),
        );
        mj_core::golden::assert_golden(env!("CARGO_MANIFEST_DIR"), "api-quota", &output);
    }
    #[test]
    fn api_hosts_matches_cli_golden() {
        use mj_core::targets::storage::StorageCondition;
        use mj_core::targets::{DeploymentCapacityKind, HostCapacityRow, HostFilesystemRow};
        const GIB: u64 = 1 << 30;
        let now = 1_700_000_000;
        let filesystem =
            |host: &str, mount: &str, available: u64, total: u64, condition| HostFilesystemRow {
                host: host.into(),
                mount: mount.into(),
                available_bytes: available,
                total_bytes: total,
                condition,
            };
        let row = |id: &str, kind, targets: &[&str]| HostCapacityRow {
            ec2_instance_type: None,
            physical_memory: None,
            id: id.into(),
            host: id.into(),
            target_ids: targets.iter().map(|id| (*id).to_owned()).collect(),
            kind,
            fleet_instances: None,
            has_reading: true,
            cpu_percent: Some(12),
            memory_used_bytes: 14 * GIB,
            memory_total_bytes: 31 * GIB,
            logical_cores: 16,
            filesystems: Vec::new(),
            sampled_at_epoch_seconds: Some(now - 20),
            probe_error: None,
            on_demand: false,
        };
        let mut local = row("local", DeploymentCapacityKind::Host, &["local", "podman"]);
        local.filesystems = vec![
            filesystem("local", "/", 300 * GIB, 900 * GIB, StorageCondition::Ok),
            filesystem(
                "local",
                "/mnt/optane",
                3 * GIB,
                400 * GIB,
                StorageCondition::Low,
            ),
        ];
        let mut build = row("build.example", DeploymentCapacityKind::Host, &["build"]);
        build.cpu_percent = None;
        build.sampled_at_epoch_seconds = Some(now - 600);
        build.probe_error = Some("ssh: connection timed out".into());
        build.filesystems = vec![filesystem(
            "build.example",
            "/srv",
            GIB / 2,
            200 * GIB,
            StorageCondition::Full,
        )];
        let mut fleet = row("ec2-fleet", DeploymentCapacityKind::AwsFleet, &["ec2"]);
        fleet.fleet_instances = Some(2);
        fleet.cpu_percent = None;
        fleet.logical_cores = 96;
        fleet.memory_total_bytes = 384 * GIB;
        fleet.filesystems = vec![
            filesystem(
                "i-0a1",
                "/mnt/ebs",
                80 * GIB,
                100 * GIB,
                StorageCondition::Ok,
            ),
            filesystem(
                "i-0b2",
                "/mnt/ebs",
                60 * GIB,
                100 * GIB,
                StorageCondition::Ok,
            ),
        ];
        let mut idle = row("ec2-idle", DeploymentCapacityKind::AwsFleet, &["ec2-spare"]);
        idle.fleet_instances = Some(0);
        idle.has_reading = false;
        idle.on_demand = true;
        // The fleet last answered empty, then its probe failed: the size is
        // unknown, so the row must not claim "0 VMs".
        let mut failed_idle = row(
            "ec2-failed",
            DeploymentCapacityKind::AwsFleet,
            &["ec2-broken"],
        );
        failed_idle.fleet_instances = Some(0);
        failed_idle.has_reading = false;
        failed_idle.probe_error = Some("describe-instances: access denied".into());
        let mut fresh = row("new.example", DeploymentCapacityKind::Host, &["new"]);
        fresh.has_reading = false;
        fresh.sampled_at_epoch_seconds = None;
        // The rows travel from the daemon as JSON.
        let rows: Vec<HostCapacityRow> = serde_json::from_value(
            serde_json::to_value(vec![local, build, fleet, idle, failed_idle, fresh]).unwrap(),
        )
        .unwrap();
        let mut output = String::from("=== mj hosts ===\n");
        output.push_str(&render_hosts(&rows, None, false, now).unwrap());
        output.push_str("\n=== mj hosts --host podman ===\n");
        output.push_str(&render_hosts(&rows, Some("podman"), false, now).unwrap());
        output.push_str("\n=== mj hosts --host ec2-fleet --json ===\n");
        output.push_str(&render_hosts(&rows, Some("ec2-fleet"), true, now).unwrap());
        output.push_str("\n=== mj hosts --host missing ===\n");
        output.push_str(
            &render_hosts(&rows, Some("missing"), false, now)
                .unwrap_err()
                .to_string(),
        );
        mj_core::golden::assert_golden(env!("CARGO_MANIFEST_DIR"), "api-hosts", &output);
    }

    #[test]
    fn golden_suspend_refusal_and_dry_run_output() {
        let refusal = mj_core::suspend_consent::format_untracked_refusal(
            6,
            &[
                "a.txt".into(),
                "b.txt".into(),
                "c.txt".into(),
                "d.txt".into(),
                "e.txt".into(),
                "f.txt".into(),
            ],
        );
        let dry_run_clean = format_suspend_dry_run(&mj_core::suspend_consent::SuspendPreflight {
            session_id: "session-1".into(),
            untracked: vec![mj_core::suspend_consent::UntrackedPath {
                checkout: mj_core::suspend_consent::CheckoutKey::default(),
                path: std::path::PathBuf::from("scratch.txt"),
                bytes: 512,
                identity: mj_core::suspend_consent::FileIdentity::new("abc".into()),
            }],
            keep_dir_bytes: 2048,
            dry_run: Some(mj_core::suspend_consent::DryRunEstimate {
                git_delta_bytes: Some(1024 * 64),
                git_delta_unavailable_reason: None,
                session_state_bytes: Some(4096),
                keep_dir_bytes: 2048,
                transfer_route: None,
            }),
            ..Default::default()
        });
        let dry_run_rebase = format_suspend_dry_run(&mj_core::suspend_consent::SuspendPreflight {
            session_id: "session-2".into(),
            operations: vec![mj_core::suspend_consent::ShownOperation {
                checkout: mj_core::suspend_consent::CheckoutKey::default(),
                kind: mj_core::suspend_consent::GitOperationKind::Rebase,
            }],
            dry_run: Some(mj_core::suspend_consent::DryRunEstimate {
                git_delta_bytes: None,
                git_delta_unavailable_reason: Some(
                    "Git size estimate unavailable during rebase".into(),
                ),
                session_state_bytes: None,
                keep_dir_bytes: 0,
                transfer_route: None,
            }),
            ..Default::default()
        });
        let output = format!(
            "=== untracked refusal ===\n{refusal}\n\n=== dry run ===\n{dry_run_clean}\n=== dry run during rebase ===\n{dry_run_rebase}"
        );
        mj_core::golden::assert_golden(env!("CARGO_MANIFEST_DIR"), "api-suspend-consent", &output);
    }

    #[test]
    fn suspend_preflight_query_includes_without_checkpoint() {
        assert_eq!(ApiClient::suspend_preflight_query(false, false), "");
        assert_eq!(
            ApiClient::suspend_preflight_query(true, false),
            "?dry_run=true"
        );
        assert_eq!(
            ApiClient::suspend_preflight_query(false, true),
            "?without_checkpoint=true"
        );
        assert_eq!(
            ApiClient::suspend_preflight_query(true, true),
            "?dry_run=true&without_checkpoint=true"
        );
    }

    #[test]
    fn purge_untracked_parses_its_selector_and_renders_each_session() {
        let parsed = Cli::try_parse_from([
            "mj",
            "purge-untracked",
            "--session",
            "session-1",
            "--dry-run",
            "--json",
        ])
        .unwrap();
        let Some(Command::PurgeUntracked(args)) = parsed.command else {
            panic!("expected purge-untracked");
        };
        assert_eq!(args.session.as_deref(), Some("session-1"));
        assert!(!args.all);
        assert!(args.dry_run);
        assert!(args.json);

        let parsed = Cli::try_parse_from(["mj", "purge-untracked", "--all"]).unwrap();
        let Some(Command::PurgeUntracked(args)) = parsed.command else {
            panic!("expected purge-untracked");
        };
        assert!(args.session.is_none());
        assert!(args.all);
        assert!(!args.dry_run);
        assert!(!args.json);

        assert!(Cli::try_parse_from(["mj", "purge-untracked"]).is_err());
        assert!(
            Cli::try_parse_from(["mj", "purge-untracked", "--session", "session-1", "--all"])
                .is_err()
        );
        assert!(Cli::try_parse_from(["mj", "purge-untracked", "--dry-run"]).is_err());

        let response = sample_purge_report(false);
        assert_eq!(
            purge_untracked_lines(&response),
            [
                "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb: 4096 reclaimed",
                "  stash project abcdef (WIP notes)",
                "  warning: could not read one stash",
                "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa: skipped (lifecycle operation in progress)",
                "Total: 4096 reclaimed",
            ]
        );
        let dry = sample_purge_report(true);
        assert_eq!(
            purge_untracked_lines(&dry),
            [
                "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb: would reclaim 4096",
                "  stash project abcdef (WIP notes)",
                "  warning: could not read one stash",
                "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa: skipped (lifecycle operation in progress)",
                "Total: 4096 would be reclaimed",
            ]
        );
        assert_eq!(
            serde_json::to_string_pretty(&response).unwrap(),
            r#"{
  "sessions": [
    {
      "session_id": "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
      "status": "purged",
      "bytes_reclaimed": 4096,
      "reason": null,
      "stash_untracked_notes": [
        {
          "repository_id": "project",
          "commit": "abcdef",
          "message": "WIP notes"
        }
      ],
      "warnings": [
        "could not read one stash"
      ]
    },
    {
      "session_id": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
      "status": "skipped",
      "bytes_reclaimed": 0,
      "reason": "lifecycle operation in progress",
      "stash_untracked_notes": [],
      "warnings": []
    }
  ],
  "total_bytes_reclaimed": 4096,
  "dry_run": false
}"#
        );
        let mut failed = response.clone();
        failed
            .sessions
            .push(mj_daemon::server::api::PurgeUntrackedSession {
                session_id: "cccccccccccccccccccccccccccccccc".into(),
                status: PurgeUntrackedStatus::Failed,
                bytes_reclaimed: 0,
                reason: Some("checkpoint archive is missing".into()),
                stash_untracked_notes: Vec::new(),
                warnings: Vec::new(),
            });
        let lines = purge_untracked_lines(&failed);
        assert_eq!(
            lines[lines.len() - 2],
            "cccccccccccccccccccccccccccccccc: failed (checkpoint archive is missing)"
        );
        assert!(lines.last().unwrap().ends_with("4096 reclaimed"));
        let json = serde_json::to_string_pretty(&failed).unwrap();
        assert!(json.contains("\"status\": \"failed\""));
        assert!(json.contains("checkpoint archive is missing"));
    }

    #[test]
    fn purge_untracked_all_prints_the_full_report_and_fails_when_a_session_failed() {
        let parsed = Cli::try_parse_from(["mj", "purge-untracked", "--all"]).unwrap();
        let Some(Command::PurgeUntracked(args)) = parsed.command else {
            panic!("expected purge-untracked");
        };
        assert!(args.all);
        assert!(!args.json);

        let mut response = sample_purge_report(false);
        response
            .sessions
            .push(mj_daemon::server::api::PurgeUntrackedSession {
                session_id: "cccccccccccccccccccccccccccccccc".into(),
                status: PurgeUntrackedStatus::Failed,
                bytes_reclaimed: 0,
                reason: Some("checkpoint archive is missing".into()),
                stash_untracked_notes: Vec::new(),
                warnings: Vec::new(),
            });
        let (report, status) = purge_untracked_conclusion(&response, false);
        assert!(report.contains("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb: 4096 reclaimed"));
        assert!(
            report.contains(
                "cccccccccccccccccccccccccccccccc: failed (checkpoint archive is missing)"
            )
        );
        assert!(report.contains("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa: skipped"));
        assert!(report.ends_with("Total: 4096 reclaimed"));
        let error = status.expect_err("a failed session fails the command");
        assert_eq!(
            error.to_string(),
            "1 session(s) failed to purge untracked payloads"
        );

        let (json, status) = purge_untracked_conclusion(&response, true);
        assert!(json.contains("\"status\": \"purged\""));
        assert!(json.contains("\"status\": \"failed\""));
        assert!(json.contains("checkpoint archive is missing"));
        assert!(json.contains("\"bytes_reclaimed\": 4096"));
        assert_eq!(
            status
                .expect_err("json still fails after the report")
                .to_string(),
            "1 session(s) failed to purge untracked payloads"
        );

        let (clean, status) = purge_untracked_conclusion(&sample_purge_report(false), false);
        assert!(clean.contains("4096 reclaimed"));
        assert!(status.is_ok());
    }

    fn sample_purge_report(dry_run: bool) -> PurgeUntrackedResponse {
        use mj_daemon::server::api::PurgeUntrackedSession;
        use mj_daemon::server::api::PurgeUntrackedStashNote;
        PurgeUntrackedResponse {
            sessions: vec![
                PurgeUntrackedSession {
                    session_id: "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".into(),
                    status: if dry_run {
                        PurgeUntrackedStatus::DryRun
                    } else {
                        PurgeUntrackedStatus::Purged
                    },
                    bytes_reclaimed: 4096,
                    reason: None,
                    stash_untracked_notes: vec![PurgeUntrackedStashNote {
                        repository_id: "project".into(),
                        commit: "abcdef".into(),
                        message: "WIP notes".into(),
                    }],
                    warnings: vec!["could not read one stash".into()],
                },
                PurgeUntrackedSession {
                    session_id: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into(),
                    status: PurgeUntrackedStatus::Skipped,
                    bytes_reclaimed: 0,
                    reason: Some("lifecycle operation in progress".into()),
                    stash_untracked_notes: Vec::new(),
                    warnings: Vec::new(),
                },
            ],
            total_bytes_reclaimed: 4096,
            dry_run,
        }
    }
}
