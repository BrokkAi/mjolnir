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
    ApiSession, ExportKind, ExportRequest, RelayState, ResumeSessionRequest, StartSessionRequest,
    WaitOutcome, WaitRequest, WaitResponse,
};

use mj_client::daemon::WikiSessionStatus;
use mj_controller::server::ViewerLifecycleCategory;
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
    /// Profile the session runs its harness from. Omit it to use the saved
    /// default: the pair `mj go` saved, or else the one the first session was
    /// created with.
    #[arg(long)]
    profile: Option<String>,
    /// Target template the session is provisioned on. Omit it to use the
    /// saved default, as for `--profile`.
    #[arg(long)]
    target: Option<String>,
    /// Existing bundle to run. Bare targets use `--project-directory` instead.
    #[arg(long)]
    bundle: Option<String>,
    /// Existing directory on the selected bare target to run the session against.
    #[arg(long)]
    project_directory: Option<PathBuf>,
    /// Start the workspace checked out at this full commit ID in the bundle's primary repository.
    #[arg(long, value_name = "SHA", requires = "bundle")]
    at: Option<String>,
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
    /// Harness model to select before the first prompt.
    #[arg(long)]
    model: Option<String>,
    /// Harness reasoning effort to select before the first prompt.
    #[arg(long)]
    effort: Option<String>,
    /// Delegation policy. Omitted reuses the last accepted new-session choice.
    #[arg(long, value_parser = ["native", "all-models", "single-model", "none"])]
    subagents: Option<String>,
    /// Fixed child model, required with --subagents single-model.
    #[arg(long, requires = "subagents")]
    subagent_model: Option<String>,
    /// Fixed child reasoning effort.
    #[arg(long, requires = "subagents")]
    subagent_effort: Option<String>,
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
    /// Session id, as `mj sessions` lists it.
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
    /// Print the response as JSON instead of text.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
pub(crate) struct WaitArgs {
    /// Session id, as `mj sessions` lists it.
    #[arg(long)]
    session: String,
    /// Wait until this turn, as `mj prompt` printed it, or a later one, has
    /// ended. The answer describes the latest turn that ended (a session keeps
    /// only that turn's outcome and reply), and `--json` reports it as
    /// `turn_id` beside `requested_turn_id`. Both forms honor the worker's
    /// completion assessment, including expected continuation. Omit it to
    /// wait until the session is idle with nothing queued.
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
    /// Resume from the highest sequence already read.
    #[arg(long)]
    after_seq: Option<u64>,
    /// Most items to return in one page.
    #[arg(long)]
    limit: Option<usize>,
    /// Return only items with this role: user, agent, thought, tool, terminal,
    /// plan, plan_proposal, or system.
    #[arg(long, value_parser = parse_transcript_role)]
    role: Option<mj_core::transcript::TranscriptRole>,
    /// Print the response as JSON instead of text.
    #[arg(long)]
    json: bool,
}

fn parse_transcript_role(value: &str) -> Result<mj_core::transcript::TranscriptRole, String> {
    serde_json::from_value(serde_json::Value::String(value.into())).map_err(|e| e.to_string())
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
        for line in usage_tree_lines(&tree) {
            println!("{line}");
        }
    } else {
        let session = args
            .session
            .context("usage requires --session or --parent")?;
        let page = client.usage(&session, args.after_seq, args.limit).await?;
        if args.json {
            return print_json(&page);
        }
        for line in usage_lines(&page) {
            println!("{line}");
        }
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
pub(crate) struct ExportArgs {
    /// Session id, as `mj sessions` lists it.
    #[arg(long)]
    session: String,
    /// What to export: a patch, a pushed branch, a git bundle, or one file.
    #[arg(long, value_enum, default_value_t = ExportKindArg::Patch)]
    kind: ExportKindArg,
    /// Branch to push, required by `--kind branch`.
    #[arg(long, required_if_eq("kind", "branch"))]
    branch: Option<String>,
    /// File to read, relative to the directory the agent runs in, required by
    /// `--kind file`.
    #[arg(long)]
    path: Option<String>,
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
    /// One file from the session workspace, which the API serves on its own
    /// route rather than as an export kind.
    File,
}

#[derive(Debug, Args)]
pub(crate) struct SessionsArgs {
    /// Show one session, including how its last turn ended.
    #[arg(long)]
    session: Option<String>,
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
    /// Confirm releasing a clone whose saved Git work is not verified as pushed.
    #[arg(long)]
    acknowledge_unpublished_work: bool,
    /// Print the response as JSON instead of text.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
pub(crate) struct DestroyArgs {
    /// Session id, as `mj sessions` lists it.
    #[arg(long)]
    session: String,
    /// Also delete the managed branch. Work held only in the environment is lost either way.
    #[arg(long)]
    delete_branch: bool,
    /// Print the response as JSON instead of text.
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

fn new_subagent_policy(args: &NewArgs) -> Result<Option<mj_core::subagent::SubagentPolicy>> {
    use mj_core::subagent::SubagentPolicy;
    if args.subagents.as_deref() != Some("single-model")
        && (args.subagent_model.is_some() || args.subagent_effort.is_some())
    {
        bail!("--subagent-model and --subagent-effort require --subagents single-model");
    }
    Ok(match args.subagents.as_deref() {
        Some("native") => Some(SubagentPolicy::Native),
        Some("all-models") => Some(SubagentPolicy::AllModels),
        Some("single-model") => Some(SubagentPolicy::SingleModel {
            model: args
                .subagent_model
                .clone()
                .context("--subagents single-model requires --subagent-model")?,
            effort: args.subagent_effort.clone(),
        }),
        Some("none") => Some(SubagentPolicy::None),
        _ => None,
    })
}

/// Create a session and deliver its optional first prompt.
pub(crate) async fn new_session(args: NewArgs, requested_workspace: Option<String>) -> Result<()> {
    let prompt = read_prompt(args.prompt.clone(), args.prompt_file.clone())?;
    if args.bundle.is_none() && args.project_directory.is_none() {
        bail!("pass --bundle, --project-directory, or both");
    }
    // Every session lives in a workspace the dashboard and the viewer list,
    // so the command names one (launch finding H-3).
    let workspace_id = match (&args.workspace_id, requested_workspace.as_deref()) {
        (Some(workspace_id), _) => Some(workspace_id.clone()),
        (None, Some(name)) => {
            // An unknown name is refused without starting a stopped daemon,
            // as a missing one is (launch findings R2-14 and R5-9).
            crate::refuse_unknown_workspace(name).await?;
            Some(crate::resolve_store_workspace(Some(name)).await?)
        }
        (None, None) => return Err(crate::workspace_required("mj new").await),
    };
    let request = StartSessionRequest {
        subagents: new_subagent_policy(&args)?,
        create_managed_worktree: None,
        at: args.at.clone(),
        branch: args.branch.clone(),
        base: args.base.clone(),
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
    let response = client.start(&request).await.map_err(name_launch_flags)?;
    if args.json {
        return print_json(&response);
    }
    println!("session {}", response.session_id);
    if let Some(turn_id) = response.turn_id {
        println!("turn {turn_id}");
    }
    Ok(())
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
        .replace("`at`", "--at")
        .replace("`branch`", "--branch")
        .replace("`base`", "--base");
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
    lines.push(summary);
    // A wait for turn N answers with the latest turn that ended, which is a
    // later turn when the session has moved on. Say so rather than let the
    // reply read as turn N's.
    if let (Some(requested), Some(answered)) = (response.requested_turn_id, response.turn_id)
        && answered > requested
    {
        lines.push(format!(
            "this is turn {answered}, which ended after turn {requested}; a session keeps only its latest turn's outcome and reply"
        ));
    }
    // The wait's message, the diagnostic and the agent's last message are
    // often one sentence (a Codex quota error was printed three times,
    // J-25), so each is printed only when it says something new.
    let said = |lines: &[String], text: &str| lines.iter().any(|line| line.trim() == text.trim());
    if let Some(message) = &response.message {
        lines.push(message.clone());
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
        session.id, session.state, session.title
    )];
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
    if let Some(outcome) = &session.last_turn_outcome {
        lines.push(format!("last turn {}", outcome.outcome));
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
    print!("{}", details.diff);
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
        ExportKindArg::File => {
            let path = args
                .path
                .as_deref()
                .context("`--kind file` needs --path, relative to the agent's directory")?;
            let bytes = client.read_file(&args.session, path).await?;
            return write_bytes(&bytes, args.out.as_deref(), args.json, "file");
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
        for line in session_report_lines(&session, mj_core::clock::epoch_millis()) {
            println!("{line}");
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
    for line in wiki_session_lines(&info) {
        println!("{line}");
    }
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
    let accepted = ApiClient::connect()
        .await?
        .suspend(session, args.acknowledge_unpublished_work)
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

/// Permanently destroy the session, environment, and recovery archive.
pub(crate) async fn destroy(args: DestroyArgs) -> Result<()> {
    ApiClient::connect()
        .await?
        .destroy(&args.session, args.delete_branch)
        .await?;
    if args.json {
        print_json(
            &serde_json::json!({"session_id": args.session, "accepted": true, "operation": "destroy"}),
        )
    } else {
        println!("destruction accepted for {}", args.session);
        println!(
            "it finishes in the background; check removal with `mj sessions --session {}`",
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
                None => crate::resolve_store_workspace(args.workspace.name.as_deref()).await?,
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

fn print_json<T: serde::Serialize>(value: &T) -> Result<()> {
    println!(
        "{}",
        serde_json::to_string_pretty(value).context("serialize the API response")?
    );
    Ok(())
}

#[derive(Debug, Args)]
pub(crate) struct ModelsArgs {
    /// Profile whose harness to ask.
    #[arg(long)]
    profile: String,
    /// List the efforts this model offers instead of the default model's.
    #[arg(long)]
    model: Option<String>,
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
    let choices = ApiClient::connect()
        .await?
        .models(&args.profile, args.model)
        .await?;
    if args.json {
        return print_json(&choices);
    }
    if choices.models.is_empty() && choices.efforts.is_empty() {
        println!("this profile's harness reports no models or efforts");
    }
    for model in &choices.models {
        println!("{}  {}", model.value, model.name);
    }
    // A harness without efforts, or one whose efforts are not known yet,
    // printed a bare "effort (default):" line, which reads as a value that
    // failed to print (F-16).
    if !choices.efforts.is_empty() {
        println!(
            "effort ({}): {}",
            choices.model.as_deref().unwrap_or("default model"),
            choices
                .efforts
                .iter()
                .map(|c| c.value.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    Ok(())
}
pub(crate) async fn set_config(args: SetConfigArgs) -> Result<()> {
    let client = ApiClient::connect().await?;
    let session = match (args.key, args.value) {
        (Some(key), Some(value)) => {
            client
                .set_config(
                    &args.session,
                    &mj_controller::server::api::SetConfigRequest { key, value },
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
    for line in config_option_lines(&session.config_options) {
        println!("{line}");
    }
    Ok(())
}

/// One line per setting: its key, its current value, and what it accepts.
fn config_option_lines(options: &[mj_controller::server::ViewerConfigOption]) -> Vec<String> {
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

    #[test]
    fn new_accepts_a_launch_base_and_leaves_it_unset_otherwise() {
        let cli = Cli::try_parse_from([
            "mj",
            "new",
            "--profile",
            "codex",
            "--target",
            "raw",
            "--project-directory",
            "/srv/project",
            "--base",
            "HEAD~1",
        ])
        .unwrap();
        let Some(Command::New(args)) = cli.command else {
            panic!("expected the new command");
        };
        assert_eq!(args.base.as_deref(), Some("HEAD~1"));

        let cli = Cli::try_parse_from([
            "mj",
            "new",
            "--profile",
            "codex",
            "--target",
            "raw",
            "--project-directory",
            "/srv/project",
        ])
        .unwrap();
        let Some(Command::New(args)) = cli.command else {
            panic!("expected the new command");
        };
        assert_eq!(args.base, None);
    }

    #[test]
    fn new_at_requires_a_bundle_and_takes_an_optional_branch_and_base() {
        let commit = "0123456789abcdef0123456789abcdef01234567";
        let parse = |extra: &[&str]| {
            let mut argv = vec!["mj", "new", "--workspace", "town"];
            argv.extend_from_slice(extra);
            Cli::try_parse_from(argv)
        };
        let error = parse(&["--at", commit]).expect_err("--at without --bundle is refused");
        assert!(error.to_string().contains("--bundle"), "{error}");

        let Some(Command::New(args)) = parse(&["--bundle", "product", "--at", commit])
            .unwrap()
            .command
        else {
            panic!("expected the new command");
        };
        assert_eq!(args.at.as_deref(), Some(commit));
        assert_eq!((args.branch, args.base), (None, None));

        let Some(Command::New(args)) = parse(&[
            "--bundle",
            "product",
            "--at",
            commit,
            "--branch",
            "town/run-1",
            "--base",
            "v1.0",
        ])
        .unwrap()
        .command
        else {
            panic!("expected the new command");
        };
        assert_eq!(args.branch.as_deref(), Some("town/run-1"));
        assert_eq!(args.base.as_deref(), Some("v1.0"));
    }

    #[test]
    fn a_refused_start_names_quoted_fields_as_flags() {
        let error = name_launch_flags(anyhow::anyhow!(
            "`at` requires bundle_id: it checks out the bundle's primary repository"
        ));
        assert_eq!(
            error.to_string(),
            "--at requires --bundle: it checks out the bundle's primary repository"
        );
    }

    #[test]
    fn new_subagent_policy_rejects_legacy_flags_and_requires_fixed_model() {
        let parse = |extra: &[&str]| {
            let mut argv = vec![
                "mj",
                "new",
                "--profile",
                "codex",
                "--target",
                "raw",
                "--project-directory",
                "/srv/project",
            ];
            argv.extend_from_slice(extra);
            let cli = Cli::try_parse_from(argv).map_err(anyhow::Error::from)?;
            let Some(Command::New(args)) = cli.command else {
                panic!("new command");
            };
            new_subagent_policy(&args)
        };
        use mj_core::subagent::SubagentPolicy;
        assert_eq!(parse(&[]).unwrap(), None);
        assert_eq!(
            parse(&["--subagents", "none"]).unwrap(),
            Some(SubagentPolicy::None)
        );
        assert_eq!(
            parse(&[
                "--subagents",
                "single-model",
                "--subagent-model",
                "model",
                "--subagent-effort",
                "high"
            ])
            .unwrap(),
            Some(SubagentPolicy::SingleModel {
                model: "model".into(),
                effort: Some("high".into())
            })
        );
        for args in [
            &["--mj-subagents"][..],
            &["--native-subagents"],
            &["--subagents", "single-model"],
            &["--subagents", "native", "--subagent-model", "model"],
        ] {
            assert!(parse(args).is_err());
        }
    }

    /// A turn the worker failed for going quiet has to say why, where a script
    /// waiting on it can see it. Before this the reason lived only in the
    /// transcript and `mj wait` printed the bare word "error" (#1020).
    /// Launch finding J-25: `mj prompt --wait` printed the Codex quota
    /// sentence three times, as the wait's message, the diagnostic, and the
    /// agent's final message. Each distinct line is printed once.
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

    /// Launch finding R11-3: `mj sessions --session` printed `last turn
    /// Completed { stop_reason: "EndTurn" }`, Rust's debug form. It says how
    /// the turn ended in words.
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
    /// turn". `mj wait` and `mj prompt --wait` use the same words, placed as
    /// the notice places them; `--json` keeps the stop reason unchanged.
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
            serde_json::json!({"turn_id": 87, "requested_turn_id": 82}),
        );
        assert!(
            wait_report_lines(&later)[1].contains("turn 87, which ended after turn 82"),
            "{:?}",
            wait_report_lines(&later)
        );
        let same = wait_response(
            "finished",
            serde_json::json!({"turn_id": 82, "requested_turn_id": 82}),
        );
        assert_eq!(wait_report_lines(&same).len(), 1);
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

    #[test]
    fn usage_selects_one_session_or_a_whole_tree_and_pages_only_sessions() {
        #[derive(clap::Parser)]
        struct UsageCommand {
            #[command(flatten)]
            usage: UsageArgs,
        }
        let parse = |args: &[&str]| <UsageCommand as clap::Parser>::try_parse_from(args);
        assert!(
            parse(&[
                "usage",
                "--session",
                "s",
                "--after-seq",
                "1",
                "--limit",
                "2"
            ])
            .is_ok()
        );
        assert!(parse(&["usage", "--parent", "p", "--json"]).is_ok());
        for args in [
            vec!["usage"],
            vec!["usage", "--session", "s", "--parent", "p"],
            vec!["usage", "--parent", "p", "--limit", "2"],
            vec!["usage", "--parent", "p", "--after-seq", "1"],
        ] {
            assert!(parse(&args).is_err(), "{args:?}");
        }
    }

    #[test]
    fn usage_tree_text_distinguishes_models_efforts_and_removed_children() {
        let tree: mj_core::storage::UsageTree=serde_json::from_value(serde_json::json!({
            "parent_session_id":"parent", "totals":{},
            "coverage":{"recorded_turns":2,"full_turn_reports":2,"last_request_reports":0,"unspecified_reports":0,"missing_reports":0},
            "by_model":[{"model":"sol","effort":"high","totals":{"input_tokens":{"tokens":10,"reported_turns":1}}},
                {"model":"luna","effort":"low","totals":{"input_tokens":{"tokens":20,"reported_turns":1}}}],
            "sessions":[{"session_id":"child","parent_session_id":"parent","task_name":"investigate",
                "operational_session_present":false,"totals":{},"by_model":[],
                "coverage":{"recorded_turns":0,"full_turn_reports":0,"last_request_reports":0,"unspecified_reports":0,"missing_reports":0}}]
        })).unwrap();
        let text = usage_tree_lines(&tree).join("\n");
        assert!(text.contains("model sol; effort high:"));
        assert!(text.contains("model luna; effort low:"));
        assert!(
            text.contains("task investigate; parent parent; accounting retained after cleanup")
        );
    }

    /// F-16: `mj usage` printed JSON unless asked for text.
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
        assert_eq!(args.profile.as_deref(), Some("codex"));
        assert_eq!(args.target.as_deref(), Some("local"));
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
    fn creating_a_session_does_not_require_naming_a_profile_or_target() {
        // Both identifiers fall back to the default `mj go` records, so a
        // caller that has never read its configuration can still start work.
        let cli = Cli::try_parse_from(["mj", "new", "--bundle", "bundle-1", "add a README line"])
            .unwrap();
        let Some(Command::New(args)) = cli.command else {
            panic!("expected the new subcommand");
        };
        assert!(args.profile.is_none());
        assert!(args.target.is_none());
        assert_eq!(args.bundle.as_deref(), Some("bundle-1"));
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

        // Launch finding R3-11: without a branch name the API answered "a
        // branch export needs a branch name", which named no flag. The
        // command line refuses it first and names `--branch`.
        let Err(error) =
            Cli::try_parse_from(["mj", "export", "--session", "s1", "--kind", "branch"])
        else {
            panic!("a branch export without --branch is refused");
        };
        assert!(error.to_string().contains("--branch"), "{error}");

        // An export defaults to the patch, which is what a caller reviewing
        // the work asks for most.
        let cli = Cli::try_parse_from(["mj", "export", "--session", "s1"]).unwrap();
        let Some(Command::Export(args)) = cli.command else {
            panic!("expected the export subcommand");
        };
        assert_eq!(args.kind, ExportKindArg::Patch);

        let cli =
            Cli::try_parse_from(["mj", "destroy", "--session", "s1", "--delete-branch"]).unwrap();
        let Some(Command::Destroy(args)) = cli.command else {
            panic!("expected the suspend subcommand");
        };
        assert!(args.delete_branch);

        let cli = Cli::try_parse_from(["mj", "suspend", "--session", "s1"]).unwrap();
        let Some(Command::Suspend(args)) = cli.command else {
            panic!("expected the suspend subcommand");
        };
        assert_eq!(args.session.as_deref(), Some("s1"));
        assert!(Cli::try_parse_from(["mj", "suspend", "--session", "s1", "--force"]).is_err());
        // `close` is gone; its old name only says what replaced it.
        let cli = Cli::try_parse_from(["mj", "close", "--session", "s1"]).unwrap();
        assert!(crate::replacement_notice(cli.command.as_ref()).is_some());

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
    fn suspend_names_the_option_that_takes_a_session_id() {
        let parsed = Cli::try_parse_from(["mj", "suspend", "s1"]).expect("the id is accepted");
        let Command::Suspend(args) = parsed.command.expect("suspend is a command") else {
            panic!("suspend parsed as another command");
        };
        let error = suspend_session_id(&args).unwrap_err();
        assert!(
            format!("{error:#}").contains("--session s1"),
            "the error has to say which option to use: {error:#}"
        );

        let parsed =
            Cli::try_parse_from(["mj", "suspend", "--session", "s1"]).expect("the option parses");
        let Command::Suspend(args) = parsed.command.expect("suspend is a command") else {
            panic!("suspend parsed as another command");
        };
        assert_eq!(suspend_session_id(&args).unwrap(), "s1");
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
