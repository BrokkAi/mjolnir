//! Move's file selection and temporary workspace-transfer contracts.
//!
//! These are not checkpoint payloads. File bodies never travel in these values.
use std::collections::BTreeSet;
use std::path::{Component, Path, PathBuf};

use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};

pub const LARGE_TRANSFER_BYTES: u64 = 1_000_000_000;
pub const SUBSTANTIAL_ROOT_BYTES: u64 = 100_000_000;

/// Worker utility input. Large content is written to disk, never returned here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum WorkspaceCommand {
    Inspect {
        repositories: Vec<WorkspaceRepository>,
    },
    Capture {
        repositories: Vec<WorkspaceRepository>,
        selection: WorkspaceSelection,
        destination: PathBuf,
    },
    Restore {
        source: PathBuf,
        repositories: Vec<WorkspaceRepository>,
    },
    Verify {
        source: PathBuf,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceRepository {
    pub id: String,
    pub root: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TransferFile {
    pub path: PathBuf,
    pub bytes: u64,
    pub sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TransferredRepository {
    pub id: String,
    pub head: String,
    pub branch: Option<String>,
    pub refs: Vec<(String, String)>,
    pub symbolic_refs: Vec<(String, String)>,
    pub stash_log: Option<String>,
    pub status: String,
    pub untracked: Vec<WorkspacePath>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceManifest {
    pub version: u32,
    pub repositories: Vec<TransferredRepository>,
    pub files: Vec<TransferFile>,
}

/// Durable ownership of a temporary transfer, distinct from checkpoint history.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceTransfer {
    pub assessment: WorkspaceAssessment,
    pub source: Box<crate::state::SessionRecord>,
    pub source_stage: PathBuf,
    pub controller_stage: PathBuf,
    pub phase: WorkspaceTransferPhase,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceTransferPhase {
    Planned,
    Captured,
    Downloaded,
    SourceStopped,
    Restored,
    Ready,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceSelection {
    #[serde(default)]
    pub exclusions: Vec<WorkspacePath>,
    #[serde(default)]
    pub acknowledge_large_transfer: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspacePath {
    pub repository: String,
    pub path: PathBuf,
}

/// The selection is valid but the caller has not agreed to a large transfer.
/// A distinct type lets callers offer the flags that give that consent, and
/// only for this refusal.
#[derive(Debug)]
pub struct LargeTransferConsentRequired(String);

impl std::fmt::Display for LargeTransferConsentRequired {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Move includes {}; review Choose files and acknowledge the large transfer",
            self.0
        )
    }
}

impl std::error::Error for LargeTransferConsentRequired {}

impl WorkspacePath {
    pub fn validate(&self) -> Result<()> {
        ensure!(!self.repository.is_empty(), "Move repository is missing");
        ensure!(
            !self.path.as_os_str().is_empty()
                && self
                    .path
                    .components()
                    .all(|c| matches!(c, Component::Normal(_))),
            "Move path must be relative without parent traversal: {}",
            self.path.display()
        );
        Ok(())
    }

    pub fn contains(&self, other: &Self) -> bool {
        self.repository == other.repository && other.path.starts_with(&self.path)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceFile {
    pub location: WorkspacePath,
    pub bytes: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceAssessment {
    pub files: Vec<WorkspaceFile>,
    #[serde(default)]
    pub roots: Vec<FileNode>,
    #[serde(default)]
    pub initially_expanded: BTreeSet<WorkspacePath>,
    /// Git history and tracked edits always travel; only untracked paths are selectable.
    pub required_bytes: u64,
    pub ignored_files: u64,
    pub ignored_bytes: u64,
    pub credential_files: u64,
    pub blockers: Vec<String>,
    #[serde(default)]
    pub storage: Vec<WorkspaceStorage>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceStorage {
    #[serde(default)]
    pub filesystem_key: String,
    #[serde(default)]
    pub allocations: BTreeSet<String>,
    pub location: String,
    pub available_bytes: u64,
    pub copies: u64,
}

impl WorkspaceAssessment {
    /// Capacity changes with the selection; consent is provided by Continue.
    pub fn selection_problem(&self, selection: &WorkspaceSelection) -> Option<String> {
        let mut acknowledged = selection.clone();
        acknowledged.acknowledge_large_transfer = true;
        acknowledged
            .validate(self)
            .err()
            .map(|error| error.to_string())
    }
}

impl WorkspaceSelection {
    pub fn includes(&self, path: &WorkspacePath) -> bool {
        !self
            .exclusions
            .iter()
            .any(|excluded| excluded.contains(path))
    }

    pub fn included_bytes(&self, assessment: &WorkspaceAssessment) -> u64 {
        assessment
            .files
            .iter()
            .filter(|f| self.includes(&f.location))
            .fold(assessment.required_bytes, |sum, f| {
                sum.saturating_add(f.bytes)
            })
    }

    pub fn validate(&self, assessment: &WorkspaceAssessment) -> Result<()> {
        for path in &self.exclusions {
            path.validate()?;
            ensure!(
                assessment.files.iter().any(|f| path.contains(&f.location)),
                "excluded path is not an eligible untracked file or directory: {}/{}",
                path.repository,
                path.path.display()
            );
        }
        ensure!(
            assessment.blockers.is_empty(),
            "{}",
            assessment.blockers.join("; ")
        );
        let bytes = self.included_bytes(assessment);
        for storage in &assessment.storage {
            let required = bytes
                .saturating_mul(storage.copies)
                .saturating_add(64 * 1024 * 1024);
            ensure!(
                storage.available_bytes >= required,
                "{} needs approximately {} free for this selection; {} available. Choose fewer files or free space before Move",
                storage.location,
                format_bytes(required),
                format_bytes(storage.available_bytes)
            );
        }

        if bytes >= LARGE_TRANSFER_BYTES && !self.acknowledge_large_transfer {
            return Err(anyhow::Error::new(LargeTransferConsentRequired(
                format_bytes(bytes),
            )));
        }
        Ok(())
    }

    /// A checked parent includes all its descendants; an unchecked parent
    /// excludes them. Selecting a child inside an excluded parent expands the
    /// exclusion into the other exact files, preserving their selection.
    pub fn set_included(
        &mut self,
        assessment: &WorkspaceAssessment,
        path: &WorkspacePath,
        included: bool,
    ) {
        let excluded: BTreeSet<_> = assessment
            .files
            .iter()
            .filter(|file| {
                if path.contains(&file.location) {
                    !included
                } else {
                    !self.includes(&file.location)
                }
            })
            .map(|file| file.location.clone())
            .collect();
        self.exclusions = excluded.into_iter().collect();
        self.acknowledge_large_transfer = false;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileSelectionState {
    Included,
    Excluded,
    Mixed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileNode {
    pub location: WorkspacePath,
    pub bytes: u64,
    pub files: usize,
    pub children: Vec<FileNode>,
}

impl FileNode {
    pub fn state(
        &self,
        assessment: &WorkspaceAssessment,
        selection: &WorkspaceSelection,
    ) -> FileSelectionState {
        let mut included = false;
        let mut excluded = false;
        for file in &assessment.files {
            if self.location.contains(&file.location) {
                if selection.includes(&file.location) {
                    included = true;
                } else {
                    excluded = true;
                }
            }
        }
        match (included, excluded) {
            (true, true) => FileSelectionState::Mixed,
            (true, false) => FileSelectionState::Included,
            _ => FileSelectionState::Excluded,
        }
    }

    pub fn label(&self) -> String {
        format!(
            "{}/{}{} — {}",
            self.location.repository,
            self.location.path.display(),
            if self.children.is_empty() { "" } else { "/" },
            format_bytes(self.bytes)
        )
    }
}

pub fn file_tree(assessment: &WorkspaceAssessment) -> Vec<FileNode> {
    fn insert(nodes: &mut Vec<FileNode>, file: &WorkspaceFile, parts: &[PathBuf], index: usize) {
        let path = &parts[index];
        let position = nodes
            .iter()
            .position(|n| {
                n.location.repository == file.location.repository && n.location.path == *path
            })
            .unwrap_or_else(|| {
                nodes.push(FileNode {
                    location: WorkspacePath {
                        repository: file.location.repository.clone(),
                        path: path.clone(),
                    },
                    bytes: 0,
                    files: 0,
                    children: Vec::new(),
                });
                nodes.len() - 1
            });
        let node = &mut nodes[position];
        node.bytes = node.bytes.saturating_add(file.bytes);
        node.files += 1;
        if index + 1 < parts.len() {
            insert(&mut node.children, file, parts, index + 1);
        }
    }
    fn finish(nodes: &mut [FileNode]) {
        for node in nodes.iter_mut() {
            finish(&mut node.children);
            while node.children.len() == 1 && !node.children[0].children.is_empty() {
                *node = node.children.remove(0);
            }
        }
        nodes.sort_by(|a, b| {
            b.bytes
                .cmp(&a.bytes)
                .then_with(|| a.location.cmp(&b.location))
        });
    }
    let mut roots = Vec::new();
    for file in &assessment.files {
        let parts: Vec<_> = file
            .location
            .path
            .ancestors()
            .filter(|p| !p.as_os_str().is_empty())
            .map(Path::to_path_buf)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        if !parts.is_empty() {
            insert(&mut roots, file, &parts, 0);
        }
    }
    finish(&mut roots);
    roots
}

pub fn initial_expansion(roots: &[FileNode]) -> BTreeSet<WorkspacePath> {
    let mut expanded = BTreeSet::new();
    let mut visible: Vec<_> = roots
        .iter()
        .filter(|n| n.bytes >= SUBSTANTIAL_ROOT_BYTES)
        .take(8)
        .collect();
    loop {
        visible.sort_by(|a, b| {
            b.bytes
                .cmp(&a.bytes)
                .then_with(|| a.location.cmp(&b.location))
        });
        let candidate = visible.iter().position(|node| {
            let children = node
                .children
                .iter()
                .filter(|n| n.bytes >= SUBSTANTIAL_ROOT_BYTES)
                .collect::<Vec<_>>();
            !expanded.contains(&node.location)
                && !children.is_empty()
                && children.iter().any(|n| !n.children.is_empty())
                && visible.len() + children.len() <= 8
        });
        let Some(index) = candidate else {
            break;
        };
        let node = visible[index];
        expanded.insert(node.location.clone());
        visible.extend(
            node.children
                .iter()
                .filter(|n| n.bytes >= SUBSTANTIAL_ROOT_BYTES),
        );
    }
    expanded
}

pub fn format_bytes(bytes: u64) -> String {
    if bytes >= 1_000_000_000 {
        format!("{:.2} GB", bytes as f64 / 1_000_000_000.0)
    } else if bytes >= 1_000_000 {
        format!("{:.1} MB", bytes as f64 / 1_000_000.0)
    } else if bytes >= 1_000 {
        format!("{:.1} KB", bytes as f64 / 1_000.0)
    } else {
        format!("{bytes} B")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn file(path: &str, bytes: u64) -> WorkspaceFile {
        WorkspaceFile {
            location: WorkspacePath {
                repository: "bifrost".into(),
                path: path.into(),
            },
            bytes,
        }
    }
    #[test]
    fn large_transfer_consent_starts_at_one_decimal_gigabyte() {
        let mut assessment = WorkspaceAssessment {
            required_bytes: LARGE_TRANSFER_BYTES - 1,
            ..Default::default()
        };
        let mut selection = WorkspaceSelection::default();
        assert!(selection.validate(&assessment).is_ok());
        assessment.required_bytes += 1;
        assert!(selection.validate(&assessment).is_err());
        selection.acknowledge_large_transfer = true;
        assert!(selection.validate(&assessment).is_ok());
        assessment.required_bytes += 1;
        assert!(selection.validate(&assessment).is_ok());
    }
    #[test]
    fn exclusions_cannot_name_tracked_or_parent_paths() {
        let assessment = WorkspaceAssessment {
            files: vec![file("output/binary", 42)],
            ..Default::default()
        };
        for path in ["../output", "/output", "tracked.rs"] {
            let selection = WorkspaceSelection {
                exclusions: vec![file(path, 0).location],
                ..Default::default()
            };
            assert!(selection.validate(&assessment).is_err());
        }
    }
}

/// A stopped source retained because the user excluded files from a Move.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RetainedMoveSource {
    pub operation_id: String,
    pub session_id: String,
    pub source: crate::state::SessionRecord,
    pub exclusions: Vec<WorkspacePath>,
    pub created_at: String,
}
