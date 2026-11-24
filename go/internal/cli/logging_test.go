// Port of the tests in mj-cli/src/logging.rs.

package cli

import (
	"bytes"
	"encoding/json"
	"fmt"
	"os"
	"path/filepath"
	"runtime"
	"strings"
	"sync"
	"sync/atomic"
	"syscall"
	"testing"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

func TestReliableWriterQueuesEveryLineWithoutDroppingABurst(t *testing.T) {
	directory := t.TempDir()
	path := filepath.Join(directory, "burst.log")
	file, err := os.Create(path)
	check(t, err)
	defer file.Close()
	writer, guard, err := reliableNonBlocking(core.NewResilientWriter(file))
	check(t, err)
	for line := 0; line < 10_000; line++ {
		_, err := writer.Write([]byte(fmt.Sprintf("error %d\n", line)))
		check(t, err)
	}
	guard.Close()

	contents, err := os.ReadFile(path)
	check(t, err)
	if lines := strings.Count(string(contents), "\n"); lines != 10_000 {
		t.Fatalf("lines = %d", lines)
	}
	if !strings.HasSuffix(string(contents), "error 9999\n") {
		t.Fatalf("contents end with %q", contents[max(len(contents)-20, 0):])
	}
}

func TestReliableWriterRemainsValidAfterGuardFlushes(t *testing.T) {
	directory := t.TempDir()
	path := filepath.Join(directory, "late.log")
	file, err := os.Create(path)
	check(t, err)
	defer file.Close()
	writer, guard, err := reliableNonBlocking(core.NewResilientWriter(file))
	check(t, err)
	_, err = writer.Write([]byte("before guard drop\n"))
	check(t, err)

	guard.Close()
	_, err = writer.Write([]byte("after guard drop\n"))
	check(t, err)
	check(t, writer.Flush())

	contents, err := os.ReadFile(path)
	check(t, err)
	if string(contents) != "before guard drop\nafter guard drop\n" {
		t.Fatalf("contents = %q", contents)
	}
}

// Hard-won: d7ca109e4: a real disk write error must not disconnect the daemon log channel.
// /dev/full exists only on Linux; macOS has no equivalent full device, so
// the fake-writer test below covers this on the other Unix hosts.
func TestDevFullKeepsTheDaemonLogThreadConnected(t *testing.T) {
	if runtime.GOOS != "linux" {
		t.Skip("/dev/full exists only on Linux, as the Rust test is cfg(target_os = \"linux\")")
	}
	full, err := os.OpenFile("/dev/full", os.O_WRONLY, 0)
	check(t, err)
	defer full.Close()
	sink := core.NewResilientWriter(&deferredLogFile{path: "/dev/full", file: full})
	writer, guard, err := reliableNonBlocking(sink)
	check(t, err)
	for range 1000 {
		_, err := writer.Write([]byte("daemon diagnostic\n"))
		check(t, err)
	}
	check(t, writer.flushChecked())
	dropped := sink.DroppedLines()
	if dropped == nil || dropped.Count != 1000 {
		t.Fatalf("dropped = %+v", dropped)
	}
	if !strings.Contains(dropped.Error, "No space left on device") {
		t.Fatalf("dropped error = %q", dropped.Error)
	}
	guard.Close()
	_, err = writer.Write([]byte("still connected\n"))
	check(t, err)
	check(t, writer.flushChecked())
	if dropped := sink.DroppedLines(); dropped == nil || dropped.Count != 1001 {
		t.Fatalf("dropped = %+v", dropped)
	}
}

// fullDisk is a writer that fails with ENOSPC while full.
type fullDisk struct {
	full   *atomic.Bool
	mu     *sync.Mutex
	output *bytes.Buffer
}

func (d *fullDisk) Write(written []byte) (int, error) {
	if d.full.Load() {
		return 0, syscall.ENOSPC
	}
	d.mu.Lock()
	defer d.mu.Unlock()
	return d.output.Write(written)
}

func (d *fullDisk) Flush() error {
	if d.full.Load() {
		return syscall.ENOSPC
	}
	return nil
}

// Hard-won: d7ca109e4: daemon logging stopped permanently after the first ENOSPC.
func TestLogThreadSurvivesFullDiskAndFlushThenReportsRecovery(t *testing.T) {
	var full atomic.Bool
	full.Store(true)
	var mu sync.Mutex
	var output bytes.Buffer
	sink := core.NewResilientWriter(&fullDisk{full: &full, mu: &mu, output: &output})
	writer, guard, err := reliableNonBlocking(sink)
	check(t, err)
	for range 1000 {
		_, err := writer.Write([]byte("lost line\n"))
		check(t, err)
	}
	check(t, writer.flushChecked())
	if dropped := sink.DroppedLines(); dropped == nil || dropped.Count != 1000 {
		t.Fatalf("dropped = %+v", dropped)
	}
	full.Store(false)
	_, err = writer.Write([]byte("recovered line\n"))
	check(t, err)
	check(t, writer.flushChecked())
	guard.Close()
	mu.Lock()
	text := output.String()
	mu.Unlock()
	if count := strings.Count(text, "dropped 1000 log lines between"); count != 1 {
		t.Fatalf("summaries = %d in %q", count, text)
	}
	if !strings.HasSuffix(text, "recovered line\n") {
		t.Fatalf("text = %q", text)
	}
}

// otherPID is an arbitrary PID used for logs that are never the protected
// daemon PID in a given test.
const otherPID uint32 = 999_999_999

// Hard-won: 1d6a27cb6a: A short-lived CLI could prune the older log still being written by the live daemon; the test protects the daemon PID log beyond the normal retention count.
func TestPruneLogsNeverRemovesTheCurrentDaemonsLog(t *testing.T) {
	dataDir := t.TempDir()
	const daemonPID uint32 = 4_242_424
	metadata, err := json.Marshal(map[string]any{
		"protocol_version": 1,
		"pid":              daemonPID,
		"address":          "127.0.0.1:0",
		"token":            "t",
		"started_at":       "2026-01-01T00:00:00Z",
		"build_version":    "0.0.0",
	})
	check(t, err)
	check(t, os.WriteFile(filepath.Join(dataDir, "daemon.json"), metadata, 0o644))
	logsDir := filepath.Join(dataDir, "logs")
	check(t, os.MkdirAll(logsDir, 0o755))

	// The current daemon's own log is the oldest by filename, so a naive
	// newest-N prune would delete it first.
	protectedName := fmt.Sprintf("mj-daemon-20260101T000000.000Z-%d.log", daemonPID)
	check(t, os.WriteFile(filepath.Join(logsDir, protectedName), []byte("daemon"), 0o644))
	var otherNames []string
	for index := range 12 {
		otherNames = append(otherNames, fmt.Sprintf("mj-daemon-202602%02dT000000.000Z-%d.log", index, otherPID))
	}
	for _, name := range otherNames {
		check(t, os.WriteFile(filepath.Join(logsDir, name), []byte("daemon"), 0o644))
	}

	resolvedPID, found := currentDaemonPID(dataDir)
	if !found || resolvedPID != daemonPID {
		t.Fatalf("current daemon PID = %d, %t", resolvedPID, found)
	}
	check(t, pruneLogs(logsDir, retainedLogs-1, &resolvedPID))

	if _, err := os.Stat(filepath.Join(logsDir, protectedName)); err != nil {
		t.Fatalf("the current daemon's own log must survive pruning even when it is the oldest: %v", err)
	}
	remainingOthers := 0
	for _, name := range otherNames {
		if _, err := os.Stat(filepath.Join(logsDir, name)); err == nil {
			remainingOthers++
		}
	}
	if remainingOthers != retainedLogs-1 {
		t.Fatalf("other daemon-kind logs beyond the retained window are still pruned: %d remain", remainingOthers)
	}
}

// Under a script that runs a command every second, the newest ten logs cover
// ten seconds. A command still running, or one that ran within the hour,
// keeps its log.
// Hard-won: 4ce833574f: A live replacement report disappeared from logs within seconds; this checks recent files and active-process logs survive pruning.
func TestPruneLogsKeepsRecentLogsAndLogsOfRunningProcesses(t *testing.T) {
	directory := t.TempDir()
	running := fmt.Sprintf("mj-cli-20260101T000000.000Z-%d.log", os.Getpid())
	check(t, os.WriteFile(filepath.Join(directory, running), []byte("running"), 0o644))
	for index := range 30 {
		name := fmt.Sprintf("mj-cli-20260105T0000%02d.000Z-%d.log", index, otherPID)
		check(t, os.WriteFile(filepath.Join(directory, name), []byte("recent"), 0o644))
	}
	old := fmt.Sprintf("mj-cli-20260102T000000.000Z-%d.log", otherPID)
	check(t, os.WriteFile(filepath.Join(directory, old), []byte("old"), 0o644))

	check(t, pruneLogsSince(directory, 2, nil, "20260105T000000.000Z"))

	if _, err := os.Stat(filepath.Join(directory, running)); err != nil {
		t.Fatalf("the running process's log was pruned: %v", err)
	}
	if _, err := os.Stat(filepath.Join(directory, old)); err == nil {
		t.Fatal("the old log survived")
	}
	entries, err := os.ReadDir(directory)
	check(t, err)
	recent := 0
	for _, entry := range entries {
		if strings.Contains(entry.Name(), "20260105") {
			recent++
		}
	}
	if recent != 30 {
		t.Fatalf("every log started in the window is kept: %d remain", recent)
	}
}

// countNames is how many entries of directory match.
func countNames(t *testing.T, directory string, matches func(name string) bool) int {
	t.Helper()
	entries, err := os.ReadDir(directory)
	check(t, err)
	count := 0
	for _, entry := range entries {
		if matches(entry.Name()) {
			count++
		}
	}
	return count
}

// Hard-won: 1d6a27cb: A burst of short-lived CLI logs could prune the older live daemon log.
func TestPruneLogsRetainsPerKind(t *testing.T) {
	directory := t.TempDir()
	for index := range 12 {
		name := fmt.Sprintf("mj-cli-202601%02dT000000.000Z-%d.log", index, otherPID)
		check(t, os.WriteFile(filepath.Join(directory, name), []byte("cli"), 0o644))
	}
	daemonName := fmt.Sprintf("mj-daemon-20260101T000000.000Z-%d.log", otherPID)
	check(t, os.WriteFile(filepath.Join(directory, daemonName), []byte("daemon"), 0o644))

	check(t, pruneLogs(directory, retainedLogs-1, nil))

	if _, err := os.Stat(filepath.Join(directory, daemonName)); err != nil {
		t.Fatalf("the single daemon log must survive even though 12 cli logs are newer by name: %v", err)
	}
	if remaining := countNames(t, directory, func(name string) bool { return strings.HasPrefix(name, "mj-cli-") }); remaining != retainedLogs-1 {
		t.Fatalf("remaining cli logs = %d", remaining)
	}

	// A crashed daemon's log is a kind of its own: a stream of newer
	// ordinary daemon logs prunes only the ordinary ones.
	var crashedNames []string
	for index := range 2 {
		crashedNames = append(crashedNames, fmt.Sprintf("mj-daemon-2025010%dT000000.000Z-%d.crashed.log", index, otherPID))
	}
	for _, name := range crashedNames {
		check(t, os.WriteFile(filepath.Join(directory, name), []byte("crashed"), 0o644))
	}
	for index := range 12 {
		name := fmt.Sprintf("mj-daemon-202602%02dT000000.000Z-%d.log", index, otherPID)
		check(t, os.WriteFile(filepath.Join(directory, name), []byte("daemon"), 0o644))
	}
	check(t, pruneLogs(directory, retainedLogs-1, nil))
	for _, name := range crashedNames {
		if _, err := os.Stat(filepath.Join(directory, name)); err != nil {
			t.Fatalf("%s was pruned", name)
		}
	}
	if remaining := countNames(t, directory, func(name string) bool {
		return strings.HasPrefix(name, "mj-daemon-") && !strings.HasSuffix(name, ".crashed.log")
	}); remaining != retainedLogs-1 {
		t.Fatalf("remaining daemon logs = %d", remaining)
	}
}

// Hard-won: 1d6a27cb: Legacy CLI log names could still crowd out the live daemon log after filename versioning.
func TestPruneLogsTreatsLegacyFilenamesAsCli(t *testing.T) {
	directory := t.TempDir()
	var legacyNames []string
	for index := range 12 {
		legacyNames = append(legacyNames, fmt.Sprintf("mj-202601%02dT000000.000Z-%d.log", index, otherPID))
	}
	for _, name := range legacyNames {
		check(t, os.WriteFile(filepath.Join(directory, name), []byte("legacy"), 0o644))
	}
	daemonName := fmt.Sprintf("mj-daemon-20260101T000000.000Z-%d.log", otherPID)
	check(t, os.WriteFile(filepath.Join(directory, daemonName), []byte("daemon"), 0o644))

	check(t, pruneLogs(directory, retainedLogs-1, nil))

	if _, err := os.Stat(filepath.Join(directory, daemonName)); err != nil {
		t.Fatalf("legacy cli-shaped logs must not crowd out the daemon's retained window: %v", err)
	}
	remainingLegacy := 0
	for _, name := range legacyNames {
		if _, err := os.Stat(filepath.Join(directory, name)); err == nil {
			remainingLegacy++
		}
	}
	if remainingLegacy != retainedLogs-1 {
		t.Fatalf("legacy filenames are pruned in the same group as mj-cli-* files: %d remain", remainingLegacy)
	}
}

func TestRemoveExpiredLogIgnoresACandidateRemovedByAnotherProcess(t *testing.T) {
	directory := t.TempDir()
	path := filepath.Join(directory, "mj-cli-20260824T000000.000Z-1.log")
	check(t, os.WriteFile(path, []byte("expired"), 0o644))

	// This is the state observed when another process wins the race between
	// pruneLogs' directory scan and its remove call.
	check(t, os.Remove(path))

	check(t, removeExpiredLog(path))
}

func TestRemoveExpiredLogReportsNonMissingErrors(t *testing.T) {
	directory := t.TempDir()
	path := filepath.Join(directory, "mj-20260824T000000.000Z-1.log")
	check(t, os.Mkdir(path, 0o755))

	err := removeExpiredLog(path)

	if err == nil || !strings.Contains(err.Error(), "remove expired Mjolnir log") {
		t.Fatalf("unexpected error: %v", err)
	}
}
