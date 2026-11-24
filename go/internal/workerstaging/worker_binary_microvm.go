// Port of mj-worker-staging/src/worker_binary/microvm.rs.

// The Nitro Sandbox host engine is this build's worker binary, run as root
// through `sudo -n` (`mj-worker microvm`). It reaches an EC2 host the way a
// worker reaches an `ssh_bare` host: through the shared worker store, linked
// into place with the same digest-checked, atomically promoted installer.

package workerstaging

import (
	"github.com/BrokkAi/mjolnir/go/internal/core"
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
	panic("unported: mj-worker-staging/src/worker_binary/microvm.rs ensure_microvm_engine")
}
