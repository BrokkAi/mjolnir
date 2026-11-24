// Port of the tests of mj-worker-staging/src/worker_binary/process.rs
// (`mod probe_tests`).

package workerstaging

import (
	"fmt"
	"syscall"
	"testing"
)

// R8-2 (cli/048): a resume whose worker exited stored the whole probe as the
// session's error, 96 lines long. The error keeps one line: the reason the
// worker recorded, then the rest of the chain.
// Hard-won: 2fd45e3c62e5: a failed resume stored the 96-line worker probe instead of a one-line cause.
func TestAFailureCarryingAWorkerExitIsOneLineNamingTheWorkersReason(t *testing.T) {
	probe := WorkerProbe{
		Startup: &WorkerStartupRecord{Step: "acp-initialized", Steps: []WorkerStartupStep{}},
		Exit: &WorkerExitRecord{
			Reason: "select required ACP execution mode auto: Cannot set permission mode " +
				"to auto: auto mode unavailable for this model\n" +
				"ACP bridge stderr:\n[session/create] phase=register durationMs=1",
		},
		Pids: []uint32{},
	}
	// Rust: io::Error::from(ErrorKind::BrokenPipe), which displays as
	// "broken pipe".
	err := fmt.Errorf("%s: %w", probe.String(),
		fmt.Errorf("write relay history_requests request: %w", syscall.EPIPE))

	want := "the worker exited: select required ACP execution mode auto: Cannot set " +
		"permission mode to auto: auto mode unavailable for this model: write relay " +
		"history_requests request: broken pipe"
	if got := FailureLine(err); got != want {
		t.Fatalf("FailureLine = %q, want %q", got, want)
	}
}
