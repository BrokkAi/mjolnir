//! Typed suspend/move consent and preflight contracts shared across the
//! checkpoint engine, daemon wire protocol, store, CLI, TUI, and web viewer.

use serde::{Deserialize, Serialize};
use std::fmt;
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CheckoutKey {
    pub repository_id: String,
    pub checkout_path: PathBuf,
}

impl Default for CheckoutKey {
    fn default() -> Self {
        Self {
            repository_id: "workspace".into(),
            checkout_path: PathBuf::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(transparent)]
pub struct FileIdentity(String);

impl FileIdentity {
    pub fn new(hex: String) -> Self {
        Self(hex)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum UntrackedItem {
    Path {
        checkout: CheckoutKey,
        path: PathBuf,
        identity: FileIdentity,
    },
    NestedRepository {
        checkout: CheckoutKey,
        path: PathBuf,
        identity: FileIdentity,
    },
    Stash {
        checkout: CheckoutKey,
        commit: String,
        message: String,
    },
    KeepDirFile {
        checkout: CheckoutKey,
        path: PathBuf,
        identity: FileIdentity,
    },
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub enum UntrackedConsent {
    #[default]
    None,
    Any,
    Shown(Vec<UntrackedItem>),
}

impl UntrackedConsent {
    pub fn is_none(&self) -> bool {
        matches!(self, Self::None)
    }

    pub fn covers(&self, item: &UntrackedItem) -> bool {
        match self {
            Self::None => false,
            Self::Any => true,
            Self::Shown(items) => items.iter().any(|shown| shown == item),
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum GitOperationKind {
    Merge,
    Rebase,
    CherryPick,
    Revert,
    Am,
    Bisect,
}

impl fmt::Display for GitOperationKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Merge => write!(f, "merge"),
            Self::Rebase => write!(f, "rebase"),
            Self::CherryPick => write!(f, "cherry-pick"),
            Self::Revert => write!(f, "revert"),
            Self::Am => write!(f, "am"),
            Self::Bisect => write!(f, "bisect"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GitOperation {
    pub checkout: CheckoutKey,
    pub kind: GitOperationKind,
}

pub type ShownOperation = GitOperation;

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub enum OperationConsent {
    #[default]
    None,
    Any,
    Shown(Vec<ShownOperation>),
}

impl OperationConsent {
    pub fn is_none(&self) -> bool {
        matches!(self, Self::None)
    }

    pub fn covers(&self, operation: &ShownOperation) -> bool {
        match self {
            Self::None => false,
            Self::Any => true,
            Self::Shown(items) => items.iter().any(|shown| shown == operation),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct CaptureConsent {
    #[serde(default)]
    pub untracked: UntrackedConsent,
    #[serde(default)]
    pub operations: OperationConsent,
    // Older workers ignore this field and keep requiring consent for these
    // files. They cannot silently discard them because of the new exemption.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub move_keep: Option<crate::session_keep::MoveKeep>,
}

/// Consent persisted on `sessions.closing_consent` when a suspend is accepted,
/// and sent over the daemon wire and HTTP API.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SuspendConsent {
    #[serde(default)]
    pub discard_untracked: UntrackedConsent,
    #[serde(default)]
    pub abort_operations: OperationConsent,
    #[serde(default)]
    pub without_checkpoint: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub confirm_no_checkpoint: bool,
}

impl SuspendConsent {
    pub fn capture_consent(&self) -> CaptureConsent {
        CaptureConsent {
            move_keep: None,
            untracked: self.discard_untracked.clone(),
            operations: self.abort_operations.clone(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct UntrackedPath {
    pub checkout: CheckoutKey,
    pub path: PathBuf,
    pub bytes: u64,
    pub identity: FileIdentity,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SuspendUntrackedStash {
    pub checkout: CheckoutKey,
    pub commit: String,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct KeepDirEntry {
    pub checkout: CheckoutKey,
    pub path: PathBuf,
    pub bytes: u64,
    pub identity: FileIdentity,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SuspendBlocker {
    pub kind: String,
    pub paths: Vec<String>,
    pub remedy: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ExistingCheckpointSummary {
    pub verified: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub age_seconds: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct DryRunEstimate {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_delta_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_delta_unavailable_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_state_bytes: Option<u64>,
    pub keep_dir_bytes: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transfer_route: Option<String>,
}

/// Target-side input for `mj-worker suspend-preflight --spec -`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct WorkerPreflightRepo {
    pub id: String,
    pub path: PathBuf,
    #[serde(default)]
    pub raw: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exclude_commits: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keep_dir: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin_override: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct WorkerSuspendPreflightSpec {
    pub session_id: String,
    pub repositories: Vec<WorkerPreflightRepo>,
    #[serde(default)]
    pub dry_run: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct WorkerSuspendPreflightResult {
    #[serde(default)]
    pub untracked: Vec<UntrackedPath>,
    #[serde(default)]
    pub nested_repositories: Vec<UntrackedPath>,
    #[serde(default)]
    pub untracked_stashes: Vec<SuspendUntrackedStash>,
    #[serde(default)]
    pub keep_dir_entries: Vec<KeepDirEntry>,
    #[serde(default)]
    pub keep_dir_bytes: u64,
    #[serde(default)]
    pub operations: Vec<ShownOperation>,
    #[serde(default)]
    pub raw: bool,
    #[serde(default)]
    pub blockers: Vec<SuspendBlocker>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dry_run: Option<DryRunEstimate>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SuspendPreflight {
    pub session_id: String,
    #[serde(default)]
    pub untracked: Vec<UntrackedPath>,
    #[serde(default)]
    pub nested_repositories: Vec<UntrackedPath>,
    #[serde(default)]
    pub untracked_stashes: Vec<SuspendUntrackedStash>,
    #[serde(default)]
    pub keep_dir_entries: Vec<KeepDirEntry>,
    #[serde(default)]
    pub keep_dir_bytes: u64,
    #[serde(default)]
    pub operations: Vec<ShownOperation>,
    #[serde(default)]
    pub raw: bool,
    #[serde(default)]
    pub interrupting: bool,
    #[serde(default)]
    pub children_not_handed_back: Vec<String>,
    #[serde(default)]
    pub blockers: Vec<SuspendBlocker>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub existing_checkpoint: Option<ExistingCheckpointSummary>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dry_run: Option<DryRunEstimate>,
}

impl SuspendPreflight {
    pub fn has_blockers(&self) -> bool {
        !self.blockers.is_empty()
    }

    pub fn untracked_count(&self) -> usize {
        self.untracked.len()
            + self.nested_repositories.len()
            + self.untracked_stashes.len()
            + self.keep_dir_entries.len()
    }

    pub fn has_warnings(&self) -> bool {
        self.untracked_count() > 0
            || !self.operations.is_empty()
            || self.raw
            || self.interrupting
            || !self.children_not_handed_back.is_empty()
    }

    pub fn shown_untracked_consent(&self) -> UntrackedConsent {
        let mut items = Vec::with_capacity(self.untracked_count());
        for entry in &self.untracked {
            items.push(UntrackedItem::Path {
                checkout: entry.checkout.clone(),
                path: entry.path.clone(),
                identity: entry.identity.clone(),
            });
        }
        for entry in &self.nested_repositories {
            items.push(UntrackedItem::NestedRepository {
                checkout: entry.checkout.clone(),
                path: entry.path.clone(),
                identity: entry.identity.clone(),
            });
        }
        for entry in &self.untracked_stashes {
            items.push(UntrackedItem::Stash {
                checkout: entry.checkout.clone(),
                commit: entry.commit.clone(),
                message: entry.message.clone(),
            });
        }
        for entry in &self.keep_dir_entries {
            items.push(UntrackedItem::KeepDirFile {
                checkout: entry.checkout.clone(),
                path: entry.path.clone(),
                identity: entry.identity.clone(),
            });
        }
        if items.is_empty() {
            UntrackedConsent::None
        } else {
            UntrackedConsent::Shown(items)
        }
    }

    pub fn shown_operation_consent(&self) -> OperationConsent {
        if self.operations.is_empty() {
            OperationConsent::None
        } else {
            OperationConsent::Shown(self.operations.clone())
        }
    }

    pub fn shown_consent(&self) -> SuspendConsent {
        SuspendConsent {
            discard_untracked: self.shown_untracked_consent(),
            abort_operations: self.shown_operation_consent(),
            without_checkpoint: false,
            confirm_no_checkpoint: false,
        }
    }

    /// Displayable list of all untracked/discarded paths for UIs and CLI refusals.
    pub fn untracked_display_paths(&self) -> Vec<String> {
        let mut paths = Vec::with_capacity(self.untracked_count());
        for entry in &self.untracked {
            paths.push(entry.path.display().to_string());
        }
        for entry in &self.nested_repositories {
            paths.push(format!("{} (nested repository)", entry.path.display()));
        }
        for entry in &self.untracked_stashes {
            paths.push(format!(
                "stash {} ({})",
                &entry.commit[..entry.commit.len().min(8)],
                entry.message
            ));
        }
        for entry in &self.keep_dir_entries {
            paths.push(format!(".mj/keep/{}", entry.path.display()));
        }
        paths
    }
}

/// Format the canonical agent/CLI refusal message for untracked files.
pub fn format_untracked_refusal(count: usize, first_paths: &[String]) -> String {
    format!(
        "{} Ignored files are never kept.",
        format_move_untracked_refusal(count, first_paths)
    )
}

pub fn format_move_untracked_refusal(count: usize, first_paths: &[String]) -> String {
    let preview: Vec<&str> = first_paths.iter().take(5).map(String::as_str).collect();
    let suffix = if count > preview.len() { "…" } else { "" };
    format!(
        "{count} untracked files will not be kept: {}{suffix} `git add` (or commit) anything you want to keep, or pass `--discard-untracked`.",
        preview.join(", ")
    )
}
