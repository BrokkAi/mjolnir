//! Durable provenance for a hidden, model-produced handoff.

use anyhow::{Context, Result, ensure};
use mj_core::archive::{CanonicalSessionSnapshot, CanonicalTranscriptBody};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const HANDOFF_STATE_KEY: &str = "mj_handoff_state";
pub const MODEL_HANDOFF_OPENING: &str = "You are continuing a coding session previously run by another ACP harness. The restored workspace is authoritative. Use the historical state below for continuity, and do not repeat completed work unless verification requires it.\n\n";
pub const RETAINED_OPEN: &str = "\n\n<retained_recent_context>\n";
pub const RETAINED_CLOSE: &str = "</retained_recent_context>";
const MARKER_OPEN: &str = "\n\n<mj_handoff_state>";
const MARKER_CLOSE: &str = "</mj_handoff_state>";
const VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HandoffState {
    pub version: u32,
    pub through_position: u64,
    pub prefix_sha256: String,
    pub summary: String,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Marker {
    version: u32,
    through_position: u64,
    prefix_sha256: String,
}

impl HandoffState {
    pub fn capture(snapshot: &CanonicalSessionSnapshot, summary: String) -> Result<Self> {
        let through_position = snapshot.event_frontier.max(
            snapshot
                .transcript
                .iter()
                .map(|item| item.position)
                .max()
                .unwrap_or(0),
        );
        Ok(Self {
            version: VERSION,
            through_position,
            prefix_sha256: prefix_digest(snapshot, through_position)?,
            summary,
        })
    }

    pub fn validate(&self, snapshot: &CanonicalSessionSnapshot) -> Result<bool> {
        if self.version != VERSION || snapshot.current_context_start() > self.through_position {
            return Ok(false);
        }
        Ok(self.prefix_sha256 == prefix_digest(snapshot, self.through_position)?)
    }

    pub fn attach_to_text(&self, text: &str) -> Result<String> {
        let summary = model_body(text).context("not a complete model handoff")?;
        ensure!(
            summary == self.summary,
            "handoff body differs from captured state"
        );
        let marker = Marker {
            version: self.version,
            through_position: self.through_position,
            prefix_sha256: self.prefix_sha256.clone(),
        };
        Ok(format!(
            "{text}{MARKER_OPEN}{}{MARKER_CLOSE}",
            serde_json::to_string(&marker)?
        ))
    }

    pub fn from_handoff_text(text: &str) -> Option<Self> {
        let (body_text, marker) = text.rsplit_once(MARKER_OPEN)?;
        let marker: Marker = serde_json::from_str(marker.strip_suffix(MARKER_CLOSE)?).ok()?;
        if marker.version != VERSION
            || marker.prefix_sha256.len() != 64
            || !marker
                .prefix_sha256
                .bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
        {
            return None;
        }
        let summary = model_body(body_text)?;
        Some(Self {
            version: marker.version,
            through_position: marker.through_position,
            prefix_sha256: marker.prefix_sha256,
            summary: summary.to_owned(),
        })
    }
}

pub fn encode_handoff(snapshot: &CanonicalSessionSnapshot, text: &str) -> Result<String> {
    let summary = model_body(text).context("not a complete model handoff")?;
    HandoffState::capture(snapshot, summary.to_owned())?.attach_to_text(text)
}

pub fn parse_handoff(text: &str) -> Option<HandoffState> {
    HandoffState::from_handoff_text(text)
}

pub fn reusable_handoff(snapshot: &CanonicalSessionSnapshot) -> Option<HandoffState> {
    let value = snapshot
        .session
        .configuration
        .values
        .get(HANDOFF_STATE_KEY)?;
    let state: HandoffState = match serde_json::from_value(value.clone()) {
        Ok(state) => state,
        Err(error) => {
            tracing::warn!(%error, "stored handoff state is malformed");
            return None;
        }
    };
    match state.validate(snapshot) {
        Ok(true) => Some(state),
        Ok(false) => None,
        Err(error) => {
            tracing::warn!(%error, "could not validate stored handoff state");
            None
        }
    }
}

fn model_body(text: &str) -> Option<&str> {
    let body = text.strip_prefix(MODEL_HANDOFF_OPENING)?;
    let (summary, retained) = body.rsplit_once(RETAINED_OPEN)?;
    if summary.trim().is_empty() || !retained.ends_with(RETAINED_CLOSE) {
        return None;
    }
    Some(body)
}

fn prefix_digest(snapshot: &CanonicalSessionSnapshot, through_position: u64) -> Result<String> {
    let start = snapshot.current_context_start();
    let mut hash = Sha256::new();
    hash.update(b"mj-handoff-prefix-v1\0");
    hash.update(start.to_le_bytes());
    for item in snapshot
        .transcript
        .iter()
        .filter(|item| item.position > start && item.position <= through_position)
    {
        hash.update(item.position.to_le_bytes());
        hash.update((item.stable_id.len() as u64).to_le_bytes());
        hash.update(item.stable_id.as_bytes());
        let mut body = item.body.clone();
        match &mut body {
            CanonicalTranscriptBody::Agent { streaming, .. }
            | CanonicalTranscriptBody::Thought { streaming, .. } => *streaming = false,
            _ => {}
        }
        let encoded = serde_json::to_vec(&body)?;
        hash.update((encoded.len() as u64).to_le_bytes());
        hash.update(encoded);
    }
    Ok(mj_core::hex::lower_hex(hash.finalize()))
}
