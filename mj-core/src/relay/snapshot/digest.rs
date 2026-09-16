//! The relay event digest chain and the validation that checks it.

use anyhow::{Context, Result, anyhow, bail};
use serde::Serialize;
use sha2::{Digest, Sha256};

use super::{
    RELAY_EVENT_DIGEST_DOMAIN, RELAY_EVENT_DIGEST_DOMAIN_V2, RELAY_EVENT_FORMAT_V1,
    RELAY_EVENT_FORMAT_V2, RelayEvent, RelayObservation,
};

/// v1 digest payload: folds `previous_digest` into the hash (the chain link).
/// Its exact field order and serde attributes are load-bearing — changing them
/// would invalidate every stored v1 digest.
#[derive(Serialize)]
struct RelayEventDigestPayload<'a, O: Serialize> {
    ordinal: u64,
    previous_digest: &'a str,
    recorded_at_ms: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    command_id: Option<&'a str>,
    observation: &'a O,
}

/// v2 digest payload: identical to v1 but with no `previous_digest`, so the
/// digest depends only on the record's own content.
#[derive(Serialize)]
struct RelayEventDigestPayloadV2<'a, O: Serialize> {
    ordinal: u64,
    recorded_at_ms: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    command_id: Option<&'a str>,
    observation: &'a O,
}

/// The `session_opened` encoding written by builds 0f070506 through e6ed54ed
/// on 2026-09-15, which serialized `native_continuity_lost` even when false.
/// Records from that window carry digests over this shape, so validation
/// accepts it for exactly that observation and nothing else.
#[derive(Serialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
enum LegacyFlaggedObservation<'a> {
    SessionOpened {
        native_session_id: &'a str,
        resumed: bool,
        native_continuity_lost: bool,
    },
}

fn digest_over(domain: &[u8], encoded: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(domain);
    hasher.update(encoded);
    format!("{:x}", hasher.finalize())
}

/// Compute the domain-separated SHA-256 digest for a relay event, using the
/// formula that matches the record's format. The `digest` field itself is
/// excluded; for v2 so is `previous_digest`.
pub fn relay_event_digest(event: &RelayEvent) -> Result<String> {
    relay_event_digest_over(event, &event.observation)
}

fn relay_event_digest_over<O: Serialize>(event: &RelayEvent, observation: &O) -> Result<String> {
    match event.format {
        RELAY_EVENT_FORMAT_V1 => {
            validate_relay_digest(&event.previous_digest, "previous event digest")?;
            let payload = RelayEventDigestPayload {
                ordinal: event.ordinal,
                previous_digest: &event.previous_digest,
                recorded_at_ms: event.recorded_at_ms,
                command_id: event.command_id.as_deref(),
                observation,
            };
            let encoded =
                serde_json::to_vec(&payload).context("serialize relay event digest payload")?;
            Ok(digest_over(RELAY_EVENT_DIGEST_DOMAIN, &encoded))
        }
        RELAY_EVENT_FORMAT_V2 => {
            if !event.previous_digest.is_empty() {
                bail!(
                    "v2 relay event {} must not carry a previous_digest",
                    event.ordinal
                );
            }
            let payload = RelayEventDigestPayloadV2 {
                ordinal: event.ordinal,
                recorded_at_ms: event.recorded_at_ms,
                command_id: event.command_id.as_deref(),
                observation,
            };
            let encoded =
                serde_json::to_vec(&payload).context("serialize relay event digest payload")?;
            Ok(digest_over(RELAY_EVENT_DIGEST_DOMAIN_V2, &encoded))
        }
        other => bail!(
            "unknown relay event format {other} at event {}",
            event.ordinal
        ),
    }
}

/// Verify an event against the exact previously applied event cursor. This is
/// the shared validation contract for both the relay journal and controller
/// projections.
///
/// Every event is validated by its **own** recomputed digest (self-contained)
/// plus ordinal contiguity. For v1 records the in-record `previous_digest` link
/// to the cursor is also enforced; v2 records carry no link — their continuity
/// to the cursor is proven by the digest anchor at the cursor ordinal
/// (`validate_cursor`) and by the page/frontier endpoint, not by an in-record
/// back-reference. This keeps a corrupt record from invalidating its successors.
pub fn validate_relay_event(
    previous_ordinal: u64,
    previous_digest: &str,
    event: &RelayEvent,
) -> Result<()> {
    validate_relay_digest(previous_digest, "previous cursor digest")?;
    let expected_ordinal = previous_ordinal
        .checked_add(1)
        .ok_or_else(|| anyhow!("relay event ordinal exhausted"))?;
    if event.ordinal != expected_ordinal {
        bail!(
            "relay event gap: expected {expected_ordinal}, found {}",
            event.ordinal
        );
    }
    if event.format == RELAY_EVENT_FORMAT_V1 && event.previous_digest != previous_digest {
        bail!(
            "relay event {} previous digest does not match cursor",
            event.ordinal
        );
    }
    validate_relay_event_self(event)
}

/// Verify a record purely against itself: its `digest` field is well-formed and
/// recomputes to the same value. This is the corruption check for a single
/// record, independent of any neighbour — the unit of trust that lets a corrupt
/// record be isolated instead of poisoning the events around it. It does not
/// check ordinal continuity or (for v1) the chain link; those are the caller's
/// job where a trusted cursor is available.
pub fn validate_relay_event_self(event: &RelayEvent) -> Result<()> {
    validate_relay_digest(&event.digest, "event digest")?;
    let expected_digest = relay_event_digest(event)?;
    if event.digest == expected_digest {
        return Ok(());
    }
    if let RelayObservation::SessionOpened {
        native_session_id,
        resumed,
        native_continuity_lost: false,
    } = &event.observation
    {
        let legacy = LegacyFlaggedObservation::SessionOpened {
            native_session_id,
            resumed: *resumed,
            native_continuity_lost: false,
        };
        if event.digest == relay_event_digest_over(event, &legacy)? {
            return Ok(());
        }
    }
    bail!("relay event {} digest is invalid", event.ordinal);
}

pub fn validate_relay_digest(digest: &str, name: &str) -> Result<()> {
    if digest.len() != 64
        || !digest
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        bail!("{name} must be 64 lowercase hexadecimal characters");
    }
    Ok(())
}
