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

impl SuspendBlocker {
    pub fn title(&self) -> &str {
        match self.kind.as_str() {
            "untracked" => "Untracked items",
            "operation_in_progress" => "Git operation in progress",
            "dirty_submodule" => "Dirty submodule",
            "missing_managed_origin" => "Missing Git origin",
            "origin_removed_by_teardown" => "Origin depends on clone",
            "unrepresented_git_state" => "External worktrees & Git state",
            "hidden_tracked_edits" => "Hidden tracked edits",
            "intent_to_add" => "Intent-to-add files",
            "untracked_submodule_registration" => "Untracked submodule registration",
            "tracked_keep_files" => "Tracked .mj/keep files",
            _ => &self.kind,
        }
    }
    pub fn is_auto_fixable(&self) -> bool {
        matches!(
            self.kind.as_str(),
            "hidden_tracked_edits" | "unrepresented_git_state"
        )
    }
}
impl SuspendPreflight {
    pub fn has_auto_fixable_blockers(&self) -> bool {
        self.blockers.iter().any(SuspendBlocker::is_auto_fixable)
    }
}
pub fn format_blocker_cleanup_prompt(blockers: &[SuspendBlocker]) -> String {
    let mut text = String::from(
        "Inspect and resolve these suspend blockers. Preserve or push wanted work, then report when ready to suspend.\n",
    );
    for blocker in blockers {
        text.push_str(&format!("\n{}\n", blocker.title()));
        for path in &blocker.paths {
            text.push_str(&format!("- {path}\n"));
        }
        text.push_str(&format!("Fix: {}\n", blocker.remedy));
    }
    text
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
    #[serde(default, skip_serializing_if = "is_zero")]
    pub owning_children_to_suspend: usize,
    #[serde(default)]
    pub blockers: Vec<SuspendBlocker>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub existing_checkpoint: Option<ExistingCheckpointSummary>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dry_run: Option<DryRunEstimate>,
}

fn is_zero(count: &usize) -> bool {
    *count == 0
}

pub fn owning_children_suspend_warning(count: usize) -> Option<String> {
    match count {
        0 => None,
        1 => Some("Suspending also suspends one child with its own environment and drops that child's untracked files.".into()),
        _ => Some(format!("Suspending also suspends {count} children with their own environments and drops their untracked files.")),
    }
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
            || self.owning_children_to_suspend > 0
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

/// Report a sample of what the final checkpoint inventory discarded.
pub fn format_dropped_untracked(items: &[UntrackedItem]) -> String {
    // A summary samples paths; its count covers the entire inventory.
    let sample = items
        .iter()
        .take(5)
        .map(|item| match item {
            UntrackedItem::Path { path, .. } | UntrackedItem::NestedRepository { path, .. } => {
                format!("{:?}", path)
            }
            UntrackedItem::KeepDirFile { path, .. } => {
                format!("{:?}", std::path::Path::new(".mj/keep").join(path))
            }
            UntrackedItem::Stash { commit, .. } => {
                format!("stash {}", &commit[..commit.len().min(8)])
            }
        })
        .collect::<Vec<_>>()
        .join(", ");
    format!("{} ({sample})", items.len())
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

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerResolveBlockersSpec {
    pub repositories: Vec<WorkerPreflightRepo>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MovedWorktreeSummary {
    pub from: String,
    pub to: String,
}
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerResolveBlockersResult {
    pub cleared_hidden_paths: Vec<String>,
    pub moved_worktrees: Vec<MovedWorktreeSummary>,
    pub skipped_worktrees: Vec<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResolveSuspendBlockersResponse {
    pub cleared_hidden_paths: Vec<String>,
    pub moved_worktrees: Vec<MovedWorktreeSummary>,
    pub skipped_worktrees: Vec<String>,
    pub preflight: SuspendPreflight,
}
impl ResolveSuspendBlockersResponse {
    pub fn summary(&self) -> String {
        format!(
            "Cleared flags on {} paths; moved {} worktrees; skipped {} worktrees.",
            self.cleared_hidden_paths.len(),
            self.moved_worktrees.len(),
            self.skipped_worktrees.len()
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn suspend_blocker_wire_schema_preserves_legacy_kinds_and_rejects_unknown_fields() {
        let legacy = r#"{"kind":"HiddenTrackedEdits legacy detail","paths":["linked/file"],"remedy":"preserve work"}"#;
        let blocker: SuspendBlocker = serde_json::from_str(legacy).unwrap();
        assert_eq!(blocker.title(), "HiddenTrackedEdits legacy detail");
        assert_eq!(
            serde_json::from_str::<SuspendBlocker>(&serde_json::to_string(&blocker).unwrap())
                .unwrap(),
            blocker
        );
        assert!(
            serde_json::from_str::<SuspendBlocker>(
                r#"{"kind":"hidden_tracked_edits","paths":[],"remedy":"","extra":true}"#
            )
            .is_err()
        );
    }
}
