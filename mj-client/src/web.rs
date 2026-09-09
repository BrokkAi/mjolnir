//! Web-viewer data shared by Mjolnir's daemon and control surfaces.

use std::net::SocketAddr;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// The conversation shape the phone reads. The chat layer projects its
/// entries into this; the browser API owns the wire form.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BrowserTranscript {
    pub latest_seq: u64,
    /// Cursor boundary below which a client must replace its feed. Usually
    /// this is the oldest retained entry, but presentation coalescing may
    /// advance it so an append-only client drops a marker hidden by a newer
    /// entry.
    pub window_start_seq: u64,
    pub reset: bool,
    pub entries: Vec<BrowserTranscriptEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BrowserTranscriptEntry {
    pub id: u64,
    pub updated_seq: u64,
    pub role: &'static str,
    pub label: String,
    pub recorded_at_ms: Option<i64>,
    pub lines: Vec<String>,
    /// The glyph the terminal draws for this role, so both surfaces read alike
    /// without the browser keeping a second copy of the mapping. Taken from
    /// the same `entry_visual` the terminal renders from.
    pub glyph: &'static str,
    /// The semantic colour name, not a colour. The stylesheet decides what
    /// `agent` or `failed` looks like; this says which one applies.
    pub tone: &'static str,
    /// A tool call's state, for a tool entry. `None` for every other role.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_status: Option<&'static str>,
    /// The changed files a tool reported, as data rather than as extra lines
    /// appended to `lines`. The terminal formats these for a terminal; a
    /// browser re-parsing that formatting is how the phone came to render
    /// every diffstat as one unsplit path.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub diffstats: Vec<BrowserDiffStat>,
}

/// One file a tool changed, and by how much.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BrowserDiffStat {
    pub path: String,
    pub insertions: u32,
    pub deletions: u32,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum WebViewerAccess {
    Starting,
    Ready {
        viewer_url: String,
        viewer_code: String,
        qr_login_url: Option<String>,
        fallback_reason: Option<String>,
    },
    Failed {
        address: SocketAddr,
        message: String,
        port_conflict: bool,
    },
    Unavailable(String),
}

impl std::fmt::Debug for WebViewerAccess {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Starting => formatter.write_str("Starting"),
            Self::Ready {
                viewer_url,
                fallback_reason,
                ..
            } => formatter
                .debug_struct("Ready")
                .field("viewer_url", viewer_url)
                .field("credentials", &"[redacted]")
                .field("fallback_reason", fallback_reason)
                .finish(),
            Self::Failed {
                address,
                message,
                port_conflict,
            } => formatter
                .debug_struct("Failed")
                .field("address", address)
                .field("message", message)
                .field("port_conflict", port_conflict)
                .finish(),
            Self::Unavailable(message) => {
                formatter.debug_tuple("Unavailable").field(message).finish()
            }
        }
    }
}

/// Identity shown before an explicit stop request and checked again before signalling.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WebListenerProcess {
    pub pid: u32,
    pub name: String,
    pub executable: PathBuf,
    pub started_at: u64,
    pub stop_disabled_reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum WebViewerRecovery {
    Retry,
    AnotherPort,
    StopAndRetry(WebListenerProcess),
}
