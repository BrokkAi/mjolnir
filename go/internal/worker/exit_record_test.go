// Port of the tests of mj-worker/src/exit_record.rs.

package worker

import (
	"encoding/json"
	"os"
	"path/filepath"
	"strings"
	"testing"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// exitRecordReason decodes a record as Rust's Option<Record>: nil for a
// reservation, the reason otherwise.
func exitRecordReason(t *testing.T, data []byte) *string {
	t.Helper()
	var record *struct {
		Reason *string `json:"reason"`
	}
	if err := json.Unmarshal(data, &record); err != nil {
		t.Fatal(err)
	}
	if record == nil {
		return nil
	}
	if record.Reason == nil {
		t.Fatal("missing field `reason`")
	}
	return record.Reason
}

// Hard-won: 540c920: A full target disk left workers unreachable without persisting the reason into their reserved exit record.
func TestAReservationReadsAsNoRecordAndTakesTheReasonInPlace(t *testing.T) {
	root := t.TempDir()
	if err := Reserve(root); err != nil {
		t.Fatal(err)
	}
	path := filepath.Join(root, core.WorkerExitFile)
	reserved, err := os.ReadFile(path)
	if err != nil {
		t.Fatal(err)
	}
	if exitRecordReason(t, reserved) != nil {
		t.Fatal("a reservation must read as no exit record")
	}

	inode, hasInode := exitRecordInode(t, path)
	failure := core.WorkerHarnessStartupFailure{Message: "could not open the native ACP session"}
	if err := WriteWithRecentLog(root,
		"relay coordinator failed: No space left on device (os error 28)", nil, &failure, nil); err != nil {
		t.Fatal(err)
	}
	data, err := os.ReadFile(path)
	if err != nil {
		t.Fatal(err)
	}
	var marked struct {
		HarnessStartupFailure struct {
			Message string `json:"message"`
		} `json:"harness_startup_failure"`
	}
	if err := json.Unmarshal(data, &marked); err != nil {
		t.Fatal(err)
	}
	if marked.HarnessStartupFailure.Message != failure.Message {
		t.Fatalf("marker = %q", marked.HarnessStartupFailure.Message)
	}
	if len(data) != ReservedExitRecordBytes {
		t.Fatalf("length = %d", len(data))
	}
	if reason := exitRecordReason(t, data); reason == nil || !strings.Contains(*reason, "No space left on device") {
		t.Fatalf("reason = %v", reason)
	}
	if after, _ := exitRecordInode(t, path); hasInode && after != inode {
		t.Fatal("the record must be written in place, not as a new file")
	}
	if err := Reserve(root); err != nil {
		t.Fatal(err)
	}
	nextBoot, err := os.ReadFile(path)
	if err != nil {
		t.Fatal(err)
	}
	if exitRecordReason(t, nextBoot) != nil {
		t.Fatal("the next boot must clear the marker")
	}
}

func TestALongReasonIsShortenedToFitTheReservation(t *testing.T) {
	root := t.TempDir()
	if err := Reserve(root); err != nil {
		t.Fatal(err)
	}
	reason := "journal write failed: " + strings.Repeat("é", 20_000)
	tail := []string{strings.Repeat("\x00\"é", 5000) + "newest diagnostic\n"}
	if err := WriteWithRecentLog(root, reason, nil, nil, tail); err != nil {
		t.Fatal(err)
	}
	data, err := os.ReadFile(filepath.Join(root, core.WorkerExitFile))
	if err != nil {
		t.Fatal(err)
	}
	if len(data) != ReservedExitRecordBytes {
		t.Fatalf("length = %d", len(data))
	}
	recorded := exitRecordReason(t, data)
	if recorded == nil || !strings.HasPrefix(*recorded, "journal write failed: ") || !strings.HasSuffix(*recorded, "…") {
		t.Fatalf("reason = %v", recorded)
	}
	var value struct {
		RecentLog []string `json:"recent_log"`
	}
	if err := json.Unmarshal(data, &value); err != nil {
		t.Fatal(err)
	}
	if len(value.RecentLog) == 0 || !strings.HasSuffix(value.RecentLog[0], "newest diagnostic\n") {
		t.Fatalf("recent log = %q", value.RecentLog)
	}
}
