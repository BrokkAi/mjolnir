// Port of mj-worker-staging/src/worker_binary/microvm.rs.

// The Nitro Sandbox host engine is this build's worker binary, run as root
// through `sudo -n` (`mj-worker microvm`). It reaches an EC2 host the way a
// worker reaches an `ssh_bare` host: through the shared worker store, linked
// into place with the same digest-checked, atomically promoted installer.

package workerstaging

import (
	"errors"
	"fmt"
	"slices"

	"github.com/BrokkAi/mjolnir/go/internal/core"
	"github.com/BrokkAi/mjolnir/go/internal/targets"
)

// engineOwner names the host-scoped installation in store gates and logs.
// It is not a session: the engine outlives every session on the host.
const engineOwner = "microvm-engine"

// EnsureMicrovmEngine makes sure the host runs this build's engine,
// installing it when the digest differs.
//
// Every engine command goes through core.EngineArgv, which names the path
// installed here, so this runs before the first one: at launch, before the
// image refresher pulls to the host, and by doctor. Concurrent callers
// converge on one installation because the store publishes atomically and
// promotion is a rename.
func EnsureMicrovmEngine(ssh *core.SshTarget, executor core.CommandExecutor) error {
	// Only the store and the platform probe use this locator, and they take
	// its host and the shape of its workspace; nothing is created there.
	hostSSH := *ssh
	hostSSH.SSHArgs = slices.Clone(ssh.SSHArgs)
	host := core.TargetsTargetLocator{
		Kind:      core.TargetsTargetLocatorSshBare,
		SSH:       &hostSSH,
		Workspace: ".local/share/hel/workspaces/" + engineOwner,
	}
	source, err := WorkerBinaryFor(&host, executor)
	if err != nil {
		return fmt.Errorf("find the worker binary for the Nitro sandbox host: %w", err)
	}
	digest, err := core.WorkerExecutableDigest(source)
	if err != nil {
		return err
	}
	destination := core.MicrovmEngineWorker
	verify := InstalledFileDigestCommand(&host, destination, "verify the host engine")
	if targets.InstalledDigestMatches(executor, &verify, digest) {
		return nil
	}
	directory, ok := pathParent(destination)
	if !ok {
		return errors.New("the engine path has no directory")
	}
	if _, err := targets.ExecuteChecked(executor,
		core.LocatorCommand(core.SshTrafficClassControl, &host, []string{"mkdir", "-p", directory}).
			WithPurpose("create the Nitro sandbox engine directory")); err != nil {
		return err
	}
	if err := InstallWorkerBinaryAt(executor, &host, engineOwner, WorkerInstallHost{Path: source}, destination, nil); err != nil {
		return fmt.Errorf("install the Nitro sandbox engine on the host: %w", err)
	}
	return nil
}
