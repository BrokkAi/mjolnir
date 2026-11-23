// Port of the startup_record_tests in mj-worker/src/worker_runtime.rs.

package worker

import (
	"encoding/json"
	"os"
	"path/filepath"
	"slices"
	"testing"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// TestTheStartupRecordNamesTheLatestStepAndKeepsTheOrder: the controller
// reads this file to decide whether a worker that has not answered yet is
// still making progress, so the latest step and the order of the steps are
// the two things that must hold.
func TestTheStartupRecordNamesTheLatestStepAndKeepsTheOrder(t *testing.T) {
	root := t.TempDir()
	RecordStartupStep(root, "start")
	RecordStartupStep(root, "login-environment")
	RecordStartupStep(root, "bind-socket")

	body, err := os.ReadFile(filepath.Join(root, core.WorkerStartupFile))
	if err != nil {
		t.Fatal(err)
	}
	var record map[string]any
	if err := json.Unmarshal(body, &record); err != nil {
		t.Fatal(err)
	}
	if record["step"] != "bind-socket" {
		t.Fatalf("step = %v", record["step"])
	}
	if record["pid"] != float64(os.Getpid()) {
		t.Fatalf("pid = %v", record["pid"])
	}
	var steps []string
	for _, step := range record["steps"].([]any) {
		steps = append(steps, step.(map[string]any)["step"].(string))
	}
	if !slices.Equal(steps, []string{"start", "login-environment", "bind-socket"}) {
		t.Fatalf("steps = %v", steps)
	}
	at, _ := record["steps"].([]any)[0].(map[string]any)["at"].(string)
	if at == "" {
		t.Fatalf("every step carries when it happened: %s", body)
	}
}

// TestRecordingAStepWithoutARootIsSilent: a breadcrumb must never be the
// reason a worker fails to start, so a root that is not there is silently
// skipped.
func TestRecordingAStepWithoutARootIsSilent(t *testing.T) {
	RecordStartupStep("/definitely/not/a/worker/root", "start")
}
