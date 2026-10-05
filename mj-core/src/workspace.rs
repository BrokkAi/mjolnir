//! Durable user-facing workspace identities and name validation.

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

/// Stable identity assigned to the workspace that receives pre-workspace data.
pub const DEFAULT_WORKSPACE_ID: &str = "default";

/// The explicit height requested for one support pane in the dashboard.
///
/// This lives with the workspace model because the dashboard persists the
/// user's layout per workspace. The serialized spelling is part of that
/// persisted representation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PaneSize {
    Minimized,
    #[default]
    Standard,
    Maximized,
}

impl PaneSize {
    /// The next title-bar control, wrapping from maximum to minimum.
    #[must_use]
    pub const fn cycled(self) -> Self {
        match self {
            Self::Minimized => Self::Standard,
            Self::Standard => Self::Maximized,
            Self::Maximized => Self::Minimized,
        }
    }
}

/// Persisted sizes for the dashboard's three support panes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct PaneSizes {
    pub sessions: PaneSize,
    pub targets: PaneSize,
    pub quota: PaneSize,
}

impl PaneSizes {
    /// Validate the exclusive maximum before applying a saved arrangement.
    pub fn validate(&self) -> Result<()> {
        let maximized = [self.sessions, self.targets, self.quota]
            .into_iter()
            .filter(|size| *size == PaneSize::Maximized)
            .count();
        if maximized > 1 {
            bail!("at most one pane can be maximized");
        }
        Ok(())
    }

    #[must_use]
    pub fn all_standard(self) -> bool {
        [self.sessions, self.targets, self.quota]
            .into_iter()
            .all(|size| size == PaneSize::Standard)
    }
}

/// Which way a split divides its pane.
///
/// `Horizontal` means the two children sit side by side, so the divider drawn
/// between them is vertical. This matches ratatui's `Direction::Horizontal`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SplitAxis {
    Horizontal,
    Vertical,
}

/// One node of the conversation area's binary space partition: either a pane
/// or a split holding two children.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum LayoutNode {
    Pane {
        id: u32,
    },
    Split {
        axis: SplitAxis,
        /// The first child's share of the split, between 0.1 and 0.9.
        ratio: f32,
        first: Box<LayoutNode>,
        second: Box<LayoutNode>,
    },
}

impl LayoutNode {
    /// Collect every pane id in tree order, reporting the first duplicate.
    fn collect_pane_ids(&self, found: &mut Vec<u32>) -> Result<()> {
        match self {
            Self::Pane { id } => {
                if found.contains(id) {
                    bail!("pane id {id} appears more than once in the layout");
                }
                found.push(*id);
            }
            Self::Split {
                ratio,
                first,
                second,
                ..
            } => {
                if !ratio.is_finite() {
                    bail!("split ratio {ratio} is not a finite number");
                }
                if !(0.1..=0.9).contains(ratio) {
                    bail!("split ratio {ratio} is outside 0.1..=0.9");
                }
                first.collect_pane_ids(found)?;
                second.collect_pane_ids(found)?;
            }
        }
        Ok(())
    }
}

/// The persisted arrangement of the dashboard's conversation area for one
/// workspace: the pane tree, the focused pane, and the session each pane
/// shows. A pane with no entry in `sessions` is empty.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConversationLayout {
    pub root: LayoutNode,
    pub focus: u32,
    pub sessions: std::collections::BTreeMap<u32, String>,
    /// Missing only in layouts saved before Browse/pins existed.
    #[serde(default)]
    pub browse: Option<u32>,
    #[serde(default)]
    pub pins: std::collections::BTreeMap<String, u32>,
}

impl Default for ConversationLayout {
    fn default() -> Self {
        Self {
            root: LayoutNode::Pane { id: 1 },
            focus: 1,
            sessions: std::collections::BTreeMap::new(),
            browse: Some(1),
            pins: std::collections::BTreeMap::new(),
        }
    }
}

impl ConversationLayout {
    /// Validate a layout before applying or storing it.
    pub fn validate(&self) -> Result<()> {
        let mut ids = Vec::new();
        self.root.collect_pane_ids(&mut ids)?;
        if !ids.contains(&self.focus) {
            bail!("focused pane {} is not in the layout", self.focus);
        }
        if let Some(browse) = self.browse {
            if !ids.contains(&browse) {
                bail!("Browse pane {browse} is not in the layout");
            }
            let mut badges = std::collections::BTreeSet::new();
            for (session, badge) in &self.pins {
                if !self
                    .sessions
                    .iter()
                    .any(|(pane, shown)| *pane != browse && shown == session)
                {
                    bail!("pin {session} is not in a pinned pane");
                }
                if !badges.insert(badge) {
                    bail!("duplicate pin badge {badge}");
                }
            }
        }
        for pane in self.sessions.keys() {
            if !ids.contains(pane) {
                bail!("session is recorded for pane {pane}, which is not in the layout");
            }
        }
        Ok(())
    }
}

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
mod pin_layout_tests {
    use super::*;

    #[test]
    fn legacy_layouts_decode_without_inventing_a_browse_location() {
        let layout: ConversationLayout = serde_json::from_value(serde_json::json!({
            "root": {"kind": "pane", "id": 17}, "focus": 17, "sessions": {"17": "session"}
        }))
        .unwrap();
        assert_eq!(layout.browse, None);
        assert!(layout.pins.is_empty());
        assert!(layout.validate().is_ok());
    }
}
