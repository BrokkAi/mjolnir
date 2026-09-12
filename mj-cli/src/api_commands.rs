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
use mj_controller::hel_server::api::{
    ExportKind, ExportRequest, RelayState, StartSessionRequest, WaitOutcome, WaitRequest,
    WaitResponse,
};

use crate::api_client::{ApiClient, ExportResult};

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
    /// Return the same session when this creation call is retried.
    #[arg(long)]
    idempotency_key: Option<String>,
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
    role: Option<hel::hel_transcript::TranscriptRole>,
    #[arg(long)]
    json: bool,
}

fn parse_transcript_role(value: &str) -> Result<hel::hel_transcript::TranscriptRole, String> {
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
        hel::hel_archive::read_session_file_input(std::io::stdin().lock())?
    } else {
        hel::hel_archive::read_session_file_input(
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
    /// File to read out of the session workspace, required by `--kind file`.
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
pub(crate) struct ApiInfoArgs {
    #[arg(long)]
    json: bool,
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
        workspace_id,
        profile_id: args.profile.clone(),
        target_id: args.target.clone(),
        bundle_id: args.bundle.clone(),
        project_directory: args.project_directory.clone(),
        title: args.title.clone(),
        model: args.model.clone(),
        effort: args.effort.clone(),
        prompt,
        idempotency_key: args.idempotency_key.clone(),
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
        let mut line = outcome_name(response.outcome).to_owned();
        if let Some(stop_reason) = &response.stop_reason {
            line.push_str(&format!(" ({stop_reason})"));
        }
        if let Some(turn_number) = response.turn_number {
            line.push_str(&format!(" turn {turn_number}"));
        }
        if let Some(elapsed_ms) = response.elapsed_ms {
            line.push_str(&format!(" in {:.1}s", elapsed_ms.max(0) as f64 / 1000.0));
        }
        println!("{line}");
        if !response.pending_elicitations.is_empty() {
            print_json(&response.pending_elicitations)?;
        }
        if let Some(message) = &response.message {
            println!("{message}");
        }
        if let Some(retry) = &response.capacity_retry {
            println!(
                "a capacity retry is armed (attempt {}); do not send another prompt yet",
                retry.attempt
            );
        }
        // A turn that is still running and a worker the daemon cannot see look
        // identical from a timeout alone, so name the relay when it is at
        // fault.
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
            println!("{line}");
        }
        if let Some(final_message) = &response.final_message {
            println!();
            println!("{final_message}");
        }
    }
    match response.outcome {
        WaitOutcome::Finished | WaitOutcome::InputRequired => Ok(()),
        outcome => bail!("the turn ended as {}", outcome_name(outcome)),
    }
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
                .context("`--kind file` needs --path, relative to the session workspace")?;
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
        let session = client.session(session_id).await?;
        if args.json {
            return print_json(&session);
        }
        println!("{}  {}  {}", session.id, session.state, session.title);
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

pub(crate) async fn close(args: SessionArgs) -> Result<()> {
    let client = ApiClient::connect().await?;
    client.close(&args.session).await?;
    match args.json {
        true => print_json(&serde_json::json!({ "session_id": args.session, "accepted": true })),
        false => {
            println!("closing {}", args.session);
            Ok(())
        }
    }
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
            &mj_controller::hel_server::api::SetConfigRequest {
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
            "--idempotency-key",
            "key-1",
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
        assert_eq!(args.idempotency_key.as_deref(), Some("key-1"));
        assert_eq!(args.prompt.as_deref(), Some("add a README line"));
        assert!(!args.json);

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

        for (argv, matched) in [
            (vec!["mj", "diff", "--session", "s1"], "diff"),
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
