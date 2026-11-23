// Port of the `cfg(target_os = "linux")` test in mj-worker-bin/src/main.rs.

package workerbin

import (
	"encoding/json"
	"errors"
	"log/slog"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"syscall"
	"testing"

	"golang.org/x/sys/unix"

	"github.com/BrokkAi/mjolnir/go/internal/core"
	"github.com/BrokkAi/mjolnir/go/internal/worker"
)

// Hard-won: d7ca109e4: a panic hook writing to a full stderr must preserve the reserved record without aborting.
// /dev/full exists only on Linux; macOS has no equivalent full device, so this
// real-disk worker check is scoped to Linux.
func TestFullStderrKeepsWorkerDiagnosticsAliveAndPanicRecordsTheTail(t *testing.T) {
	const childRoot = "MJ_TEST_FULL_STDERR_ROOT"
	if root, ok := os.LookupEnv(childRoot); ok {
		full, err := os.OpenFile("/dev/full", os.O_WRONLY, 0)
		if err != nil {
			t.Fatal(err)
		}
		// This isolated child owns stderr, and the descriptor is open.
		if err := unix.Dup2(int(full.Fd()), unix.Stderr); err != nil {
			t.Fatal(err)
		}
		if _, err := os.Stderr.Write([]byte("ENOSPC probe\n")); !errors.Is(err, syscall.ENOSPC) {
			t.Fatalf("writing to /dev/full returned %v", err)
		}
		installWorkerLastWords(root)
		if err := installStderrLogging(); err != nil {
			t.Fatal(err)
		}
		if err := worker.Reserve(root); err != nil {
			t.Fatal(err)
		}
		for index := range 1000 {
			core.DiagnosticStderr("startup diagnostic %d", index)
			slog.Warn("still running with full stderr", "index", index)
		}
		dropped := core.StderrWriter().DroppedLines()
		if dropped == nil || dropped.Count != 2000 {
			t.Fatalf("dropped %+v", dropped)
		}
		if !strings.Contains(strings.Join(core.StderrWriter().RecentLog(), ""), "still running with full stderr") {
			t.Fatal("the recent log lost the warnings")
		}
		if err := os.WriteFile(filepath.Join(root, "survived"), []byte("alive"), 0o666); err != nil {
			t.Fatal(err)
		}
		panicked := func() (panicked bool) {
			defer func() { panicked = recover() != nil }()
			defer recordWorkerLastWords()
			panic("intentional full-stderr panic")
		}()
		if !panicked {
			t.Fatal("the panic was not raised again")
		}
		return
	}
	root := t.TempDir()
	executable, err := os.Executable()
	if err != nil {
		t.Fatal(err)
	}
	command := exec.Command(executable, "-test.run=^"+t.Name()+"$", "-test.count=1", "-test.v")
	command.Env = append(os.Environ(),
		childRoot+"="+root,
		"MJ_DATA_DIR="+filepath.Join(root, "data"),
		"RUST_BACKTRACE=1",
	)
	output, err := core.RunCapturingStdout(command)
	if err != nil {
		t.Fatal(err)
	}
	if !output.Status.Success() {
		t.Fatalf("%v: %s", output.Status, output.Stdout)
	}
	if _, err := os.Stat(filepath.Join(root, "survived")); err != nil {
		t.Fatal(err)
	}
	data, err := os.ReadFile(filepath.Join(root, core.WorkerExitFile))
	if err != nil {
		t.Fatal(err)
	}
	if len(data) != worker.ReservedExitRecordBytes {
		t.Fatalf("exit record is %d bytes", len(data))
	}
	var record struct {
		Reason    string          `json:"reason"`
		RecentLog json.RawMessage `json:"recent_log"`
	}
	if err := json.Unmarshal(data, &record); err != nil {
		t.Fatal(err)
	}
	if !strings.Contains(record.Reason, "intentional full-stderr panic") {
		t.Fatalf("reason %q", record.Reason)
	}
	diagnostics := record.Reason + string(record.RecentLog)
	if !strings.Contains(diagnostics, "core.PanicMessage") {
		t.Fatalf("the record carries no backtrace: %s", diagnostics)
	}
}
