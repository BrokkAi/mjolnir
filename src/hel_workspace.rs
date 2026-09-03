//! Durable user-facing workspace identities and name validation.

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

/// Stable identity assigned to the workspace that receives pre-workspace data.
pub const DEFAULT_WORKSPACE_ID: &str = "default";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceRecord {
    pub id: String,
    pub name: String,
    pub created_at: String,
    pub last_opened_at: String,
    /// Number of active sessions currently owned by this workspace.
    pub session_count: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DetachedDraft {
    pub id: String,
    pub workspace_id: String,
    pub session_id: Option<String>,
    pub source: String,
    pub owner_pid: Option<u32>,
    pub saved_at: String,
    pub text: String,
    pub recovered_at: Option<String>,
}

/// Normalize a display name and return the case-insensitive uniqueness key.
pub fn normalize_workspace_name(name: &str) -> Result<(String, String)> {
    let name = name.trim();
    let length = name.chars().count();
    if length == 0 {
        bail!("workspace name is empty");
    }
    if length > 64 {
        bail!("workspace name is longer than 64 characters");
    }
    if name.chars().any(char::is_control) {
        bail!("workspace name contains a control character");
    }
    Ok((name.to_owned(), name.to_lowercase()))
}

pub fn new_workspace_id() -> Result<String> {
    let mut random = [0_u8; 16];
    getrandom::fill(&mut random)
        .map_err(|error| anyhow::anyhow!("generate workspace id: {error}"))?;
    Ok(random.iter().map(|byte| format!("{byte:02x}")).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workspace_names_are_trimmed_and_case_folded() {
        assert_eq!(
            normalize_workspace_name("  Bifrost-Fuzz  ").unwrap(),
            ("Bifrost-Fuzz".to_owned(), "bifrost-fuzz".to_owned())
        );
    }

    #[test]
    fn workspace_names_reject_empty_long_and_control_text() {
        assert!(normalize_workspace_name("  ").is_err());
        assert!(normalize_workspace_name(&"x".repeat(65)).is_err());
        assert!(normalize_workspace_name("line\nbreak").is_err());
    }

    #[test]
    fn generated_workspace_ids_are_distinct_hex() {
        let first = new_workspace_id().unwrap();
        let second = new_workspace_id().unwrap();
        assert_eq!(first.len(), 32);
        assert!(first.bytes().all(|byte| byte.is_ascii_hexdigit()));
        assert_ne!(first, second);
    }
}
