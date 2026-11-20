// Port of mj-core/src/state/container_upgrade.rs.

package core

// ContainerUpgradePreview is an engine inspection, never an inference from
// the daemon's own version.
type ContainerUpgradePreview struct {
	SessionID         string   `json:"session_id"`
	SourceContainerID string   `json:"source_container_id"`
	CurrentImage      string   `json:"current_image"`
	CurrentImageID    string   `json:"current_image_id"`
	TargetImage       string   `json:"target_image"`
	TargetImageID     *string  `json:"target_image_id"`
	Available         bool     `json:"available"`
	RetainedMounts    []string `json:"retained_mounts"`
	Warning           string   `json:"warning"`
}

const ContainerUpgradeWarning = "The existing workspace and build cache mounts will be reattached. State in the container's writable layer, including manually installed packages, may be lost. A checkpoint does not preserve the whole environment. Busy workers finish their accepted work before the upgrade starts. You can cancel while it is pending."

type ContainerUpgradeRequest struct {
	Selection                    ContainerUpgradePreview `json:"selection"`
	AcknowledgeWritableLayerLoss bool                    `json:"acknowledge_writable_layer_loss"`
}
