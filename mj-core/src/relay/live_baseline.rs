//! Connection-owned live recovery cuts and frozen snapshot transport.
use super::*;
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LiveQueuedCommand {
    pub command_id: String,
    pub command: RelayCommand,
    pub created_at_ms: i64,
    pub accepted_ordinal: Option<u64>,
}

/// One coherent current-state cut; no transcript history is represented.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LiveSessionBaseline {
    pub state: RelayOperationalState,
    pub queue: Vec<LiveQueuedCommand>,
    pub questions: Vec<crate::elicitation::ElicitationRequest>,
    pub unavailable_questions: usize,
    pub active_turn: Option<crate::state::MaterializedTurn>,
    pub native_agent_replay: Option<Vec<crate::native_agent::NativeAgent>>,
    pub native_use_evidence: Option<NativeSessionUseEvidence>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FrozenSnapshotPage {
    pub offset: u64,
    pub next_offset: u64,
    pub complete: bool,
    pub chunk: String,
}

/// A frozen byte download belongs to its connection and dies on disconnect.
pub struct FrozenSnapshotDownload {
    bytes: Vec<u8>,
    next_offset: u64,
}

impl FrozenSnapshotDownload {
    pub fn new(bytes: Vec<u8>) -> Self {
        Self {
            bytes,
            next_offset: 0,
        }
    }
    pub fn read(&mut self, offset: u64, capacity: usize) -> Result<FrozenSnapshotPage> {
        use base64::Engine as _;
        ensure!(
            offset == self.next_offset,
            "snapshot page offset mismatch: expected {}, got {offset}",
            self.next_offset
        );
        let start =
            usize::try_from(offset).context("snapshot offset exceeds addressable memory")?;
        let end = start.saturating_add(capacity).min(self.bytes.len());
        ensure!(start <= end, "snapshot offset exceeds download length");
        let chunk = base64::engine::general_purpose::STANDARD.encode(&self.bytes[start..end]);
        self.next_offset = u64::try_from(end).context("snapshot offset exceeds transfer range")?;
        Ok(FrozenSnapshotPage {
            offset,
            next_offset: self.next_offset,
            complete: end == self.bytes.len(),
            chunk,
        })
    }
}

pub fn live_baseline_page_capacity(protocol_version: u32, request_id: &str) -> Result<usize> {
    relay_transfer_chunk_capacity(&RelayResponseEnvelope {
        request_id: request_id.into(),
        protocol_version,
        body: RelayResponseBody::Ok {
            payload: RelayResponsePayload::LiveBaselinePage {
                page: FrozenSnapshotPage {
                    offset: u64::MAX,
                    next_offset: u64::MAX,
                    complete: false,
                    chunk: String::new(),
                },
            },
        },
    })
}

/// Shared framing of a connection-owned recovery download. Capture is separate
/// so callers can release the relay lock before serialization and encoding.
pub fn live_baseline_reply(
    request: &RelayRequest,
    cut: Option<LiveSessionBaseline>,
    download: &mut Option<FrozenSnapshotDownload>,
    request_id: &str,
    protocol_version: u32,
) -> Result<RelayResponsePayload> {
    match request {
        RelayRequest::BeginLiveBaseline => {
            ensure!(
                download.is_none(),
                "connection already owns a live baseline download"
            );
            let bytes = serde_json::to_vec(&cut.context("live baseline capture is missing")?)?;
            let total_bytes =
                u64::try_from(bytes.len()).context("live baseline exceeds transfer range")?;
            *download = Some(FrozenSnapshotDownload::new(bytes));
            Ok(RelayResponsePayload::LiveBaselineStarted { total_bytes })
        }
        RelayRequest::ReadLiveBaselinePage { offset } => {
            let capacity = live_baseline_page_capacity(protocol_version, request_id)?;
            let page = download
                .as_mut()
                .context("connection has no live baseline download")?
                .read(*offset, capacity)?;
            if page.complete {
                *download = None;
            }
            Ok(RelayResponsePayload::LiveBaselinePage { page })
        }
        RelayRequest::AbortLiveBaseline => {
            *download = None;
            Ok(RelayResponsePayload::LiveBaselineAborted)
        }
        _ => anyhow::bail!("not a live baseline download request"),
    }
}
