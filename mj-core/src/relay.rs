//! Shared relay messages, deterministic state and framing. Durable execution lives in mj-worker.
pub mod capacity;
pub mod protocol;
#[doc(hidden)]
pub mod snapshot;
mod types;
use crate::archive::CanonicalQueuedPrompt;
use crate::clock::epoch_millis;
use anyhow::{Context, Result, bail};
pub use capacity::*;
pub use protocol::*;
use serde::{Deserialize, Serialize};
pub use snapshot::*;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
pub use types::*;

pub const MAX_FRAME_BYTES: usize = 8 * 1024 * 1024;
/// Serialized bytes of observations allowed in one attach response, well under
/// `MAX_FRAME_BYTES` to leave room for the envelope.
pub const RELAY_REPLAY_BYTE_BUDGET: usize = 4 * 1024 * 1024;
/// A single durable command must remain comfortably smaller than a relay
/// frame. Commands are repeated in the event journal and private dispatch
/// state, so admitting a frame-sized command would make later attaches
/// impossible to encode.
pub const RELAY_COMMAND_BYTE_BUDGET: usize = 1024 * 1024;
/// Every event must fit by itself in a replay page.
pub const RELAY_EVENT_BYTE_BUDGET: usize = 2 * 1024 * 1024;
/// The public operational state shares an attach frame with a replay page.
pub const RELAY_STATE_BYTE_BUDGET: usize = 2 * 1024 * 1024;
/// Bytes of terminal output one journal entry keeps. The agent read the whole
/// stream over `terminal/output`; the journal copy is for the person reading
/// the transcript, so it keeps the tail and stays far below the event budget.
pub const TERMINAL_JOURNAL_OUTPUT_BYTES: usize = 256 * 1024;
/// Headroom left for an event's envelope — ordinals, digests, timestamp and
/// command id — when clamping an observation to `RELAY_EVENT_BYTE_BUDGET`.
pub const RELAY_EVENT_ENVELOPE_RESERVE: usize = 8 * 1024;
/// Clamping never shortens a string below this. Identifiers, type tags and
/// paths stay whole; only genuinely large payloads are candidates.
pub const RELAY_TRUNCATION_FLOOR: usize = 4 * 1024;
/// The private snapshot also has a hard ceiling so repeated accepted commands
/// cannot grow the durable state file without bound between checkpoints.
pub const RELAY_SNAPSHOT_BYTE_BUDGET: usize = 16 * 1024 * 1024;
/// Current durable ACP relay protocol. Peers that only speak an older
/// version in [`RELAY_MIN_PROTOCOL_VERSION`]..=this range still connect.
/// Protocol 0 is the retired pre-relay worker protocol and is rejected.
pub const RELAY_PROTOCOL_VERSION: u32 = 9;
pub const RELAY_MIN_PROTOCOL_VERSION: u32 = 1;
/// Digest for the empty relay event prefix (ordinal zero).
pub const RELAY_EVENT_GENESIS_DIGEST: &str = crate::archive::EVENT_FRONTIER_GENESIS_DIGEST;
/// Domain separator for a v1 (chained) relay event digest. v1 records fold
/// `previous_digest` into the hash, forming a linked chain.
pub const RELAY_EVENT_DIGEST_DOMAIN: &[u8] = b"hel-relay-event-v1\0";
/// Domain separator for a v2 (self-describing) relay event digest. v2 records
/// carry no `previous_digest`; the digest depends only on the record's own
/// content, so a corrupt record can never invalidate its neighbours.
pub const RELAY_EVENT_DIGEST_DOMAIN_V2: &[u8] = b"hel-relay-event-v2\0";
/// Snapshots at or below this schema are readable; a newer schema is rejected.
/// A v1 snapshot is upgraded in place to the current schema on open (its stored
/// frontier digests stay valid, since each is recomputed with the formula that
/// matches the record's format).
pub const RELAY_STATE_VERSION: u32 = 5;
/// The relay snapshot inside a worker root. Teardown and restore name it from
/// here rather than repeating the literal.
pub const RELAY_STATE_FILE: &str = "relay-state.json";
/// The relay's durable event journal inside a worker root.
pub const RELAY_JOURNAL_DIR: &str = "relay-journal";
/// The seed a checkpoint restore leaves in a worker root for the relay that
/// opens next. It carries only what a fresh relay cannot derive on its own.
pub const RESTORED_RELAY_SEED_FILE: &str = "relay-seed.json";
/// File in which a running worker daemon records its own PID, inside its
/// worker root. Session teardown reads it to stop that daemon before the root
/// it writes to is removed.
pub const WORKER_PID_FILE: &str = "worker.pid";
pub const RELAY_ACTIVE_SEGMENT: &str = "active.jsonl";
pub const RELAY_SEGMENT_BYTE_LIMIT: u64 = 1024 * 1024;
/// Journal bytes a restart may have to replay before the snapshot is rewritten.
/// Transcript observations are reconstructed by that replay, so they are
/// journaled without their own snapshot write until this much has accumulated.
pub const RELAY_SNAPSHOT_LAG_BYTE_LIMIT: usize = 1024 * 1024;
pub const RELAY_HOT_EVENT_CAPACITY: usize = 32;
pub const RELAY_REPLAY_CURSOR_CAPACITY: usize = 32;
pub const NATIVE_SESSION_IDENTITY_FILE: &str = "native-session.json";

/// Remove controller-only context that an ACP harness copied into a user-facing
/// prompt or title. Hidden context is prepended as reserved XML-like blocks;
/// an unterminated reserved block is treated as a truncated hidden value, not
/// as text safe to display.
pub fn strip_hidden_prompt_context(mut text: &str) -> &str {
    loop {
        text = text.trim_start();
        let Some(after_open) = text.strip_prefix('<') else {
            return text;
        };
        let Some(open_end) = after_open.find('>') else {
            return if reserved_hidden_context_prefix(after_open) {
                ""
            } else {
                text
            };
        };
        let tag = &after_open[..open_end];
        if !reserved_hidden_context_tag(tag) {
            return text;
        }
        let close = format!("</{tag}>");
        let after_open = &after_open[open_end + 1..];
        let Some(close_start) = after_open.rfind(&close) else {
            return "";
        };
        text = &after_open[close_start + close.len()..];
    }
}

fn reserved_hidden_context_prefix(text: &str) -> bool {
    text.starts_with("hel-") || text.starts_with("mj-") || text.starts_with("user_shell_command")
}

fn reserved_hidden_context_tag(tag: &str) -> bool {
    ((tag.starts_with("hel-") || tag.starts_with("mj-"))
        && tag
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-'))
        || tag == "user_shell_command"
}
/// Process-local clock for inbound ACP traffic. It deliberately stays out of
/// the durable relay journal: this is render timing, not recoverable session
/// history.
#[derive(Debug, Clone, Default)]
pub struct AcpActivityClock(Arc<AtomicI64>);

impl AcpActivityClock {
    pub fn mark(&self) {
        self.0.store(epoch_millis(), Ordering::Release);
    }

    pub fn last_at_ms(&self) -> Option<i64> {
        let value = self.0.load(Ordering::Acquire);
        (value > 0).then_some(value)
    }
}

/// What a restored relay needs from the checkpoint it continues.
///
/// A restore used to leave the whole canonical session here, but the relay only
/// ever read three fields from it. On a large session the unread transcript was
/// tens of megabytes written on the target and parsed again at worker start.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RestoredRelaySeed {
    pub event_frontier: u64,
    pub event_frontier_digest: String,
    /// Commands the archived session still had queued, already filtered by the
    /// restore spec's queue disposition.
    #[serde(default)]
    pub queued_prompts: Vec<CanonicalQueuedPrompt>,
}

impl RestoredRelaySeed {
    /// The same frontier checks a canonical session snapshot carries, so a
    /// malformed seed is refused before it can become relay state.
    pub fn validate(&self) -> Result<()> {
        validate_relay_digest(
            &self.event_frontier_digest,
            "restored relay event frontier digest",
        )?;
        if (self.event_frontier == 0) != (self.event_frontier_digest == RELAY_EVENT_GENESIS_DIGEST)
        {
            bail!("restored relay event frontier and genesis digest disagree");
        }
        Ok(())
    }
}

pub fn restored_relay_seed_path(relay_root: &Path) -> PathBuf {
    relay_root.join(RESTORED_RELAY_SEED_FILE)
}

pub fn clear_native_session_identity(root: &Path) -> Result<()> {
    let path = root.join(NATIVE_SESSION_IDENTITY_FILE);
    match std::fs::remove_file(&path) {
        Ok(()) => {
            #[cfg(unix)]
            std::fs::File::open(root)?.sync_all()?;
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("remove {}", path.display())),
    }
}
