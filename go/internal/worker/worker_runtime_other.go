// Port of the `cfg(not(unix))` parts of mj-worker/src/worker_runtime.rs.
// Their Unix counterparts are in worker_runtime_discovery.go and in the
// Unix runtime (worker_runtime_unix.go and its submodules), which must carry
// the same signatures.

//go:build !unix

package worker

import (
	"context"
	"errors"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// DiscoverProfileConfig needs a Unix worker host.
func DiscoverProfileConfig(ctx context.Context, spec core.ProfileProbeSpec) (core.ProfileConfig, error) {
	return core.ProfileConfig{}, errors.New("profile discovery requires a Unix worker host")
}

// LeadProcessGroup does nothing without Unix process groups.
func LeadProcessGroup() {}

// RunDaemon needs Unix.
func RunDaemon(ctx context.Context, root string, config core.WorkerLaunchConfig) error {
	return errors.New("target workers require Unix")
}

// RunDaemonOwned needs Unix.
func RunDaemonOwned(ctx context.Context, owner *WorkerRootOwner, config core.WorkerLaunchConfig) error {
	return errors.New("target workers require Unix")
}

// Proxy needs Unix.
func Proxy(ctx context.Context, root string) error {
	return errors.New("target workers require Unix")
}

// PrepareManagedHarness needs Unix.
func PrepareManagedHarness(ctx context.Context, config core.WorkerLaunchConfig) error {
	return errors.New("managed target harnesses require Unix")
}

// RunACPSupervisor needs Unix.
func RunACPSupervisor(ctx context.Context, spec AcpSupervisorSpec) error {
	return errors.New("ACP supervision requires Unix")
}
