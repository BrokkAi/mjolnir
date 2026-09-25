//! Shared request and response types for repository discovery.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ProjectDiscoveryRequest {
    Github {
        query: String,
    },
    Directory {
        path: String,
        #[serde(default)]
        filter: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectDiscovery {
    pub entries: Vec<ProjectEntry>,
    pub directory: Option<String>,
    pub parent: Option<String>,
    pub truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectEntry {
    pub name: String,
    pub source: String,
    pub description: String,
    pub kind: ProjectEntryKind,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProjectEntryKind {
    Repository,
    Directory,
}
