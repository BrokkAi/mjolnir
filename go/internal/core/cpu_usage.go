// Port of mj-core/src/cpu_usage.rs.

// CPU accounting scopes, normalized to their machine.

package core

import (
	"encoding/json"
	"fmt"
)

// CpuSampleScope is the physical scope actually measured, including for
// legacy workers. The default is CpuSampleScopeProcessTree.
type CpuSampleScope string

const (
	CpuSampleScopeProcessTree CpuSampleScope = "process_tree"
	CpuSampleScopeContainer   CpuSampleScope = "container"
)

// UnmarshalJSON accepts only the scopes serde accepts.
func (s *CpuSampleScope) UnmarshalJSON(data []byte) error {
	var text string
	if err := json.Unmarshal(data, &text); err != nil {
		return err
	}
	switch scope := CpuSampleScope(text); scope {
	case CpuSampleScopeProcessTree, CpuSampleScopeContainer:
		*s = scope
		return nil
	}
	return fmt.Errorf("unknown variant `%s`, expected `process_tree` or `container`", text)
}

// CpuAccountingMode is the placement chosen by the controller; container
// samples share a physical scope. The default is
// CpuAccountingModeProcessTree.
type CpuAccountingMode string

const (
	CpuAccountingModeProcessTree CpuAccountingMode = "process_tree"
	CpuAccountingModeContainer   CpuAccountingMode = "container"
)

// UnmarshalJSON accepts only the modes serde accepts.
func (m *CpuAccountingMode) UnmarshalJSON(data []byte) error {
	var text string
	if err := json.Unmarshal(data, &text); err != nil {
		return err
	}
	switch mode := CpuAccountingMode(text); mode {
	case CpuAccountingModeProcessTree, CpuAccountingModeContainer:
		*m = mode
		return nil
	}
	return fmt.Errorf("unknown variant `%s`, expected `process_tree` or `container`", text)
}

// IsProcessTree reports whether this is the process-tree mode. The zero
// value is the default, so it counts too.
func (m CpuAccountingMode) IsProcessTree() bool {
	return m == CpuAccountingModeProcessTree || m == ""
}

// SessionCpuUsage is one CPU sample of a session.
type SessionCpuUsage struct {
	// Scope defaults to process_tree when absent.
	Scope CpuSampleScope `json:"scope"`
	// RecentPermille is the machine CPU share over the newest interval, in
	// tenths of a percent.
	RecentPermille uint16 `json:"recent_permille"`
	// HourlyPermille is the time-weighted share with a one-hour decay,
	// corrected for startup.
	HourlyPermille uint16 `json:"hourly_permille"`
	// HourlyCoveredSecs is the seconds covered by the average, capped at one
	// hour.
	HourlyCoveredSecs uint32 `json:"hourly_covered_secs"`
	OnlineCpus        uint32 `json:"online_cpus"`
}

// UnmarshalJSON applies serde's default for a missing scope.
func (u *SessionCpuUsage) UnmarshalJSON(data []byte) error {
	type plain SessionCpuUsage
	decoded := plain{Scope: CpuSampleScopeProcessTree}
	if err := json.Unmarshal(data, &decoded); err != nil {
		return err
	}
	*u = SessionCpuUsage(decoded)
	return nil
}

// MarshalJSON writes the default scope for a zero-valued one.
func (u SessionCpuUsage) MarshalJSON() ([]byte, error) {
	type plain SessionCpuUsage
	if u.Scope == "" {
		u.Scope = CpuSampleScopeProcessTree
	}
	return json.Marshal(plain(u))
}
