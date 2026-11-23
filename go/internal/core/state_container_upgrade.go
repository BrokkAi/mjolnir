// Port of mj-core/src/state/container_upgrade.rs.

package core

import "encoding/json"

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
	// Settings is the container settings change this upgrade applies, if
	// any.
	Settings *ContainerSettingsChange `json:"settings,omitempty"`
}

type ContainerSettingsChange struct {
	ResourceAllocation SessionResourceAllocation `json:"resource_allocation"`
	Mounts             []AdditionalMount         `json:"mounts"`
	MountHistory       *[]string                 `json:"mount_history,omitempty"`
}

// MarshalJSON writes an absent mount list as [], as serde writes a Vec.
func (c ContainerSettingsChange) MarshalJSON() ([]byte, error) {
	type plain ContainerSettingsChange
	if c.Mounts == nil {
		c.Mounts = []AdditionalMount{}
	}
	return json.Marshal(plain(c))
}

func (c *ContainerSettingsChange) UnmarshalJSON(data []byte) error {
	if err := requireFields(data, "mounts"); err != nil {
		return err
	}
	type plain ContainerSettingsChange
	aux := struct {
		*plain
		ResourceAllocation stateUnionJSON[SessionResourceAllocation] `json:"resource_allocation"`
	}{plain: (*plain)(c)}
	if err := json.Unmarshal(data, &aux); err != nil {
		return err
	}
	c.ResourceAllocation = aux.ResourceAllocation.V
	return nil
}

const ContainerUpgradeWarning = "The existing workspace and build cache mounts will be reattached. State in the container's writable layer, including manually installed packages, may be lost. A checkpoint does not preserve the whole environment. Busy workers finish their accepted work before the upgrade starts. You can cancel while it is pending."

type ContainerUpgradeRequest struct {
	Selection                    ContainerUpgradePreview `json:"selection"`
	AcknowledgeWritableLayerLoss bool                    `json:"acknowledge_writable_layer_loss"`
}
