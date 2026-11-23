// Port of the tests in mj-worker-bin/src/main.rs.

package workerbin

import (
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"os"
	"path/filepath"
	"strings"
	"testing"
	"time"

	"github.com/BrokkAi/mjolnir/go/internal/checkpoint"
	"github.com/BrokkAi/mjolnir/go/internal/core"
)

func init() { core.EnableTestFixtures() }

func TestMemoryMcpIgnoresTheLegacyRootArgument(t *testing.T) {
	parsed, err := parseCli([]string{
		"hel",
		"worker",
		"memory-mcp",
		"--root",
		"/profile/projects/session/memory",
		"--history-socket",
		"/worker/control.sock",
	})
	if err != nil {
		t.Fatal(err)
	}
	worker, ok := parsed.command.(commandWorker)
	if !ok {
		t.Fatalf("parsed %#v", parsed.command)
	}
	memory, ok := worker.command.(workerCommandMemoryMcp)
	if !ok || memory.historySocket == nil || *memory.historySocket != "/worker/control.sock" {
		t.Fatalf("parsed %#v", worker.command)
	}
}

// A precondition failure leaves its reason on the process, not a generic
// error: the daemon maps this exit code to a 409 carrying that reason.
// Hard-won: 5d4d3a8: Caller-fixable export refusals were returned as server errors indistinguishable from broken git.
func TestAnExportPreconditionFailureCarriesTheRefusalExitCode(t *testing.T) {
	root := t.TempDir()
	_, readErr := checkpoint.ReadSessionFile(root, "../secret.txt")
	if readErr == nil {
		t.Fatal("a path leaving the workspace was read")
	}
	var refusal exportRefused
	if !errors.As(exportError(readErr), &refusal) {
		t.Fatalf("a path leaving the workspace is a refusal: %v", readErr)
	}
	if !strings.Contains(refusal.Error(), "..") {
		t.Fatalf("the refusal names the reason: %s", refusal)
	}
	if checkpoint.ExportRefusedExitCode != 3 {
		t.Fatalf("ExportRefusedExitCode = %d", checkpoint.ExportRefusedExitCode)
	}

	var missingRemote exportRefused
	if !errors.As(pushError(checkpoint.PushBranchErrorNoRemote{}), &missingRemote) ||
		missingRemote.Error() != "no push remote configured" {
		t.Fatalf("missing remote = %v", pushError(checkpoint.PushBranchErrorNoRemote{}))
	}

	// A push that actually ran and failed is not a refusal; it stays an
	// ordinary error so the daemon reports it as a failure.
	failed := pushError(checkpoint.PushBranchErrorFailed{Err: errors.New("git exploded")})
	if errors.As(failed, new(exportRefused)) {
		t.Fatalf("a failed push is a refusal: %v", failed)
	}
}

// runSelf runs this test binary's named test as a child with stdin, the way
// the Rust tests re-run their own executable.
func runSelf(t *testing.T, name string, stdin []byte, environment map[string]string) core.CommandOutput {
	t.Helper()
	executable, err := os.Executable()
	if err != nil {
		t.Fatal(err)
	}
	command := core.NewCommandSpec(executable, []string{"-test.run=^" + name + "$", "-test.count=1"}).WithSensitiveStdin(stdin)
	command.Env = environment
	output, err := core.CancellableProcessExecutorWithTimeout(30 * time.Second).Execute(&command)
	if err != nil {
		t.Fatal(err)
	}
	return output
}

// Hard-won: #1543: the receiving worker also capped fetched artifacts at 16 MiB.
func TestFetchStorageAcceptsLargeFilesAndRefusesTruncatedInput(t *testing.T) {
	const childRoot = "MJ_TEST_FETCH_STORAGE_ROOT"
	const source = "/workspace/λ binary.dat"
	const session = "source-session"
	const length = 17 * 1024 * 1024
	if _, ok := os.LookupEnv(childRoot); ok {
		err := runCommand(context.Background(), commandWorker{workerArgs{command: workerCommandStoreFetch{
			session:   session,
			source:    source,
			directory: false,
			length:    length,
		}}}, nil)
		if err != nil {
			t.Fatal(err)
		}
		if err := writeStdout([]byte("\n")); err != nil {
			t.Fatal(err)
		}
		return
	}
	root := t.TempDir()
	data := make([]byte, length)
	for index := range data {
		data[index] = byte(index % 251)
	}
	environment := map[string]string{}
	for _, key := range []string{childRoot, "TMPDIR", "TEMP", "TMP", "MJ_DATA_DIR"} {
		environment[key] = root
	}
	output := runSelf(t, t.Name(), data, environment)
	if output.Status != 0 {
		t.Fatalf("status %d: %s%s", output.Status, output.Stdout, output.Stderr)
	}
	var saved checkpoint.SavedFetch
	found := false
	for _, line := range strings.Split(string(output.Stdout), "\n") {
		if json.Unmarshal([]byte(line), &saved) == nil {
			found = true
			break
		}
	}
	if !found {
		t.Fatalf("worker returns saved-file metadata: %s", output.Stdout)
	}
	if !strings.HasPrefix(saved.Path, filepath.Join(root, "mjolnir", session)+string(filepath.Separator)) {
		t.Fatalf("saved at %s", saved.Path)
	}
	if saved.Source != source || saved.Bytes != length {
		t.Fatalf("saved %+v", saved)
	}
	if stored, err := os.ReadFile(saved.Path); err != nil || !bytes.Equal(stored, data) {
		t.Fatalf("stored file differs: %v", err)
	}
	output = runSelf(t, t.Name(), make([]byte, 128*1024), environment)
	if output.Status == 0 {
		t.Fatal("truncated input was accepted")
	}
	if !strings.Contains(string(output.Stdout), "incomplete fetch transfer") &&
		!strings.Contains(string(output.Stderr), "incomplete fetch transfer") {
		t.Fatalf("stdout %s stderr %s", output.Stdout, output.Stderr)
	}
	if stored, err := os.ReadFile(saved.Path); err != nil || !bytes.Equal(stored, data) {
		t.Fatalf("a truncated transfer replaced the stored file: %v", err)
	}
}

func TestFileInjectionStreamsLargeStdinWhileDrainingWorkerOutput(t *testing.T) {
	const childRoot = "MJ_TEST_FILE_INPUT_ROOT"
	if root, ok := os.LookupEnv(childRoot); ok {
		// A noisy worker must not deadlock the controller's full stdin.
		if err := writeStdout(bytes.Repeat([]byte{'x'}, 128*1024)); err != nil {
			t.Fatal(err)
		}
		length := uint64(512 * 1024)
		err := runCommand(context.Background(), commandWorker{workerArgs{command: workerCommandWriteFile{
			root:      root,
			path:      filepath.Join("nested", "input.bin"),
			overwrite: false,
			length:    &length,
		}}}, nil)
		if err != nil {
			t.Fatal(err)
		}
		return
	}
	root := t.TempDir()
	data := make([]byte, 512*1024)
	for index := range data {
		data[index] = byte(index % 251)
	}
	output := runSelf(t, t.Name(), data, map[string]string{childRoot: root})
	if output.Status != 0 {
		t.Fatalf("status %d: %s", output.Status, output.Stderr)
	}
	if len(output.Stdout) < 128*1024 {
		t.Fatalf("stdout carried %d bytes", len(output.Stdout))
	}
	if written, err := os.ReadFile(filepath.Join(root, "nested", "input.bin")); err != nil || !bytes.Equal(written, data) {
		t.Fatalf("written file differs: %v", err)
	}
	interrupted := t.TempDir()
	output = runSelf(t, t.Name(), make([]byte, 128*1024), map[string]string{childRoot: interrupted})
	if output.Status == 0 {
		t.Fatal("premature EOF must fail")
	}
	if _, err := os.Stat(filepath.Join(interrupted, "nested", "input.bin")); err == nil {
		t.Fatal("an interrupted upload must not publish a partial file")
	}
}
