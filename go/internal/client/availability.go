// Port of mj-client/src/availability.rs.

// Daemon-computed environment readiness; profile environments retain sources
// only.

package client

import (
	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// MachineRuntimeProbe is what probing one machine found.
type MachineRuntimeProbe struct {
	MachineID string `json:"machine_id"`
	// Machine is the machine as it was probed.
	Machine core.Machine   `json:"machine"`
	Finding MachineFinding `json:"finding"`
}

func (v MachineRuntimeProbe) MarshalJSON() ([]byte, error) {
	type plain MachineRuntimeProbe
	return daemonMarshalStruct(plain(v))
}

func (v *MachineRuntimeProbe) UnmarshalJSON(data []byte) error {
	type plain MachineRuntimeProbe
	return daemonDecodeStruct(data, (*plain)(v), false)
}

// MachineFinding is what a machine probe found. Externally tagged with the
// Rust variant names.
type MachineFinding interface {
	isMachineFinding()
}

// MachineFindingAnswered: the machine answered.
type MachineFindingAnswered struct {
	// Runtimes are the runtimes it can run, under the ids they would like.
	Runtimes []DaemonPair[string, core.TargetTemplate] `json:"runtimes"`
	// Rejected are the runtimes it cannot run, with why.
	Rejected []RejectedRuntime `json:"rejected"`
}

// MachineFindingUnreachable: the SSH check failed, so what it runs is
// unknown.
type MachineFindingUnreachable struct {
	Detail      string `json:"detail"`
	Remediation string `json:"remediation"`
	// AnsweredNothing is true only when the host answered nothing at all.
	// Older daemons omit this fact; keep their failures actionable.
	AnsweredNothing bool `json:"answered_nothing" serde:"default"`
}

func (MachineFindingAnswered) isMachineFinding()    {}
func (MachineFindingUnreachable) isMachineFinding() {}

func (v MachineFindingAnswered) MarshalJSON() ([]byte, error) {
	type plain MachineFindingAnswered
	return sessionExternalStruct("Answered", plain(v))
}

func (v MachineFindingUnreachable) MarshalJSON() ([]byte, error) {
	type plain MachineFindingUnreachable
	return sessionExternalStruct("Unreachable", plain(v))
}

// UnmarshalMachineFindingJSON decodes serde's externally tagged form.
func UnmarshalMachineFindingJSON(data []byte) (MachineFinding, error) {
	name, body, err := daemonSplitExternal(data)
	if err != nil {
		return nil, err
	}
	switch name {
	case "Answered":
		return sessionExternalVariant[MachineFindingAnswered, MachineFinding](body)
	case "Unreachable":
		return sessionExternalVariant[MachineFindingUnreachable, MachineFinding](body)
	}
	return nil, daemonUnknownVariant(name, []string{"Answered", "Unreachable"})
}

// LocalEngine is one of this computer's container engines, as a check found
// it.
type LocalEngine struct {
	// Missing is why it cannot run sessions, or nil when it can.
	Missing *string `json:"missing"`
	// Installed is whether its command is on this computer at all. A standard
	// runtime for an engine that was never installed is left out without a
	// word; one that is installed but not working is named.
	Installed bool `json:"installed"`
}

func (v *LocalEngine) UnmarshalJSON(data []byte) error {
	type plain LocalEngine
	return daemonDecodeStruct(data, (*plain)(v), false)
}

// AvailabilityChecked is what one availability check found and changed.
type AvailabilityChecked struct {
	// Profiles is every configured profile as it was checked, and why it is
	// missing (nil when it is not).
	Profiles map[string]DaemonPair[core.HarnessProfile, *string] `json:"profiles"`
	// Local is this computer's engines, by runtime kind (`podman`, `docker`,
	// `apple-container`).
	Local map[string]LocalEngine `json:"local"`
	// Machines is what each SSH and EC2 machine can run.
	Machines      []MachineRuntimeProbe `json:"machines"`
	AddedProfiles []string              `json:"added_profiles"`
	AddedRuntimes map[string][]string   `json:"added_runtimes"`
}

func (v AvailabilityChecked) MarshalJSON() ([]byte, error) {
	type plain AvailabilityChecked
	return daemonMarshalStruct(plain(v))
}

func (v *AvailabilityChecked) UnmarshalJSON(data []byte) error {
	type plain AvailabilityChecked
	return daemonDecodeStruct(data, (*plain)(v), false)
}
