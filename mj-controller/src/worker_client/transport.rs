use super::*;

/// Keep transport, protocol, and explicit relay rejections visible at the
/// point where a request fails. Callers often turn these into a user-facing
/// string or a retry, which otherwise loses the operation and request ID that
/// make concurrent session failures diagnosable.
pub(super) fn log_relay_client_failure(
    client: &RelayClient,
    operation: &str,
    request_id: &str,
    error: &anyhow::Error,
) {
    let rejection = error.chain().find_map(|cause| {
        cause
            .downcast_ref::<RelayRejected>()
            .map(|rejected| &rejected.0)
    });
    let transport_dead = RelayTransportDead::marks(error);
    match rejection {
        Some(rejection) => tracing::warn!(
            session_id = %client.session_id,
            relay_version = %client.relay_version,
            %operation,
            %request_id,
            relay_error_code = ?rejection.code,
            relay_retryable = rejection.retryable,
            transport_dead,
            error = %error,
            "relay request rejected"
        ),
        None => tracing::warn!(
            session_id = %client.session_id,
            relay_version = %client.relay_version,
            %operation,
            %request_id,
            transport_dead,
            error = %error,
            "relay request failed"
        ),
    }
}

impl Drop for RelayClient {
    fn drop(&mut self) {
        // Async owners call `detach` so EOF has a bounded chance to propagate
        // through Podman or SSH before the launcher is stopped. Drop is the
        // shutdown-safe fallback: it may run while Tokio's drivers are already
        // gone, so its bounded reaper cannot use runtime work or Tokio timers.
        drop(self.input.take());
        let Some(child) = self.child.take() else {
            return;
        };
        let session_id = self.session_id.clone();
        // The proxy's SSH session stays leased until the reaper is done with
        // the child.
        let ssh_session = self.ssh_session.take();
        if let Err(error) = std::thread::Builder::new()
            .name("hel-relay-reaper".into())
            .spawn(move || {
                reap_dropped_relay_proxy(child, session_id);
                drop(ssh_session);
            })
        {
            tracing::warn!(
                session_id = %self.session_id,
                %error,
                "could not start dropped relay proxy reaper"
            );
        }
    }
}

/// Let EOF traverse a proxy launcher, then stop and reap it without relying on
/// an async runtime that may already be shutting down.
pub(super) fn reap_dropped_relay_proxy(mut child: Child, session_id: String) {
    let deadline = Instant::now() + RELAY_PROXY_DETACH_GRACE;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                if !status.success() {
                    tracing::warn!(
                        %session_id,
                        %status,
                        "dropped relay proxy exited unsuccessfully"
                    );
                }
                return;
            }
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(RELAY_PROXY_REAP_POLL);
            }
            Ok(None) => break,
            Err(error) => {
                tracing::warn!(%session_id, %error, "could not reap dropped relay proxy");
                return;
            }
        }
    }

    if let Err(error) = child.start_kill()
        && error.kind() != std::io::ErrorKind::NotFound
    {
        tracing::warn!(%session_id, %error, "could not stop dropped relay proxy");
        return;
    }
    let deadline = Instant::now() + RELAY_PROXY_DETACH_GRACE;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return,
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(RELAY_PROXY_REAP_POLL);
            }
            Ok(None) => {
                tracing::warn!(%session_id, "stopped relay proxy could not be reaped in time");
                return;
            }
            Err(error) => {
                tracing::warn!(%session_id, %error, "could not reap stopped relay proxy");
                return;
            }
        }
    }
}

pub(super) fn credential_snapshot(payload: RelayResponsePayload) -> Result<CredentialSnapshot> {
    match payload {
        RelayResponsePayload::CredentialState {
            present,
            fingerprint,
            freshness_epoch_ms,
        } => Ok(CredentialSnapshot {
            present,
            fingerprint,
            freshness_epoch_ms,
        }),
        _ => bail!("relay returned an unexpected credential state response"),
    }
}

pub(super) fn skills_sync_state(
    payload: RelayResponsePayload,
) -> Result<mj_core::skills::SkillsSyncState> {
    match payload {
        RelayResponsePayload::SkillsState {
            present,
            fingerprint,
        } => Ok(mj_core::skills::SkillsSyncState {
            present,
            fingerprint,
        }),
        _ => bail!("relay returned an unexpected skills state response"),
    }
}

pub(super) fn github_token_snapshot(
    payload: RelayResponsePayload,
) -> Result<mj_core::credentials::GithubTokenSnapshot> {
    match payload {
        RelayResponsePayload::GithubTokenState {
            present,
            fingerprint,
        } => Ok(mj_core::credentials::GithubTokenSnapshot {
            present,
            fingerprint,
        }),
        _ => bail!("relay returned an unexpected GitHub token state response"),
    }
}

pub(super) async fn read_bounded_frame(
    reader: &mut (impl AsyncBufRead + Unpin),
    kind: ExchangeKind,
) -> Result<Option<String>> {
    read_bounded_frame_with_limit(reader, MAX_FRAME_BYTES, kind).await
}

pub(super) async fn read_bounded_frame_with_limit(
    reader: &mut (impl AsyncBufRead + Unpin),
    maximum_bytes: usize,
    kind: ExchangeKind,
) -> Result<Option<String>> {
    use mj_core::bounded_frame::{BoundedFrame, BoundedFrameError};
    // A failed read and a half-written frame are transport deaths; the limit
    // and encoding failures are protocol violations that a worker restart
    // would not fix, so only the first two carry the marker.
    let mut frame = match mj_core::bounded_frame::read_bounded_frame(reader, maximum_bytes).await {
        Ok(BoundedFrame::Line(frame)) => frame,
        Ok(BoundedFrame::End) => return Ok(None),
        Ok(BoundedFrame::Truncated(_)) => {
            return Err(anyhow::Error::new(RelayTransportDead::during_exchange(
                "relay proxy disconnected in the middle of a response frame",
                kind,
            )));
        }
        Err(BoundedFrameError::Io(error)) => {
            return Err(RelayTransportDead::from_io(error, kind).into());
        }
        Err(BoundedFrameError::TooLarge) => bail!("relay response frame is too large"),
    };
    if frame.last() == Some(&b'\r') {
        frame.pop();
    }
    String::from_utf8(frame)
        .context("relay response is not UTF-8")
        .map(Some)
}

pub(super) fn clip_catch_up_page(
    page: RelayAttachment,
    previous: &RelayCursor,
    frontier: &RelayCursor,
) -> Result<RelayEventPage> {
    if previous.ordinal > frontier.ordinal {
        bail!("relay catch-up starts beyond its fixed frontier");
    }
    if previous.ordinal == frontier.ordinal {
        if previous != frontier {
            bail!("relay catch-up cursor digest differs from its fixed frontier");
        }
        if !page.events.is_empty() || page.through_ordinal != previous.ordinal {
            bail!("relay attachment advanced beyond its advertised frontier");
        }
        return Ok(RelayEventPage {
            events: Vec::new(),
            through_ordinal: previous.ordinal,
            through_digest: previous.digest.clone(),
        });
    }
    if page.through_ordinal <= previous.ordinal || page.events.is_empty() {
        bail!("relay catch-up page did not advance");
    }
    if page.through_ordinal <= frontier.ordinal {
        let through = RelayCursor {
            ordinal: page.through_ordinal,
            digest: page.through_digest.clone(),
        };
        if through.ordinal == frontier.ordinal && through != *frontier {
            bail!("relay catch-up page digest differs from its fixed frontier");
        }
        return Ok(RelayEventPage {
            events: page.events,
            through_ordinal: through.ordinal,
            through_digest: through.digest,
        });
    }

    let events = page
        .events
        .into_iter()
        .take_while(|event| event.ordinal <= frontier.ordinal)
        .collect::<Vec<_>>();
    let reached = events
        .last()
        .map(|event| RelayCursor {
            ordinal: event.ordinal,
            digest: event.digest.clone(),
        })
        .ok_or_else(|| anyhow!("relay catch-up page skipped its fixed frontier"))?;
    if reached != *frontier {
        bail!("relay catch-up page does not contain its fixed frontier");
    }
    Ok(RelayEventPage {
        events,
        through_ordinal: reached.ordinal,
        through_digest: reached.digest,
    })
}

pub(super) fn decode_relay_response(
    line: &str,
    request_id: &str,
    protocol: u32,
) -> Result<RelayResponsePayload> {
    let response: RelayResponseEnvelope =
        serde_json::from_str(line).context("decode relay response")?;
    if response.request_id != request_id {
        bail!(
            "relay response ID mismatch: expected {request_id}, got {}",
            response.request_id
        );
    }
    if response.protocol_version != protocol {
        bail!(
            "relay response protocol mismatch: expected {protocol}, got {}",
            response.protocol_version
        );
    }
    match response.body {
        RelayResponseBody::Ok { payload } => Ok(payload),
        RelayResponseBody::Error { error } => Err(RelayRejected(error).into()),
    }
}

pub(super) fn decode_relay_hello_response(
    line: &str,
    request_id: &str,
) -> Result<RelayResponsePayload> {
    let response: RelayResponseEnvelope =
        serde_json::from_str(line).context("decode relay hello response")?;
    if response.request_id != request_id {
        bail!(
            "relay response ID mismatch: expected {request_id}, got {}",
            response.request_id
        );
    }
    match response.body {
        RelayResponseBody::Ok {
            payload: payload @ RelayResponsePayload::Hello { negotiated, .. },
        } => {
            if response.protocol_version != negotiated {
                bail!(
                    "relay hello envelope uses protocol {}, negotiated {negotiated}",
                    response.protocol_version
                );
            }
            Ok(payload)
        }
        RelayResponseBody::Ok { .. } => bail!("relay returned an unexpected hello response"),
        RelayResponseBody::Error { error } => Err(RelayRejected(error).into()),
    }
}
