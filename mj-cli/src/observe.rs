//! CLI inspection and export of Mjolnir's Brokk Observe envelopes.
//!
//! This first checkpoint deliberately exports locally. It proves the
//! harness-neutral conversion without making a running daemon depend on
//! Observe's availability.

use std::collections::BTreeSet;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::{Args, Subcommand};
use serde::Serialize;

use mj_controller::observe::{ObserveSession, ObserveSpool, envelopes_for_transcript};
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
    spool: PathBuf,
    pending: usize,
    mode: &'static str,
}

pub(crate) async fn run(args: ObserveArgs) -> Result<()> {
    match args.command {
        ObserveCommand::Status(args) => status(args),
        ObserveCommand::Export(args) => export(args).await,
    }
}

fn status(args: ObserveStatusArgs) -> Result<()> {
    let spool_path = spool_path();
    let spool = ObserveSpool::open(&spool_path)?;
    let status = ObserveStatus {
        url_configured: std::env::var_os("OBSERVE_URL").is_some_and(|value| !value.is_empty()),
        token_configured: std::env::var_os("OBSERVE_TOKEN").is_some_and(|value| !value.is_empty()),
        spool: spool_path,
        pending: spool.pending()?.len(),
        mode: "export-only; daemon publication and upload are not enabled yet",
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
        println!("Spool:         {}", status.spool.display());
        println!("Pending:       {}", status.pending);
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
        harness: session.harness_kind.clone(),
        model,
        cwd: None,
        started_at_ms: parse_timestamp(&session.created_at).unwrap_or_default(),
        workspace_id: Some(session.workspace_id.clone()),
        profile_id: Some(session.profile_id.clone()),
        target_id: Some(session.target_id.clone()),
        runtime: None,
    }
}

fn parse_timestamp(value: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(value)
        .ok()
        .map(|time| time.timestamp_millis())
}

fn spool_path() -> PathBuf {
    mj_core::config::data_dir().join("observe").join("spool")
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
            spool: PathBuf::from("/tmp/spool"),
            pending: 2,
            mode: "export-only",
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
