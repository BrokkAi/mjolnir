// Port of the `#[cfg(all(test, windows))] mod windows_tests` in
// mj-worker-launch/src/local_profile_homes.rs.

package workerlaunch

import (
	"os"
	"slices"
	"strings"
	"testing"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// sysinfo fills command lines only when the refresh asks for them, and reads
// them from each process's memory. The sweep's guard depends on this
// process's own command line showing up whole.
func TestRunningProcessArgumentsIncludeThisProcess(t *testing.T) {
	own := strings.Join(os.Args, " ")
	running, ok := RunningProcessArguments(core.ProcessExecutor{})
	if !ok {
		t.Fatal("list running processes")
	}
	if !slices.Contains(running, own) {
		t.Fatalf("%q not in %d processes", own, len(running))
	}
}
