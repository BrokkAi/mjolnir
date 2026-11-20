// Port of mj-core/src/targets/capacity_report.rs.
//
// The per-host capacity readings the daemon serves and `mj hosts` prints.
//
// One row stands for one deployment host, or for one EC2 fleet. A host row
// reports how much CPU and RAM is in use; a fleet has no percentage of its
// own and reports the cores and memory it is running. Free space comes from
// the storage owner's verdicts, the same ones that refuse writes.

package core

import (
	"encoding/json"
	"fmt"
)

// CapacitySampleStaleAfterSeconds: a reading older than this stopped
// tracking the host. The poller samples every 30 seconds, so three missed
// rounds mean the number shown is no longer what the host is doing.
const CapacitySampleStaleAfterSeconds uint64 = 90

// CapacitySampleIsStale reports whether a reading sampled at sampledAt has
// stopped tracking its host. Every surface that marks capacity stale asks
// this one rule.
func CapacitySampleIsStale(sampledAt, nowEpochSeconds uint64) bool {
	var age uint64
	if nowEpochSeconds > sampledAt {
		age = nowEpochSeconds - sampledAt
	}
	return age > CapacitySampleStaleAfterSeconds
}

// CapacityStaleness says why a reading cannot be trusted, if it cannot: a
// probe that failed, or a sample that stopped refreshing. false means the
// reading is current.
func CapacityStaleness(hasReading bool, probeError *string, sampledAtEpochSeconds *uint64, nowEpochSeconds uint64) (string, bool) {
	if probeError != nil {
		// A probe that never produced a reading already says "unavailable";
		// only say more when stale numbers are still shown and could be
		// mistaken for current.
		if hasReading {
			return "stale: " + *probeError, true
		}
		return "", false
	}
	if sampledAtEpochSeconds == nil || !CapacitySampleIsStale(*sampledAtEpochSeconds, nowEpochSeconds) {
		return "", false
	}
	return "stale: sampled " + RefreshAge(nowEpochSeconds, *sampledAtEpochSeconds), true
}

// TargetCapacityState is everything the capacity service knows about one
// probe target. One value owns the last good reading, when it was taken, the
// latest failure and whether the fleet answered empty, so no reader can pair
// a fresh sample time with an older reading. Writers change it only through
// Apply.
type TargetCapacityState struct {
	// Usage is the last good reading. A failed probe leaves it in place.
	Usage *DeploymentCapacityUsage
	// SampledAtEpochSeconds is when Usage (or the empty-fleet answer) was taken.
	SampledAtEpochSeconds *uint64
	// ProbeError is set when the latest probe failed.
	ProbeError *string
	// OnDemand: the fleet answered and has no instances running. Cleared by a
	// failed probe: after a failure the fleet's size is unknown, not zero.
	OnDemand bool
	// AnsweredRefresh is the newest refresh request that a probe began after.
	// A result whose probe started after request n answers every request up
	// to n; one that started earlier, or a scheduled round, does not answer a
	// later request. A surface that nudges the poller waits for this to reach
	// the generation it was given.
	AnsweredRefresh uint64
}

// HasAnswered reports whether any probe has answered, successfully or not.
// Until then the target is loading, not idle.
func (s *TargetCapacityState) HasAnswered() bool {
	return s.SampledAtEpochSeconds != nil || s.ProbeError != nil
}

// FleetInstances is how many instances a fleet is running, when the last
// probe answered. false for a plain host, and for a fleet whose size is
// unknown because its probe failed without a reading to fall back on.
func (s *TargetCapacityState) FleetInstances(target *DeploymentCapacityTarget) (int, bool) {
	if target.Kind == DeploymentCapacityKindAwsFleet && (s.Usage != nil || s.OnDemand) {
		return len(target.Probes), true
	}
	return 0, false
}

// Apply folds one probe result into the state. probeErr non-nil is the Rust
// Err(String); otherwise usage is the Ok value (nil for an empty fleet).
func (s *TargetCapacityState) Apply(usage *DeploymentCapacityUsage, probeErr *string, sampledAtEpochSeconds, refreshGeneration uint64) {
	s.AnsweredRefresh = max(s.AnsweredRefresh, refreshGeneration)
	if probeErr != nil {
		s.OnDemand = false
		message := *probeErr
		s.ProbeError = &message
		return
	}
	s.OnDemand = usage == nil
	if usage == nil {
		s.Usage = nil
	} else {
		cloned := *usage
		s.Usage = &cloned
	}
	sampled := sampledAtEpochSeconds
	s.SampledAtEpochSeconds = &sampled
	s.ProbeError = nil
}

// HostFilesystemRow is one filesystem Mjolnir writes to on a host, with the
// storage verdict.
type HostFilesystemRow struct {
	// Host is the machine measured: the host, or one instance of a fleet.
	Host           string           `json:"host"`
	Mount          string           `json:"mount"`
	AvailableBytes uint64           `json:"available_bytes"`
	TotalBytes     uint64           `json:"total_bytes"`
	Condition      StorageCondition `json:"condition"`
}

// HostCapacityRow is one row of the Targets capacity table.
type HostCapacityRow struct {
	ID   string `json:"id"`
	Host string `json:"host"`
	// TargetIDs are the configured targets that deploy to this host or fleet.
	TargetIDs []string               `json:"target_ids"`
	Kind      DeploymentCapacityKind `json:"kind"`
	// FleetInstances is how many instances a fleet is running. A host is one
	// machine and has no count; its probes are alternative connections to
	// that machine.
	FleetInstances *int `json:"fleet_instances,omitempty"`
	// HasReading is false when no reading has arrived, or only an empty
	// fleet answered.
	HasReading            bool                `json:"has_reading"`
	CPUPercent            *uint8              `json:"cpu_percent"`
	MemoryUsedBytes       uint64              `json:"memory_used_bytes"`
	MemoryTotalBytes      uint64              `json:"memory_total_bytes"`
	LogicalCores          uint64              `json:"logical_cores"`
	Filesystems           []HostFilesystemRow `json:"filesystems"`
	SampledAtEpochSeconds *uint64             `json:"sampled_at_epoch_seconds"`
	ProbeError            *string             `json:"probe_error"`
	OnDemand              bool                `json:"on_demand"`
}

// MarshalJSON writes empty lists as [], as serde does.
func (r HostCapacityRow) MarshalJSON() ([]byte, error) {
	type plain HostCapacityRow
	if r.TargetIDs == nil {
		r.TargetIDs = []string{}
	}
	if r.Filesystems == nil {
		r.Filesystems = []HostFilesystemRow{}
	}
	return json.Marshal(plain(r))
}

// NewHostCapacityRow builds the row for target from its state and the
// storage views (Rust: HostCapacityRow::new).
func NewHostCapacityRow(target *DeploymentCapacityTarget, state *TargetCapacityState, storage []TargetStorageView) HostCapacityRow {
	panic("unported: mj-core/src/targets/capacity_report.rs HostCapacityRow::new")
}

// Matches reports whether name selects this row: its host, or a target it
// contains.
func (r *HostCapacityRow) Matches(name string) bool {
	if r.Host == name {
		return true
	}
	for _, id := range r.TargetIDs {
		if id == name {
			return true
		}
	}
	return false
}

// Staleness says why the row's reading cannot be trusted, if it cannot.
func (r *HostCapacityRow) Staleness(nowEpochSeconds uint64) (string, bool) {
	return CapacityStaleness(r.HasReading, r.ProbeError, r.SampledAtEpochSeconds, nowEpochSeconds)
}

// MemoryPercent is used memory as a whole percent, or false when the total is
// unknown.
func (r *HostCapacityRow) MemoryPercent() (uint8, bool) {
	panic("unported: mj-core/src/targets/capacity_report.rs HostCapacityRow::memory_percent")
}

// FleetMachinesLabel counts the fleet's VMs.
func (r *HostCapacityRow) FleetMachinesLabel() string {
	count := 0
	if r.FleetInstances != nil {
		count = *r.FleetInstances
	}
	return Counted(count, "VM", "VMs")
}

// CPUAndMemory are the CPU and RAM cells.
func (r *HostCapacityRow) CPUAndMemory() (cpu, memory string) {
	panic("unported: mj-core/src/targets/capacity_report.rs HostCapacityRow::cpu_and_memory")
}

// FilesystemPlace is where a filesystem is: its mount, qualified by machine
// for a fleet.
func (r *HostCapacityRow) FilesystemPlace(filesystem *HostFilesystemRow) string {
	if r.Kind == DeploymentCapacityKindAwsFleet {
		return fmt.Sprintf("%s %s", filesystem.Host, filesystem.Mount)
	}
	return filesystem.Mount
}

// StorageConditionFlag is the condition suffix after a filesystem's free
// space.
func StorageConditionFlag(condition StorageCondition) string {
	switch condition {
	case StorageConditionFull:
		return " full"
	case StorageConditionLow:
		return " low"
	default:
		return ""
	}
}

// RenderHostCapacity is `mj hosts` text: a table of hosts with CPU and RAM,
// then each host's filesystems and why its reading is stale, if it is.
func RenderHostCapacity(rows []HostCapacityRow, nowEpochSeconds uint64) string {
	panic("unported: mj-core/src/targets/capacity_report.rs render_host_capacity")
}
