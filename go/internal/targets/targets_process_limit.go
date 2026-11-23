// Port of mj-targets/src/targets/process_limit.rs.
//
// Recognising a target that has run out of process slots, and saying so.
//
// A session container is created with a pids limit (see
// containerPidsLimit), and every thread counts against it. Past the limit
// every `fork` and every thread spawn fails with `EAGAIN`, which reaches
// Mjolnir as whatever the failing program printed: "os error 11",
// "Resource temporarily unavailable", a shell's "Cannot fork", or Tokio's
// "can't spawn worker thread". A sub-agent start that fails that way has not
// failed for anything in its task, and its parent can fix it by closing
// children it no longer needs (#1161), so the parent is told exactly that.

package targets

import (
	"time"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// PidsUsageReadTimeout is how long reading the container's pid counts may
// take. The read runs in a container that may be full, where it can hang or
// fail; it only adds detail.
const PidsUsageReadTimeout = 5 * time.Second

// processExhaustionMarkers is what the failing programs print when `fork` or
// a thread spawn returns `EAGAIN`. Matched without regard to case. Rust
// prints the errno in parentheses, and the closing one keeps
// `(os error 111)`, a refused connection, from matching.
var processExhaustionMarkers = [5]string{
	"(os error 11)",
	"resource temporarily unavailable",
	"cannot fork",
	"can't fork",
	"can't spawn worker thread",
}

// ShowsProcessExhaustion reports whether an error's chain shows that its
// target could not create another process or thread.
func ShowsProcessExhaustion(err error) bool {
	panic("unported: mj-targets/src/targets/process_limit.rs shows_process_exhaustion")
}

// PidsUsage is the pid counts of a container's cgroup: `pids.current`, and
// `pids.max`, which is nil when the cgroup has no limit (the file says
// `max`).
type PidsUsage struct {
	Current uint64
	Max     *uint64
}

// pidsUsageScript prints `pids.current` and `pids.max` of the cgroup the
// command runs in, under cgroup v2 or, failing that, v1.
const pidsUsageScript = "cat /sys/fs/cgroup/pids.current /sys/fs/cgroup/pids.max 2>/dev/null " +
	"|| cat /sys/fs/cgroup/pids/pids.current /sys/fs/cgroup/pids/pids.max"

// IsContainer reports whether a target runs its sessions in a container,
// which is what has a pids limit of its own.
func IsContainer(locator *core.TargetsTargetLocator) bool {
	switch locator.Kind {
	case core.TargetsTargetLocatorLocalPodman,
		core.TargetsTargetLocatorLocalDocker,
		core.TargetsTargetLocatorAppleContainer,
		core.TargetsTargetLocatorSshPodman,
		core.TargetsTargetLocatorSshDocker,
		core.TargetsTargetLocatorSshAppleContainer:
		return true
	default:
		return false
	}
}

// ReadPidsUsage reads the pid counts of the container locator names, from
// inside it.
func ReadPidsUsage(executor core.CommandExecutor, locator *core.TargetsTargetLocator, sessionID string) (PidsUsage, error) {
	panic("unported: mj-targets/src/targets/process_limit.rs read_pids_usage")
}

func parsePidsUsage(text string) (PidsUsage, error) {
	panic("unported: mj-targets/src/targets/process_limit.rs parse_pids_usage")
}

// ProcessExhaustionMessage is the message a parent model reads when a
// sub-agent could not be started because its target ran out of process
// slots. usage and usageErr are the result of ReadPidsUsage for a container;
// both are nil for a target that is not one (Rust: usage None). The original
// error follows, so nothing it said is lost.
func ProcessExhaustionMessage(usage *PidsUsage, usageErr error, err error) string {
	panic("unported: mj-targets/src/targets/process_limit.rs process_exhaustion_message")
}
