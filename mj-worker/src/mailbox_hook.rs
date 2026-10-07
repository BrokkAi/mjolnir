//! Harness-hook client for draining the worker-owned session mailbox.

use anyhow::{Context, Result, bail};
use serde_json::json;
use std::io::Write;
use std::path::Path;

/// Consume one harness hook request, write its response, then acknowledge the
/// relay lease. Relay unavailability is deliberately fail-open: an unacked
/// lease returns to the mailbox after its timeout or a worker restart.
pub fn run(socket: &Path, hook_event: &str) -> Result<()> {
    if let Err(error) = std::io::copy(&mut std::io::stdin().lock(), &mut std::io::sink()) {
        return write_empty_with_diagnostic(&format!("could not consume hook input: {error}"));
    }
    match drain(socket, hook_event) {
        Ok((Some(text), count, Some(lease_id))) if count > 0 => {
            write_stdout(&json!({
                "hookSpecificOutput": {
                    "hookEventName": hook_event,
                    "additionalContext": text,
                }
            }))?;
            if let Err(error) = acknowledge(socket, &lease_id) {
                eprintln!(
                    "Mjolnir mailbox hook: could not acknowledge delivered events: {error:#}"
                );
            }
            Ok(())
        }
        Ok((None, 0, None)) => write_stdout(&json!({})),
        Ok(_) => write_empty_with_diagnostic("worker returned an incomplete mailbox lease"),
        Err(error) => write_empty_with_diagnostic(&format!("could not drain mailbox: {error:#}")),
    }
}

fn drain(socket: &Path, hook_event: &str) -> Result<(Option<String>, usize, Option<String>)> {
    anyhow::ensure!(
        matches!(hook_event, "PostToolUse" | "PostToolBatch"),
        "unsupported mailbox hook event"
    );
    match send_request(
        socket,
        mj_core::relay::RelayRequest::DrainMailbox {
            hook_event: hook_event.to_owned(),
        },
    )? {
        mj_core::relay::RelayResponsePayload::MailboxDrained {
            lease_id,
            text,
            count,
        } => Ok((text, count, lease_id)),
        other => bail!("worker refused mailbox hook request: {other:?}"),
    }
}

fn acknowledge(socket: &Path, lease_id: &str) -> Result<()> {
    match send_request(
        socket,
        mj_core::relay::RelayRequest::AckMailbox {
            lease_id: lease_id.to_owned(),
        },
    )? {
        mj_core::relay::RelayResponsePayload::MailboxAcknowledged { acknowledged: true } => Ok(()),
        mj_core::relay::RelayResponsePayload::MailboxAcknowledged {
            acknowledged: false,
        } => bail!("worker no longer holds the mailbox lease; it may be delivered again"),
        other => bail!("worker refused mailbox acknowledgement: {other:?}"),
    }
}

fn send_request(
    socket: &Path,
    request: mj_core::relay::RelayRequest,
) -> Result<mj_core::relay::RelayResponsePayload> {
    const REQUEST_TIMEOUT: std::time::Duration =
        std::time::Duration::from_secs(mj_core::mailbox::MAILBOX_HOOK_REQUEST_TIMEOUT_SECS);
    let request_id = mj_core::state::new_session_id()?;
    let mut stream = mj_core::local_sockets::connect_unix_stream(socket)
        .with_context(|| format!("connect to worker control socket {}", socket.display()))?;
    stream.set_read_timeout(Some(REQUEST_TIMEOUT))?;
    stream.set_write_timeout(Some(REQUEST_TIMEOUT))?;
    let envelope = mj_core::relay::RelayRequestEnvelope {
        request_id: request_id.clone(),
        protocol_version: mj_core::relay::RELAY_PROTOCOL_VERSION,
        request,
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
        mj_core::relay::RelayResponseBody::Ok { payload } => Ok(payload),
        other => bail!("worker refused mailbox hook request: {other:?}"),
    }
}

fn write_stdout(value: &serde_json::Value) -> Result<()> {
    let mut output = serde_json::to_vec(value)?;
    output.push(b'\n');
    let mut stdout = std::io::stdout().lock();
    stdout.write_all(&output)?;
    stdout.flush()?;
    Ok(())
}

fn write_empty_with_diagnostic(message: &str) -> Result<()> {
    eprintln!("Mjolnir mailbox hook: {message}");
    write_stdout(&json!({}))
}
