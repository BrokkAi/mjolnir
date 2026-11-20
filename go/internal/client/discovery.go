// Port of mj-client/src/discovery.rs.

// Settings discovery from the machine owning configuration and credentials.

package client

import (
	"encoding/json"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// DetectScope is which section of the configuration a detection run fills
// in. Each scope runs only the probes that section needs. Serialized with
// the Rust variant names.
type DetectScope string

const (
	DetectScopeProfiles DetectScope = "Profiles"
	DetectScopeRuntimes DetectScope = "Runtimes"
)

func (s *DetectScope) UnmarshalText(text []byte) error {
	switch value := DetectScope(text); value {
	case DetectScopeProfiles, DetectScopeRuntimes:
		*s = value
		return nil
	}
	return daemonUnknownVariant(string(text), []string{"Profiles", "Runtimes"})
}

// SetupDetection is what one detection run found: entries for the scope it
// was asked about, and the runtimes it turned down. A runtime detection also
// reports what each machine in the draft can run.
type SetupDetection struct {
	Scope DetectScope `json:"scope"`
	// Config is decoded with environment sources only (sourceConfig).
	Config           core.Config           `json:"config"`
	ProfileReadiness map[string]*string    `json:"profile_readiness"`
	RejectedRuntimes []RejectedRuntime     `json:"rejected_runtimes"`
	Machines         []MachineRuntimeProbe `json:"machines"`
}

func (v SetupDetection) MarshalJSON() ([]byte, error) {
	type plain SetupDetection
	return daemonMarshalStruct(plain(v))
}

// UnmarshalJSON reads Config with environment sources only, as Rust's
// source_config does, and every other field as serde does.
func (v *SetupDetection) UnmarshalJSON(data []byte) error {
	var wire struct {
		Scope            DetectScope           `json:"scope"`
		Config           json.RawMessage       `json:"config"`
		ProfileReadiness map[string]*string    `json:"profile_readiness"`
		RejectedRuntimes []RejectedRuntime     `json:"rejected_runtimes"`
		Machines         []MachineRuntimeProbe `json:"machines"`
	}
	if err := daemonDecodeStruct(data, &wire, false); err != nil {
		return err
	}
	config, err := sourceConfig(wire.Config)
	if err != nil {
		return err
	}
	*v = SetupDetection{
		Scope:            wire.Scope,
		Config:           config,
		ProfileReadiness: wire.ProfileReadiness,
		RejectedRuntimes: wire.RejectedRuntimes,
		Machines:         wire.Machines,
	}
	return nil
}

// sourceConfig decodes a configuration projection without reading
// credentials or secret files.
func sourceConfig(data json.RawMessage) (core.Config, error) {
	var config core.Config
	err := core.WithEnvironmentSourcesOnly(func() error {
		return json.Unmarshal(data, &config)
	})
	return config, err
}
