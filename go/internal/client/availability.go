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

// AvailabilityVerdictKind is AvailabilityVerdict's variant. The zero value
// is Unknown, Rust's default.
type AvailabilityVerdictKind uint8

const (
	AvailabilityVerdictUnknown AvailabilityVerdictKind = iota
	AvailabilityVerdictAvailable
	AvailabilityVerdictMissing
)

// AvailabilityVerdict applies to the entry's current configuration.
// Internally tagged by "status" in snake_case: {"status":"available"},
// {"status":"missing","reason":...}, {"status":"unknown"}.
type AvailabilityVerdict struct {
	Kind AvailabilityVerdictKind
	// Reason is why the entry is missing, when Kind is
	// AvailabilityVerdictMissing.
	Reason string
}

// MissingReason is why the entry is missing, false unless it is.
func (v AvailabilityVerdict) MissingReason() (string, bool) {
	if v.Kind == AvailabilityVerdictMissing {
		return v.Reason, true
	}
	return "", false
}

func (v AvailabilityVerdict) MarshalJSON() ([]byte, error) {
	switch v.Kind {
	case AvailabilityVerdictAvailable:
		return daemonInternalJSON("status", "available", nil)
	case AvailabilityVerdictMissing:
		return daemonInternalJSON("status", "missing", struct {
			Reason string `json:"reason"`
		}{v.Reason})
	}
	return daemonInternalJSON("status", "unknown", nil)
}

func (v *AvailabilityVerdict) UnmarshalJSON(data []byte) error {
	name, err := daemonSplitInternal(data, "status")
	if err != nil {
		return err
	}
	switch name {
	case "available":
		*v = AvailabilityVerdict{Kind: AvailabilityVerdictAvailable}
		return nil
	case "missing":
		var fields struct {
			Reason string `json:"reason"`
		}
		if err := daemonDecodeStruct(data, &fields, false); err != nil {
			return err
		}
		*v = AvailabilityVerdict{Kind: AvailabilityVerdictMissing, Reason: fields.Reason}
		return nil
	case "unknown":
		*v = AvailabilityVerdict{Kind: AvailabilityVerdictUnknown}
		return nil
	}
	return daemonUnknownVariant(name, []string{"available", "missing", "unknown"})
}

// AvailabilityNote is one line under a target picker. A machine that did not
// answer is muted and can name several runtimes in one line.
type AvailabilityNote struct {
	Text    string `json:"text"`
	Full    string `json:"full"`
	Routine bool   `json:"routine"`
}

func (v *AvailabilityNote) UnmarshalJSON(data []byte) error {
	type plain AvailabilityNote
	return daemonDecodeStruct(data, (*plain)(v), false)
}

type RuntimeAvailability struct {
	Profiles               map[string]AvailabilityVerdict `json:"profiles"`
	Targets                map[string]AvailabilityVerdict `json:"targets"`
	TargetNotes            []AvailabilityNote             `json:"target_notes"`
	MachinesWithoutRuntime map[string]string              `json:"machines_without_runtime"`
	// Browser is the explanations prepared for browser and API clients in
	// the daemon; never serialized.
	Browser BrowserAvailabilityReasons `json:"-"`
	// UninstalledEngines is the local engines whose command is absent
	// regardless of the last probe.
	UninstalledEngines sortedStringSet `json:"uninstalled_engines"`
}

func (v RuntimeAvailability) MarshalJSON() ([]byte, error) {
	type plain RuntimeAvailability
	value := plain(v)
	if value.UninstalledEngines == nil {
		value.UninstalledEngines = sortedStringSet{}
	}
	return daemonMarshalStruct(value)
}

func (v *RuntimeAvailability) UnmarshalJSON(data []byte) error {
	type plain RuntimeAvailability
	return daemonDecodeStruct(data, (*plain)(v), false)
}

// BrowserAvailabilityReasons is the public wording for the same missing
// verdicts. Only in-process projection code needs both versions. The
// terminal feed carries its own wording.
type BrowserAvailabilityReasons struct {
	Profiles    map[string]string
	Targets     map[string]string
	TargetNotes []AvailabilityNote
}
