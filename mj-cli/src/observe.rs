//! CLI inspection and export of Mjolnir's Brokk Observe envelopes.
//!
//! The daemon publishes automatically when Observe is configured. This command
//! also supports a local export for inspecting the same envelopes.

use std::collections::BTreeSet;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::{Args, Subcommand};
use serde::Serialize;

use mj_controller::observe::{
    ObserveDeliveryState, ObservePaths, ObserveSession, ObserveSpool, envelopes_for_transcript,
};
use mj_controller::server::api::{ApiSession, TranscriptItemView, TranscriptResponse};

use crate::api_client::ApiClient;

#[derive(Debug, Args)]
pub(crate) struct ObserveArgs {
    #[command(subcommand)]
    command: ObserveCommand,
}

#[derive(Debug, Subcommand)]
enum ObserveCommand {
    /// Show configuration and locally spooled delivery state.
    Status(ObserveStatusArgs),
    /// Convert one Mjolnir session to newline-delimited Observe envelopes.
    Export(ObserveExportArgs),
}

#[derive(Debug, Args)]
struct ObserveStatusArgs {
    /// Print the status as JSON.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct ObserveExportArgs {
    /// Session id, as `mj sessions` lists it.
    #[arg(long)]
    session: String,
    /// Write envelopes here instead of standard output. This command never
    /// uploads them.
    #[arg(short, long, value_name = "PATH")]
    output: Option<PathBuf>,
}

#[derive(Debug, Serialize)]
struct ObserveStatus {
    url_configured: bool,
    token_configured: bool,
    enforcement_requested: bool,
    enforcement_error: Option<String>,
    delivery_enabled: bool,
    delivery_error: Option<String>,
    config_file: Option<PathBuf>,
    spool: PathBuf,
    pending: usize,
    last_success_ms: Option<i64>,
    last_failure_ms: Option<i64>,
    last_failure: Option<String>,
    mode: &'static str,
}

pub(crate) async fn run(args: ObserveArgs) -> Result<()> {
    match args.command {
        ObserveCommand::Status(args) => status(args),
        ObserveCommand::Export(args) => export(args).await,
    }
}

fn status(args: ObserveStatusArgs) -> Result<()> {
    let paths = ObservePaths::current();
    let spool = ObserveSpool::open(&paths.spool)?;
    let enforcement = mj_core::observe::ObserveEnforcementConfig::from_environment();
    let enforcement_requested =
        mj_core::observe::ObserveEnforcementConfig::requested().unwrap_or(false);
    let delivery = mj_core::observe::ObserveDeliveryConfig::load();
    let configured_parts = mj_core::observe::ObserveDeliveryConfig::configured_parts();
    let delivery_enabled = delivery.as_ref().is_ok_and(Option::is_some);
    let delivery_error = delivery.err().map(|error| format!("{error:#}"));
    let configured_parts = configured_parts.unwrap_or_default();
    let delivery_state = ObserveDeliveryState::load(&paths.state, mj_core::clock::epoch_millis())?;
    let status = ObserveStatus {
        url_configured: configured_parts.0,
        token_configured: configured_parts.1,
        enforcement_requested,
        enforcement_error: enforcement.err().map(|error| format!("{error:#}")),
        delivery_enabled,
        delivery_error,
        config_file: mj_core::observe::config_path(),
        spool: paths.spool,
        pending: spool.pending()?.len(),
        last_success_ms: delivery_state.last_success_ms,
        last_failure_ms: delivery_state.last_failure_ms,
        last_failure: delivery_state.last_failure,
        mode: if delivery_enabled {
            "automatic session delivery is enabled"
        } else {
            "export-only; set OBSERVE_URL and OBSERVE_TOKEN for automatic delivery"
        },
    };
    if args.json {
        println!("{}", serde_json::to_string_pretty(&status)?);
    } else {
        println!(
            "Observe server: {}",
            if status.url_configured {
                "configured"
            } else {
                "not configured (set OBSERVE_URL)"
            }
        );
        println!(
            "Ingest token:   {}",
            if status.token_configured {
                "configured"
            } else {
                "not configured (set OBSERVE_TOKEN)"
            }
        );
        if let Some(path) = &status.config_file {
            println!("Config file:   {}", path.display());
        }
        println!("Spool:         {}", status.spool.display());
        println!("Pending:       {}", status.pending);
        println!(
            "Delivery:      {}",
            match (&status.delivery_error, status.delivery_enabled) {
                (Some(error), _) => format!("misconfigured: {error}"),
                (None, true) => "automatic".into(),
                (None, false) => "disabled".into(),
            }
        );
        if let Some(at) = status.last_success_ms {
            println!("Last success:  {at}");
        }
        if let Some(failure) = &status.last_failure {
            println!(
                "Last failure:  {} at {}",
                failure,
                status.last_failure_ms.unwrap_or_default()
            );
        }
        println!(
            "Enforcement:   {}",
            match (&status.enforcement_error, status.enforcement_requested) {
                (Some(error), _) => format!("misconfigured: {error}"),
                (None, true) => "enabled among DSH sessions".into(),
                (None, false) => "off (set OBSERVE_ENFORCE=true)".into(),
            }
        );
        println!("Mode:          {}", status.mode);
    }
    Ok(())
}

async fn export(args: ObserveExportArgs) -> Result<()> {
    let client = ApiClient::connect().await?;
    let api_session = client
        .session_if_known(&args.session)
        .await?
        .with_context(|| format!("session {:?} was not found", args.session))?;
    let transcript = fetch_transcript(&client, &args.session).await?;
    let observed = observe_session(&api_session);
    let envelopes = envelopes_for_transcript(&observed, &transcript)?;
    let mut body = String::new();
    for envelope in &envelopes {
        body.push_str(&envelope.to_ndjson()?);
    }
    match args.output {
        Some(path) if path != Path::new("-") => {
            std::fs::write(&path, body)
                .with_context(|| format!("write Observe export {}", path.display()))?;
            eprintln!(
                "exported {} event(s) for {} to {}",
                envelopes.len(),
                args.session,
                path.display()
            );
        }
        _ => {
            std::io::stdout()
                .write_all(body.as_bytes())
                .context("write Observe export to stdout")?;
        }
    }
    Ok(())
}

async fn fetch_transcript(client: &ApiClient, session_id: &str) -> Result<TranscriptResponse> {
    const PAGE: usize = 500;
    let mut items: Vec<TranscriptItemView> = Vec::new();
    let mut seen = BTreeSet::new();
    let mut after = 0_u64;
    let mut latest = 0_u64;
    let execution = loop {
        let page = client
            .transcript(session_id, Some(after), Some(PAGE), &[], false)
            .await?;
        let page_execution = page.execution;
        latest = latest.max(page.latest_seq);
        let next = page.next_after_seq;
        for item in page.items {
            if seen.insert((item.position, item.stable_id.clone())) {
                items.push(item);
            }
        }
        let has_next = page_has_next(after, next, latest);
        after = after.max(next);
        if !has_next {
            break page_execution;
        }
    };
    Ok(TranscriptResponse {
        next_after_seq: after,
        session_id: session_id.to_owned(),
        latest_seq: latest,
        execution,
        items,
    })
}

fn page_has_next(after: u64, next: u64, latest: u64) -> bool {
    next > after && next < latest
}

fn observe_session(session: &ApiSession) -> ObserveSession {
    let model = session
        .config_options
        .iter()
        .find(|option| option.key == "model")
        .and_then(|option| option.current.clone());
    ObserveSession {
        id: session.id.clone(),
        parent_id: None,
        native_session_id: None,
        worker_id: None,
        harness: session.harness_kind.clone(),
        model,
        cwd: None,
        hostname: None,
        os: None,
        started_at_ms: parse_timestamp(&session.created_at).unwrap_or_default(),
        workspace_id: Some(session.workspace_id.clone()),
        profile_id: Some(session.profile_id.clone()),
        target_id: Some(session.target_id.clone()),
        runtime: None,
        git: None,
        commits: Vec::new(),
        historical: false,
    }
}

fn parse_timestamp(value: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(value)
        .ok()
        .map(|time| time.timestamp_millis())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn export_requires_a_nonempty_session_argument() {
        use clap::Parser;

        #[derive(Debug, Parser)]
        struct TestCli {
            #[command(subcommand)]
            command: ObserveCommand,
        }

        let parsed = TestCli::try_parse_from(["observe", "export", "--session", "session-1"])
            .expect("valid observe export");
        assert!(matches!(
            parsed.command,
            ObserveCommand::Export(ObserveExportArgs { session, output: None })
                if session == "session-1"
        ));
    }

    #[test]
    fn status_never_contains_the_token_value() {
        let status = ObserveStatus {
            url_configured: true,
            token_configured: true,
            enforcement_requested: false,
            enforcement_error: None,
            delivery_enabled: true,
            delivery_error: None,
            config_file: Some(PathBuf::from("/tmp/config")),
            spool: PathBuf::from("/tmp/spool"),
            pending: 2,
            last_success_ms: Some(1),
            last_failure_ms: None,
            last_failure: None,
            mode: "automatic",
        };
        let encoded = serde_json::to_string(&status).unwrap();
        assert!(encoded.contains("\"token_configured\":true"));
        assert!(!encoded.contains("OBSERVE_TOKEN="));
    }

    #[test]
    fn transcript_paging_continues_through_a_tie_only_page() {
        assert!(page_has_next(500, 750, 1_000));
        assert!(!page_has_next(500, 500, 1_000));
        assert!(!page_has_next(750, 1_000, 1_000));
        assert!(!page_has_next(750, 500, 1_000));
    }
}
