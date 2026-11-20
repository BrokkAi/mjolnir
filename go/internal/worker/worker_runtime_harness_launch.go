// Port of mj-worker/src/worker_runtime/harness_launch.rs.

// Select the concrete executable before starting a harness.
//
// Rust re-exports core's find_command here as `harness_launch::find_command`;
// Go callers use core.FindCommand directly.

//go:build unix

package worker

import (
	"bytes"
	"context"
	"errors"
	"fmt"
	"io"
	"io/fs"
	"log/slog"
	"os"
	"slices"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// PreparedHarnessLaunch keeps this preparation alive until the supervisor
// holds its installation lease. Release is Rust's Drop of the leases.
type PreparedHarnessLaunch struct {
	Spec        AcpSupervisorSpec
	environment map[string]string
	managed     *core.ManagedHarness
	managedNode *core.ManagedNodeRuntime
}

// Release drops the installation leases this preparation holds.
func (l *PreparedHarnessLaunch) Release() {
	if l.managed != nil {
		l.managed.Release()
	}
	if l.managedNode != nil {
		l.managedNode.Release()
	}
}

// PrepareHarnessLaunch resolves the login environment, the managed Node.js
// runtime, the build caches and the harness executable for a bridge launch.
func PrepareHarnessLaunch(ctx context.Context, harness core.HarnessKind, policy core.HarnessRuntimePolicy, executionPolicy core.ExecutionPolicy, spec AcpSupervisorSpec) (*PreparedHarnessLaunch, error) {
	panic("unported: mj-worker/src/worker_runtime/harness_launch.rs prepare_harness_launch")
}

// freezeNodeEntrypoints keeps Mjolnir's JavaScript entrypoints independent
// of project-selected Node shims. Provider launch APIs accept an executable
// path, so those get a private launcher rather than depending on the
// bridge's PATH again.
func freezeNodeEntrypoints(ctx context.Context, root string, launch *PreparedHarnessLaunch) error {
	panic("unported: mj-worker/src/worker_runtime/harness_launch.rs freeze_node_entrypoints")
}

func usesEnvNode(ctx context.Context, path string) (bool, error) {
	file, err := os.Open(path)
	if errors.Is(err, fs.ErrNotExist) {
		return false, nil
	}
	if err != nil {
		return false, fmt.Errorf("read runtime entrypoint %s: %w", path, err)
	}
	defer file.Close()
	// Read just the interpreter prefix, not the JavaScript bundle or ELF body.
	prefix, err := io.ReadAll(io.LimitReader(file, int64(len("#!/usr/bin/env node\r\n"))))
	if err != nil {
		return false, err
	}
	return bytes.HasPrefix(prefix, []byte("#!/usr/bin/env node\n")) ||
		bytes.HasPrefix(prefix, []byte("#!/usr/bin/env node\r\n")), nil
}

// needsNodeRuntime: DSH can launch its own native binary. Its npm fallback
// is the only branch that needs Node.js. Other Node-using harnesses always
// need the runtime, even when their adapter is already installed.
func needsNodeRuntime(ctx context.Context, harness core.HarnessKind, environment map[string]string) (bool, error) {
	panic("unported: mj-worker/src/worker_runtime/harness_launch.rs needs_node_runtime")
}

// linkBuildCacheConfiguration: reviewer homes can be created after
// provisioning. Link their mbx lookup to machine policy too, without
// changing XDG_CONFIG_HOME for any other program.
func linkBuildCacheConfiguration(ctx context.Context, environment map[string]string) error {
	panic("unported: mj-worker/src/worker_runtime/harness_launch.rs link_build_cache_configuration")
}

// removeStaleConfigurationLink: a policy that existed when the home was
// prepared can disappear, leaving a link to nothing. Remove only Mjolnir's
// own link; a real file belongs to the user.
func removeStaleConfigurationLink(destination string, source string) {
	if target, err := os.Readlink(destination); err != nil || target != source {
		return
	}
	if err := os.Remove(destination); err != nil {
		slog.Warn("could not remove a stale shared mbx configuration link", "configuration", destination, "error", err)
	}
}

func resolveCommand(command string, environment map[string]string) (string, error) {
	found, ok, err := core.FindCommand(command, environment)
	if err != nil {
		return "", err
	}
	if !ok {
		return "", errors.New("runtime command is not executable on the selected PATH")
	}
	return found, nil
}

// primarySupervisorSpec: only a supported primary may enforce the
// repository's review gate.
func primarySupervisorSpec(config *core.WorkerLaunchConfig) AcpSupervisorSpec {
	spec := AcpSupervisorSpecFrom(config)
	delete(spec.Environment, "MJ_REVIEW_GATE")
	if config.ReviewGate() {
		spec.ExcludedEnvironment = slices.DeleteFunc(spec.ExcludedEnvironment, func(name string) bool { return name == "MJ_REVIEW_GATE" })
		spec.Environment["MJ_REVIEW_GATE"] = "1"
	} else if !slices.Contains(spec.ExcludedEnvironment, "MJ_REVIEW_GATE") {
		spec.ExcludedEnvironment = append(spec.ExcludedEnvironment, "MJ_REVIEW_GATE")
	}
	return spec
}

// prepareReviewGate records each repository's session start and, for a
// gated primary, installs the before-push hook; it returns notices for the
// user.
func prepareReviewGate(launch *core.WorkerLaunchConfig) ([]string, error) {
	panic("unported: mj-worker/src/worker_runtime/harness_launch.rs prepare_review_gate")
}
