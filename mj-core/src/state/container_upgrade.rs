use serde::{Deserialize, Serialize};

/// An engine inspection, never an inference from the daemon's own version.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContainerUpgradePreview {
    pub session_id: String,
    pub source_container_id: String,
    pub current_image: String,
    pub current_image_id: String,
    pub target_image: String,
    pub target_image_id: Option<String>,
    pub available: bool,
    pub retained_mounts: Vec<String>,
    pub warning: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub settings: Option<Box<ContainerSettingsChange>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContainerSettingsChange {
    pub resource_allocation: Option<super::SessionResourceAllocation>,
    pub mounts: Vec<crate::targets::AdditionalMount>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mount_history: Option<Vec<std::path::PathBuf>>,
}

pub const CONTAINER_UPGRADE_WARNING: &str = "The existing workspace and build cache mounts will be reattached. State in the container's writable layer, including manually installed packages, may be lost. A checkpoint does not preserve the whole environment. Busy workers finish their accepted work before the upgrade starts. You can cancel while it is pending.";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContainerUpgradeRequest {
    pub selection: ContainerUpgradePreview,
    pub acknowledge_writable_layer_loss: bool,
}
