// Port of mj-core/src/placement.rs.
//
// Where this process runs, and therefore where its harness programs come
// from.
//
// The container launcher names the daemon container in DaemonContainerEnv
// for the daemon and for the mj commands it runs inside that container.
// Every decision that depends on running there asks this module instead of
// reading the variable itself.

package core

import (
	"os"
	"strings"
)

// DaemonContainerEnv is the environment variable naming the daemon
// container this process runs in. A host daemon and a host mj do not have it.
const DaemonContainerEnv = "MJ_DAEMON_CONTAINER"

// DaemonContainer is the daemon container this process runs in, or false on
// a host.
func DaemonContainer() (string, bool) {
	name, ok := os.LookupEnv(DaemonContainerEnv)
	if !ok || strings.TrimSpace(name) == "" {
		return "", false
	}
	return name, true
}

// LocalHarnessRuntime is where harness programs come from for work this
// machine runs itself: local sessions, the model-discovery probe, mj login
// and local quota checks.
//
// Mjolnir manages these installations on the host and in the daemon container.
func LocalHarnessRuntime() HarnessRuntimePolicy {
	return HarnessRuntimePolicyManaged
}
