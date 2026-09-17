use super::*;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClaudeSessionSelection {
    NativeSessionId(String),
    Latest,
}

pub type CodexSessionSelection = ClaudeSessionSelection;
pub type KimiSessionSelection = ClaudeSessionSelection;
pub type GrokSessionSelection = ClaudeSessionSelection;

#[derive(Debug, Clone)]
pub struct LocatedClaudeSession {
    pub native_session_id: String,
    pub jsonl_path: PathBuf,
    pub modified_at: SystemTime,
    pub title: String,
    pub cwd: PathBuf,
    pub git_branch: String,
    pub size_bytes: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodexHistoryMode {
    Legacy,
    Paginated,
}

/// Grok Build's conversation of record inside a session directory.
pub(super) const CHAT_HISTORY: &str = "chat_history.jsonl";

pub const CODEX_LEGACY_IMPORT_ISSUE: &str = "Legacy Codex history cannot be imported. Run codex migrate-rollouts --apply, then reopen \
     this dialog.";

impl CodexHistoryMode {
    pub fn import_issue(self) -> Option<&'static str> {
        match self {
            Self::Legacy => Some(CODEX_LEGACY_IMPORT_ISSUE),
            Self::Paginated => None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct LocatedCodexSession {
    pub native_session_id: String,
    pub jsonl_path: PathBuf,
    pub modified_at: SystemTime,
    pub title: String,
    pub cwd: PathBuf,
    pub git_branch: String,
    pub size_bytes: u64,
    pub history_mode: CodexHistoryMode,
    /// Archived inside Codex itself. Hel mirrors that one way: the row is
    /// hidden by default and never written back to Codex.
    pub natively_archived: bool,
}

#[derive(Debug, Clone)]
pub struct LocatedKimiSession {
    pub native_session_id: String,
    pub session_path: PathBuf,
    pub modified_at: SystemTime,
    pub title: String,
    pub cwd: PathBuf,
    pub git_branch: String,
    pub size_bytes: u64,
}

/// Grok Build keeps one directory per session, like Kimi Code.
pub type LocatedGrokSession = LocatedKimiSession;

#[derive(Debug, Clone)]
pub struct SessionScanProgress<T> {
    pub scanned: usize,
    pub total: usize,
    pub session: Option<T>,
}

#[derive(Debug)]
pub(super) struct FileScanCandidate {
    pub(super) path: PathBuf,
    pub(super) modified_at: SystemTime,
    pub(super) size_bytes: u64,
}

#[derive(Debug)]
pub(super) struct KimiScanCandidate {
    pub(super) native_session_id: String,
    pub(super) session_path: PathBuf,
    pub(super) modified_at: SystemTime,
    pub(super) title: String,
    pub(super) cwd: PathBuf,
}

#[derive(Debug)]
pub(super) struct CodexSessionMetadata {
    pub(super) id: String,
    pub(super) cwd: PathBuf,
    pub(super) git_branch: String,
    pub(super) history_mode: CodexHistoryMode,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaudeTranscript {
    pub cwd: PathBuf,
    /// Files reliably reported as edited by the native harness.
    pub edited_paths: Vec<PathBuf>,
    pub events: Vec<SequencedEvent>,
}

pub type CodexTranscript = ClaudeTranscript;
pub type KimiTranscript = ClaudeTranscript;
pub type GrokTranscript = ClaudeTranscript;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BundleResolution {
    Existing(String),
    /// The caller must ask the user before adding this to their config.
    Synthesized {
        id: String,
        bundle: ProjectBundle,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionEditTargets {
    pub git_roots: Vec<PathBuf>,
    /// Git roots under a temporary directory. They are throwaway workspaces
    /// rather than project repositories, so the import omits them.
    pub scratch_git_roots: Vec<PathBuf>,
    pub non_git_dirs: Vec<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportSafetyIssues {
    pub dirty_git_roots: Vec<(PathBuf, String)>,
    pub omitted_non_git_dirs: Vec<PathBuf>,
    pub scratch_git_roots: Vec<PathBuf>,
    pub has_untracked_files: bool,
}
