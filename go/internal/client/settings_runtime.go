// Port of mj-client/src/settings/runtime.rs.

package client

// RejectedRuntime is a container runtime that detection found but cannot
// use, carrying the reason so the Settings notice can name it.
type RejectedRuntime struct {
	// Kind is the runtime kind as a runtime stores it (`bare`, `podman`,
	// `docker`, `apple-container`).
	Kind        string  `json:"kind"`
	Label       string  `json:"label"`
	Detail      string  `json:"detail"`
	Remediation *string `json:"remediation"`
}

func (r *RejectedRuntime) UnmarshalJSON(data []byte) error {
	type plain RejectedRuntime
	return feedDecodeStruct(data, (*plain)(r), false, "kind", "label", "detail")
}
