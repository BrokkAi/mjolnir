//! Harness-hook client for draining the worker-owned session mailbox.

use anyhow::{Context, Result, bail};
use serde_json::json;
use std::io::Write;
use std::path::Path;

/// Consume one harness hook request, drain the mailbox, and print the hook's
/// JSON response. Relay unavailability is deliberately fail-open: the relay
/// keeps events pending for a later prompt or wake.
pub fn run(socket: &Path, hook_event: &str) -> Result<()> {
    if let Err(error) = std::io::copy(&mut std::io::stdin().lock(), &mut std::io::sink()) {
        return write_empty_with_diagnostic(&format!("could not consume hook input: {error}"));
    }
    match drain(socket, hook_event) {
        Ok((Some(text), count)) if count > 0 => write_stdout(&json!({
            "hookSpecificOutput": {
                "hookEventName": hook_event,
                "additionalContext": text,
            }
        })),
        Ok(_) => write_stdout(&json!({})),
        Err(error) => write_empty_with_diagnostic(&format!("could not drain mailbox: {error:#}")),
    }
}

fn drain(socket: &Path, hook_event: &str) -> Result<(Option<String>, usize)> {
    anyhow::ensure!(
        matches!(hook_event, "PostToolUse" | "PostToolBatch"),
        "unsupported mailbox hook event"
    );
    let request_id = mj_core::state::new_session_id()?;
    let mut stream = mj_core::local_sockets::connect_unix_stream(socket)
        .with_context(|| format!("connect to worker control socket {}", socket.display()))?;
    stream.set_read_timeout(Some(std::time::Duration::from_secs(5)))?;
    stream.set_write_timeout(Some(std::time::Duration::from_secs(5)))?;
    let envelope = mj_core::relay::RelayRequestEnvelope {
        request_id: request_id.clone(),
        protocol_version: mj_core::relay::RELAY_PROTOCOL_VERSION,
        request: mj_core::relay::RelayRequest::DrainMailbox {
            hook_event: hook_event.to_owned(),
        },
    };
    serde_json::to_writer(&mut stream, &envelope)?;
    stream.write_all(b"\n")?;

    let mut reader = std::io::BufReader::new(stream);
    let mut line = Vec::new();
    let (read, complete) =
        mj_core::relay::read_bounded_line(&mut reader, &mut line, mj_core::relay::MAX_FRAME_BYTES)?;
    anyhow::ensure!(read > 0, "worker closed the mailbox hook connection");
    anyhow::ensure!(complete, "worker returned an incomplete mailbox hook frame");
    let response: mj_core::relay::RelayResponseEnvelope = serde_json::from_slice(&line)?;
    anyhow::ensure!(
        response.request_id == request_id,
        "mailbox hook reply identity mismatch"
    );
    match response.body {
        mj_core::relay::RelayResponseBody::Ok {
            payload: mj_core::relay::RelayResponsePayload::MailboxDrained { text, count },
        } => Ok((text, count)),
        other => bail!("worker refused mailbox hook request: {other:?}"),
    }
}

fn write_stdout(value: &serde_json::Value) -> Result<()> {
    let mut output = serde_json::to_vec(value)?;
    output.push(b'\n');
    std::io::stdout().lock().write_all(&output)?;
    Ok(())
}

fn write_empty_with_diagnostic(message: &str) -> Result<()> {
    eprintln!("Mjolnir mailbox hook: {message}");
    write_stdout(&json!({}))
}
