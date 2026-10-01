//! Shared project catalog views consumed by directory and project pickers.

use crate::repository::{ProjectBundleSnapshot, RepositoryIdentity};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedProject {
    pub bundle_id: String,
    pub name: String,
    pub project: ProjectBundleSnapshot,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectLocation {
    pub host: String,
    /// The original path is retained for lookup; suggestions use checkout_root.
    pub directory: PathBuf,
    pub checkout_root: PathBuf,
    pub repository_root: PathBuf,
    pub identity: RepositoryIdentity,
    pub seen_at: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ProjectCatalogStatus {
    #[default]
    Ready,
    Refreshing,
    Failed {
        errors: Vec<String>,
    },
}

impl ProjectCatalogStatus {
    /// Discovery details are logged by the daemon; keep picker notices bounded.
    pub fn failure_summary(&self) -> Option<String> {
        let Self::Failed { errors } = self else {
            return None;
        };
        let count = errors.len();
        let noun = if count == 1 { "error" } else { "errors" };
        Some(format!(
            "Project discovery reported {count} {noun}. See daemon logs for details."
        ))
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectCatalogView {
    pub projects: Vec<SavedProject>,
    pub locations: Vec<ProjectLocation>,
    pub status: ProjectCatalogStatus,
}
