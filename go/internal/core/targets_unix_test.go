//go:build unix

// Port of the executor tests in mj-core/src/targets.rs (Rust:
// #[cfg(all(test, unix))] mod executor_tests and mod fake_ssh).

package core

import (
	"bytes"
	"context"
	"errors"
	"fmt"
	"io"
	"os"
	"os/signal"
	"path/filepath"
	"runtime"
	"slices"
	"strconv"
	"strings"
	"sync"
	"sync/atomic"
	"syscall"
	"testing"
	"time"
)

// fastRetriesLock: the retry backoff override is process-wide, so tests
// that shorten it take turns: one test restoring the real delay mid-run
// would stretch another's retries past its deadline.
var fastRetriesLock sync.Mutex

// fastRetries shortens the SSH retry backoff until closed (Rust:
// fake_ssh::FastRetries).
type fastRetries struct{}

func newFastRetries() *fastRetries {
	fastRetriesLock.Lock()
	delay := 5 * time.Millisecond
	SetSSHRetryBackoffForTest(&delay)
	return &fastRetries{}
}

func (*fastRetries) close() {
	SetSSHRetryBackoffForTest(nil)
	SetSSHPersistentRefusalForTest(nil)
	fastRetriesLock.Unlock()
}

// fakeSSH is a stand-in `ssh` for tests: a script on PATH that refuses the
// first attempts as a plan says and otherwise runs the remote command line
// in a shell, as the remote side would, so the start marker is printed by
// the real line.
type fakeSSH struct {
	directory string
}

// newFakeSSH: plan has one word per attempt: `refuse-session`,
// `refuse-auth`, `refuse-session-late-stderr` (closes stdout first and
// prints the refusal to stderr moments later), `warn-then-run` (prints an
// ssh warning to stderr before the session starts, then runs), or
// `run-then-255` (runs the command, then fails like a dropped connection).
// Attempts past the plan just run.
func newFakeSSH(t *testing.T, plan []string) *fakeSSH {
	t.Helper()
	directory := t.TempDir()
	script := `#!/bin/sh
dir=$(dirname "$0")
count=$(cat "$dir/attempts" 2>/dev/null || echo 0)
echo $((count + 1)) > "$dir/attempts"
for last; do :; done
case "$(sed -n "$((count + 1))p" "$dir/plan")" in
  refuse-session)
    echo 'mux_client_request_session: session request failed: Session open refused by peer' >&2
    exit 255 ;;
  refuse-session-late-stderr)
    exec >&-
    sleep 0.3
    echo 'mux_client_request_session: session request failed: Session open refused by peer' >&2
    exit 255 ;;
  refuse-auth)
    echo 'kex_exchange_identification: Connection closed by 10.0.0.1 port 22' >&2
    exit 255 ;;
  warn-then-run)
    echo "Warning: Permanently added 'fake-host' to the list of known hosts." >&2
    sleep 0.3
    exec sh -c "$last" ;;
  run-then-255)
    sh -c "$last"
    echo 'Read from remote host fake-host: Connection reset by peer' >&2
    exit 255 ;;
  *) exec sh -c "$last" ;;
esac
`
	if err := os.WriteFile(filepath.Join(directory, "ssh"), []byte(script), 0o755); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(directory, "plan"), []byte(strings.Join(plan, "\n")), 0o644); err != nil {
		t.Fatal(err)
	}
	return &fakeSSH{directory: directory}
}

// command is an `ssh` session command for remote on the fake host.
func (f *fakeSSH) command(remote []string) CommandSpec {
	ssh := SshTarget{Destination: "fake-host"}
	command := SSHCommand(SshTrafficClassControl, &ssh, remote).WithPurpose("run through the fake ssh")
	command.Env["PATH"] = f.directory + ":" + os.Getenv("PATH")
	return command
}

func (f *fakeSSH) attempts(t *testing.T) uint32 {
	t.Helper()
	text, err := os.ReadFile(filepath.Join(f.directory, "attempts"))
	if err != nil {
		return 0
	}
	count, err := strconv.ParseUint(strings.TrimSpace(string(text)), 10, 32)
	if err != nil {
		t.Fatalf("attempt count: %v", err)
	}
	return uint32(count)
}

func TestStepScriptReportsPartialFailureAndRunsCleanup(t *testing.T) {
	directory := t.TempDir()
	cleaned := filepath.Join(directory, "cleanup 'with spaces")
	skipped := filepath.Join(directory, "must not run")
	argv := StepScript([]CommandStep{
		NewCommandStep("absent Git key", []string{"sh", "-c", "exit 5"}).AcceptedStatuses(0, 5),
		NewCommandStep("failing middle step", []string{"sh", "-c", "echo precise-error >&2; exit 7"}),
		NewCommandStep("later operation", []string{"touch", skipped}),
		NewCommandStep("remove staging", []string{"touch", cleaned}).Cleanup(),
		NewCommandStep("optional cleanup", []string{"sh", "-c", "echo cleanup-error >&2; exit 9"}).BestEffortCleanup(),
	})
	command := NewCommandSpec(argv[0], argv[1:]).WithPurpose("batch")
	output, err := ProcessExecutor{}.Execute(&command)
	if err != nil {
		t.Fatal(err)
	}
	if output.Status != 7 {
		t.Fatalf("status %d", output.Status)
	}
	warnings := StepScriptWarnings(&output)
	if len(warnings) != 1 {
		t.Fatalf("warnings %v", warnings)
	}
	if !strings.Contains(warnings[0], "optional cleanup failed with status 9") || !strings.Contains(warnings[0], "cleanup-error") {
		t.Fatalf("warning %q", warnings[0])
	}
	_, err = CheckedCommandOutput(&command, output)
	if err == nil || !strings.HasPrefix(err.Error(), "failing middle step failed with status 7: precise-error") {
		t.Fatalf("%v", err)
	}
	if _, err := os.Stat(cleaned); err != nil {
		t.Fatalf("cleanup did not run: %v", err)
	}
	if _, err := os.Stat(skipped); err == nil {
		t.Fatal("a step after the failure ran")
	}

	// Cleanup failure remains visible even after successful publication.
	argv = StepScript([]CommandStep{
		NewCommandStep("optional discard", []string{"sh", "-c", "echo discard-error >&2; exit 9"}).BestEffortCleanup(),
	})
	command = NewCommandSpec(argv[0], argv[1:])
	output, err = ProcessExecutor{}.Execute(&command)
	if err != nil {
		t.Fatal(err)
	}
	if output.Status != 0 {
		t.Fatalf("status %d", output.Status)
	}
	if got := StepScriptWarnings(&output); !slices.Equal(got, []string{"optional discard failed with status 9: discard-error"}) {
		t.Fatalf("warnings %q", got)
	}
}

// Hard-won: leaked exec records: a broken SSH pipe killed the attached client.
func TestContainerExecDrainPreservesStreamsStdinAndStatus(t *testing.T) {
	temp := t.TempDir()
	fake := filepath.Join(temp, "podman")
	// Never exec a file this multi-threaded test just wrote (ETXTBSY).
	InstallFakeCommand(temp, "podman", "shift 3\ncat\nprintf 'stderr bytes\\n' >&2\nexit \"$1\"\n")
	for _, status := range []int{0, 37, 23, 255} {
		plain := NewCommandSpec(fake, []string{"exec", "-i", "ctr", strconv.Itoa(status)})
		wrapped := DrainedContainerExec(fake, "ctr", []string{strconv.Itoa(status)})
		input := bytes.Repeat([]byte{'x'}, 204800)
		expected, err := ProcessExecutor{}.ExecuteWithStdin(&plain, bytes.NewReader(input))
		if err != nil {
			t.Fatal(err)
		}
		actual, err := ProcessExecutor{}.ExecuteWithStdin(&wrapped, bytes.NewReader(input))
		if err != nil {
			t.Fatal(err)
		}
		if !bytes.Equal(actual.Stdout, expected.Stdout) || !bytes.Equal(actual.Stderr, expected.Stderr) || actual.Status != expected.Status {
			t.Fatalf("status %d: drained %d/%q differs from plain %d/%q", status, actual.Status, actual.Stderr, expected.Status, expected.Stderr)
		}
		if len(actual.Stdout) != len(input) {
			t.Fatalf("stdout length %d", len(actual.Stdout))
		}
		argv := append([]string{wrapped.Program}, wrapped.Args...)
		marked := NewCommandSpec("sh", []string{"-c", withStartMarker(RemoteCommandLine(argv))})
		markedOutput, err := ProcessExecutor{}.ExecuteWithStdin(&marked, bytes.NewReader(input))
		if err != nil {
			t.Fatal(err)
		}
		if !bytes.Equal(markedOutput.Stdout, append([]byte(SSHStartMarker), actual.Stdout...)) {
			t.Fatal("the marked stdout is not the marker and the drained stdout")
		}
		if !bytes.Equal(markedOutput.Stderr, actual.Stderr) || markedOutput.Status != actual.Status {
			t.Fatalf("marked %d/%q differs from drained %d/%q", markedOutput.Status, markedOutput.Stderr, actual.Status, actual.Stderr)
		}
	}
}

// Hard-won: leaked exec records: output cancellation must let the client exit normally.
// Hard-won: 1e324ede0: BSD head rejects zero-byte readers in this fixture.
func TestContainerExecDrainSurvivesClosedDownstreamPipes(t *testing.T) {
	temp := t.TempDir()
	fake := filepath.Join(temp, "podman")
	InstallFakeCommand(temp, "podman",
		"set -e\nshift 3\nhead -c 204800 /dev/zero\nhead -c 204800 /dev/zero >&2\nprintf done > \"$1\"\n")
	marker := filepath.Join(temp, "normal-exit")
	wrapped := DrainedContainerExec(fake, "ctr", []string{marker})
	argv := append([]string{wrapped.Program}, wrapped.Args...)
	line := fmt.Sprintf("{ %s 2>&1 1>&3 | true; } 3>&1 | true", RemoteCommandLine(argv))
	command := NewCommandSpec("sh", []string{"-c", line})
	result, err := ProcessExecutor{}.Execute(&command)
	if err != nil {
		t.Fatal(err)
	}
	if result.Status != 0 {
		t.Fatalf("status %d", result.Status)
	}
	if text, err := os.ReadFile(marker); err != nil || string(text) != "done" {
		t.Fatalf("marker %q, %v", text, err)
	}
}

// waitForDescendantExit waits up to two seconds for pid to be gone or a
// zombie, killing it and failing otherwise.
func waitForDescendantExit(t *testing.T, pid int, failure string) {
	t.Helper()
	deadline := time.Now().Add(2 * time.Second)
	for {
		state, err := os.ReadFile(fmt.Sprintf("/proc/%d/stat", pid))
		if err != nil {
			return
		}
		if index := strings.LastIndex(string(state), ") "); index >= 0 && strings.HasPrefix(string(state)[index+2:], "Z") {
			return
		}
		if time.Now().After(deadline) {
			// Do not leave the fixture behind when the regression fails.
			syscall.Kill(pid, syscall.SIGKILL)
			t.Fatal(failure)
		}
		time.Sleep(10 * time.Millisecond)
	}
}

func readPID(t *testing.T, path string) int {
	t.Helper()
	text, err := os.ReadFile(path)
	if err != nil {
		t.Fatal(err)
	}
	pid, err := strconv.Atoi(strings.TrimSpace(string(text)))
	if err != nil {
		t.Fatal(err)
	}
	return pid
}

func TestSuccessfulCommandsStopDescendantsThatClosedTheirPipes(t *testing.T) {
	if runtime.GOOS != "linux" {
		t.Skip("reads /proc")
	}
	for _, streamed := range []bool{false, true} {
		pidFile := filepath.Join(t.TempDir(), "descendant")
		command := NewCommandSpec("sh", []string{
			"-c",
			"cat >/dev/null; head -c 131072 /dev/zero; sleep 60 </dev/null >/dev/null 2>&1 & echo $! > \"$1\"",
			"owned-descendant", pidFile,
		})
		executor := CancellableProcessExecutorWithTimeout(5 * time.Second)
		var output CommandOutput
		var err error
		if streamed {
			output, err = executor.ExecuteWithStdin(&command, bytes.NewReader(bytes.Repeat([]byte{'x'}, 256*1024)))
		} else {
			output, err = executor.Execute(&command)
		}
		if err != nil {
			t.Fatal(err)
		}
		if len(output.Stdout) != 131072 {
			t.Fatalf("stdout length %d", len(output.Stdout))
		}
		pid := readPID(t, pidFile)
		waitForDescendantExit(t, pid, fmt.Sprintf("successful command left descendant %d running (streamed=%v)", pid, streamed))
	}
}

// B-2 at the executor: a backgrounded descendant that holds the output pipes
// must not hold the completed command past the drain.
// Hard-won: c86b2123: A target command stayed Running until a background descendant closed inherited pipes.
func TestCommandsCompleteAtLeaderExitWhenADescendantHoldsThePipes(t *testing.T) {
	if runtime.GOOS != "linux" {
		t.Skip("reads /proc")
	}
	for _, streamed := range []bool{false, true} {
		pidFile := filepath.Join(t.TempDir(), "descendant")
		command := NewCommandSpec("sh", []string{
			"-c", "sleep 300 & echo $! > \"$1\"; echo hi; exit 3", "held-pipes", pidFile,
		})
		executor := CancellableProcessExecutorWithTimeout(30 * time.Second)
		started := time.Now()
		var output CommandOutput
		var err error
		if streamed {
			output, err = executor.ExecuteWithStdin(&command, bytes.NewReader([]byte("input")))
		} else {
			output, err = executor.Execute(&command)
		}
		if err != nil {
			t.Fatal(err)
		}
		if elapsed := time.Since(started); elapsed >= 5*time.Second {
			t.Fatalf("streamed=%v took %v", streamed, elapsed)
		}
		if output.Status != 3 || string(output.Stdout) != "hi\n" {
			t.Fatalf("status %d stdout %q", output.Status, output.Stdout)
		}
		pid := readPID(t, pidFile)
		waitForDescendantExit(t, pid, fmt.Sprintf("completed command left descendant %d (streamed=%v)", pid, streamed))
	}
}

func TestStreamedDeadlineSurvivesLeaderExitAndInheritedPipes(t *testing.T) {
	command := NewCommandSpec("sh", []string{
		"-c", "head -c 131072 /dev/zero; cat >/dev/null; (trap '' TERM; sleep 60) & exit 0",
	})
	started := time.Now()
	_, err := CancellableProcessExecutorWithTimeout(300*time.Millisecond).
		ExecuteWithStdin(&command, bytes.NewReader(bytes.Repeat([]byte{'x'}, 256*1024)))
	if err == nil || !strings.Contains(err.Error(), "timed out after 0.3 s while") {
		t.Fatalf("%v", err)
	}
	if time.Since(started) >= 5*time.Second {
		t.Fatal("the deadline did not end the command")
	}
}

func TestExecutorDeadlinesNameTheDurationAndPurposeWithoutRelabellingCancellation(t *testing.T) {
	command := NewCommandSpec("sh", []string{"-c", "sleep 30"}).WithPurpose("stage a worker")
	_, err := CancellableProcessExecutorWithTimeout(50 * time.Millisecond).Execute(&command)
	if err == nil || err.Error() != "timed out after 0.05 s while stage a worker" {
		t.Fatalf("%v", err)
	}
	cancelled := new(atomic.Bool)
	cancelled.Store(true)
	_, err = NewCancellableProcessExecutor(cancelled).WithDeadline(0).Execute(&command)
	if err == nil || err.Error() != "operation cancelled while stage a worker" {
		t.Fatalf("%v", err)
	}
}

func TestCleanupTimeoutReplacesAnExpiredDeadlineAndKeepsSharedCancellation(t *testing.T) {
	marker := filepath.Join(t.TempDir(), "completed")
	command := NewCommandSpec("sh", []string{"-c", "printf ran > \"$1\"", "cleanup-deadline", marker})
	cancelled := new(atomic.Bool)
	executor := NewCancellableProcessExecutor(cancelled).WithDeadline(0)

	output, err := executor.ExecuteCleanupWithTimeout(&command, 5*time.Second)
	if err != nil {
		t.Fatal(err)
	}
	if output.Status != 0 {
		t.Fatalf("status %d", output.Status)
	}
	if text, err := os.ReadFile(marker); err != nil || string(text) != "ran" {
		t.Fatalf("marker %q, %v", text, err)
	}

	cancelled.Store(true)
	_, err = executor.ExecuteCleanupWithTimeout(&command, 5*time.Second)
	if err == nil || !strings.Contains(err.Error(), "operation cancelled") {
		t.Fatalf("%v", err)
	}
}

// flakySSHScript is a stand-in for `ssh` that is refused by the server on
// its first calls and connects on the next, the way a host at its
// `MaxStartups` ceiling behaves once the daemon's burst drains.
//
// The count is written to a scratch file and renamed into place, so a
// deadline that kills the script mid-write never leaves an empty counter.
func flakySSHScript(directory string, refusals uint32) CommandSpec {
	counter := filepath.Join(directory, "attempts")
	script := fmt.Sprintf("count=$(cat %[1]s 2>/dev/null || echo 0)\n"+
		"echo $((count + 1)) > %[1]s.tmp && mv %[1]s.tmp %[1]s\n"+
		"if [ \"$count\" -lt %[2]d ]; then\n"+
		"echo 'kex_exchange_identification: Connection closed by 10.0.0.1 port 22' >&2\n"+
		"exit 255\n"+
		"fi\n"+
		"echo connected\n", counter, refusals)
	return NewCommandSpec("sh", []string{"-c", script}).
		WithSSHDestination("build@10.0.0.1").
		WithPurpose("run the flaky SSH fixture")
}

func flakyAttempts(t *testing.T, directory string) uint32 {
	t.Helper()
	text, err := os.ReadFile(filepath.Join(directory, "attempts"))
	if err != nil {
		t.Fatalf("the fixture records its attempts: %v", err)
	}
	count, err := strconv.ParseUint(strings.TrimSpace(string(text)), 10, 32)
	if err != nil {
		t.Fatalf("attempt count is a number: %v", err)
	}
	return uint32(count)
}

// masterKilledOnce is an executor that models `ssh` masters and the
// sessions on them. The first session finds its master dead (it was killed
// after the ledger last checked it), which `ssh` reports the way a session
// guarded with `ProxyCommand=false` does.
type masterKilledOnce struct {
	CommandExecutorDefaults
	mu       sync.Mutex
	running  map[string]bool
	openers  int
	sessions [][]string
}

func (e *masterKilledOnce) PauseDeadline() *DeadlinePause { return DeadlinePauseNone() }

func (e *masterKilledOnce) CancellationRequestedWithoutDeadline() bool {
	return e.CancellationRequested()
}

func (e *masterKilledOnce) ExecuteWithoutDeadline(command *CommandSpec) (CommandOutput, error) {
	return e.Execute(command)
}

func (e *masterKilledOnce) ExecuteWithStdinWithoutDeadline(command *CommandSpec, input io.Reader) (CommandOutput, error) {
	return e.ExecuteWithStdin(command, input)
}

func (e *masterKilledOnce) ExecuteCleanup(command *CommandSpec) (CommandOutput, error) {
	return e.Execute(command)
}

func (e *masterKilledOnce) ExecuteCleanupWithTimeout(command *CommandSpec, _ time.Duration) (CommandOutput, error) {
	return e.ExecuteCleanup(command)
}

func (e *masterKilledOnce) Execute(command *CommandSpec) (CommandOutput, error) {
	reply := func(status int, stderr string) CommandOutput {
		return CommandOutput{Status: status, Stdout: []byte{}, Stderr: []byte(stderr)}
	}
	if command.SSHSession != nil {
		// A session. The first one arrives just after its master died.
		return withSSHRefusalRetry(command, e, nil, func(spawned *CommandSpec, _ *StartGate) (CommandOutput, error) {
			e.mu.Lock()
			defer e.mu.Unlock()
			e.sessions = append(e.sessions, append([]string{}, spawned.Args...))
			if len(e.sessions) == 1 {
				e.running = map[string]bool{}
				return reply(255, "Connection closed by UNKNOWN port 65535"), nil
			}
			return reply(0, ""), nil
		})
	}
	// The first session on a master the ledger just opened.
	if slices.Contains(command.Args, "ProxyCommand=false") {
		return reply(0, ""), nil
	}
	socket := ""
	for _, arg := range command.Args {
		if path, ok := strings.CutPrefix(arg, "ControlPath="); ok {
			socket = path
			break
		}
	}
	if socket == "" {
		panic("a master command names its socket")
	}
	e.mu.Lock()
	defer e.mu.Unlock()
	for i := 0; i+1 < len(command.Args); i++ {
		if command.Args[i] == "-O" && command.Args[i+1] == "check" {
			if e.running[socket] {
				return reply(0, ""), nil
			}
			return reply(255, ""), nil
		}
	}
	if !slices.Contains(command.Args, "ControlMaster=yes") {
		panic(fmt.Sprintf("an opener: %v", command.Args))
	}
	e.openers++
	if e.running == nil {
		e.running = map[string]bool{}
	}
	e.running[socket] = true
	return reply(0, ""), nil
}

func TestASessionWhoseMasterDiedIsRetriedOnAReopenedMaster(t *testing.T) {
	fast := newFastRetries()
	defer fast.close()
	socketDir, err := os.MkdirTemp("/tmp", "mj-sockets")
	if err != nil {
		t.Fatalf("short socket directory: %v", err)
	}
	defer os.RemoveAll(socketDir)
	SetSSHConnectionSharingForTest(&SshSharingForTest{Kind: SshSharingForTestDirectory, Directory: socketDir})
	ssh := SshTarget{Destination: "master-killed-once-host"}
	executor := &masterKilledOnce{}
	command := SSHCommand(SshTrafficClassControl, &ssh, []string{"true"})
	output, err := executor.Execute(&command)
	SetSSHConnectionSharingForTest(nil)

	if err != nil {
		t.Fatalf("the retry succeeds: %v", err)
	}
	if output.Status != 0 {
		t.Fatalf("status %d", output.Status)
	}
	if len(executor.sessions) != 2 {
		t.Fatalf("sessions %v", executor.sessions)
	}
	if executor.openers != 2 {
		t.Fatalf("the retry reopens the master instead of trusting the earlier check: %d openers", executor.openers)
	}
	for _, args := range executor.sessions {
		if len(args) < 6 || args[5] != "ProxyCommand=false" {
			t.Fatalf("%v", args)
		}
	}
}

// Hard-won: e6ed54ed: sshd MaxStartups rejected concurrent session handshakes and users saw dead workers and failed closes.
func TestATransportRejectedSshCommandIsRetriedOnceAndThenSucceeds(t *testing.T) {
	fast := newFastRetries()
	defer fast.close()
	directory := t.TempDir()
	command := flakySSHScript(directory, 1)

	output, err := ProcessExecutor{}.Execute(&command)
	if err != nil {
		t.Fatalf("the retry must reach the successful attempt: %v", err)
	}
	if output.Status != 0 || strings.TrimSpace(string(output.Stdout)) != "connected" {
		t.Fatalf("status %d stdout %q", output.Status, output.Stdout)
	}
	if got := flakyAttempts(t, directory); got != 2 {
		t.Fatalf("attempts %d", got)
	}
}

// Hard-won: e6ed54ed: sshd MaxStartups rejected remote operations during daemon startup and they were falsely reported as failed.
func TestTheCancellableExecutorAlsoRetriesATransportRejection(t *testing.T) {
	fast := newFastRetries()
	defer fast.close()
	directory := t.TempDir()
	command := flakySSHScript(directory, 1)

	output, err := NewCancellableProcessExecutor(new(atomic.Bool)).Execute(&command)
	if err != nil {
		t.Fatalf("the retry must reach the successful attempt: %v", err)
	}
	if output.Status != 0 {
		t.Fatalf("status %d", output.Status)
	}
	if got := flakyAttempts(t, directory); got != 2 {
		t.Fatalf("attempts %d", got)
	}
}

// A host at its `MaxStartups` ceiling can keep dropping connections for
// longer than any fixed budget. The command never ran, so a caller that can
// be cancelled keeps coming back until it gets in.
func TestACancellableCallerRetriesPreAuthenticationRefusalsUntilItGetsIn(t *testing.T) {
	fast := newFastRetries()
	defer fast.close()
	directory := t.TempDir()
	command := flakySSHScript(directory, 7)

	output, err := NewCancellableProcessExecutor(new(atomic.Bool)).Execute(&command)
	if err != nil {
		t.Fatalf("the seventh attempt gets in: %v", err)
	}
	if output.Status != 0 {
		t.Fatalf("status %d", output.Status)
	}
	if got := flakyAttempts(t, directory); got != 8 {
		t.Fatalf("attempts %d", got)
	}
}

// A permanent block (a firewall or gateway that always closes the
// connection) looks like `MaxStartups`. The retries end at the caller's
// deadline when it has one, and after the persistent-refusal bound for every
// caller, cancellable or not, with an error that names the host and the
// possible causes.
// Hard-won: 5cb64e019: pre-authentication refusals were retried until cancellation, so a cancellable caller with no deadline (Move, session creation) retried a permanent block forever.
func TestPreAuthenticationRetriesEndAtTheDeadlineOrTheTimeBound(t *testing.T) {
	fast := newFastRetries()
	defer fast.close()
	directory := t.TempDir()
	command := flakySSHScript(directory, ^uint32(0))
	_, err := CancellableProcessExecutorWithTimeout(2 * time.Second).Execute(&command)
	if err == nil || !strings.Contains(err.Error(), "timed out") {
		t.Fatalf("a server that never lets the command in ends at the deadline: %v", err)
	}
	if got := flakyAttempts(t, directory); got <= 3 {
		t.Fatalf("attempts %d", got)
	}

	bound := 400 * time.Millisecond
	SetSSHPersistentRefusalForTest(&bound)
	for _, cancellable := range []bool{true, false} {
		directory := t.TempDir()
		command := flakySSHScript(directory, ^uint32(0))
		started := time.Now()
		var err error
		if cancellable {
			_, err = NewCancellableProcessExecutor(new(atomic.Bool)).Execute(&command)
		} else {
			_, err = ProcessExecutor{}.Execute(&command)
		}
		if err == nil {
			t.Fatal("a server that never lets the command in fails after the bound")
		}
		message := err.Error()
		elapsed := time.Since(started)
		if elapsed >= 10*time.Second {
			t.Fatal(message)
		}
		for _, needle := range []string{"build@10.0.0.1", "MaxStartups", "firewall", "kex_exchange"} {
			if !strings.Contains(message, needle) {
				t.Fatalf("%s missing: %s", needle, message)
			}
		}
		// Each attempt spawns several processes, so how many fit in the
		// bound depends on the machine. Check that the caller retried and
		// that the bound, not an attempt count, ended the retries.
		if got := flakyAttempts(t, directory); got < 2 {
			t.Fatalf("cancellable: %v, retried within the bound: %d attempts", cancellable, got)
		}
		if elapsed < bound {
			t.Fatalf("cancellable: %v, stopped before the bound after %v", cancellable, elapsed)
		}
	}
}

// A fail-fast probe (target validation, `mj doctor`) fails within its own
// connect timeout when the server closes every connection before
// authentication, instead of retrying for the persistent-refusal bound of a
// minute like a command that has to get in.
// Hard-won: ca6580092: probes retried pre-authentication refusals for the full 60 s bound.
func TestAProbeGivesARefusalNoMoreTimeThanItsConnectTimeout(t *testing.T) {
	fast := newFastRetries()
	defer fast.close()
	directory := t.TempDir()
	command := flakySSHScript(directory, ^uint32(0))
	command.SSHSessionProbe = true
	started := time.Now()
	_, err := ProcessExecutor{}.Execute(&command)
	if err == nil {
		t.Fatal("a server that always closes fails the probe")
	}
	elapsed := time.Since(started)
	message := err.Error()
	if !strings.Contains(message, "build@10.0.0.1") || !strings.Contains(message, "firewall") {
		t.Fatal(message)
	}
	if elapsed < 3*time.Second || elapsed >= 15*time.Second {
		t.Fatalf("%v", elapsed)
	}
	if got := flakyAttempts(t, directory); got <= 3 {
		t.Fatalf("retried within the bound: %d attempts", got)
	}
}

// Pins both defects of an exit-255 rule without a start proof: a command
// that ran and printed its marker, then lost its connection (exit 255 with
// "Connection reset" on stderr), was retried as a transport rejection and so
// could run twice.
// Hard-won: 5cb64e019: exit 255 after the command started was retried as a refusal.
func TestASessionThatPrintedTheMarkerIsNeverRetriedEvenAtExit255(t *testing.T) {
	fast := newFastRetries()
	defer fast.close()
	SetSSHConnectionSharingForTest(&SshSharingForTest{Kind: SshSharingForTestDisabled})
	defer SetSSHConnectionSharingForTest(nil)
	for _, cancellable := range []bool{false, true} {
		fake := newFakeSSH(t, []string{"run-then-255"})
		command := fake.command([]string{"echo", "ran"})
		var output CommandOutput
		var err error
		if cancellable {
			output, err = NewCancellableProcessExecutor(new(atomic.Bool)).Execute(&command)
		} else {
			output, err = ProcessExecutor{}.Execute(&command)
		}
		if err != nil {
			t.Fatalf("ssh runs: %v", err)
		}
		if output.Status != 255 {
			t.Fatalf("status %d", output.Status)
		}
		if string(output.Stdout) != "ran\n" {
			t.Fatalf("the marker is stripped: %q", output.Stdout)
		}
		if got := fake.attempts(t); got != 1 {
			t.Fatalf("cancellable: %v: %d attempts", cancellable, got)
		}
	}
}

// A streamed command the server refuses is retried like any other, and the
// whole input (more than a pipe buffers) arrives once, on the attempt that
// started: nothing is read from the caller's stream before that.
// Hard-won: 5cb64e019: streamed commands returned the refusal instead of retrying.
func TestARefusedStreamedCommandIsRetriedAndReceivesAllItsInputOnce(t *testing.T) {
	fast := newFastRetries()
	defer fast.close()
	SetSSHConnectionSharingForTest(&SshSharingForTest{Kind: SshSharingForTestDisabled})
	defer SetSSHConnectionSharingForTest(nil)
	payload := bytes.Repeat([]byte{'x'}, 300*1024)
	for _, cancellable := range []bool{false, true} {
		fake := newFakeSSH(t, []string{"refuse-session", "refuse-auth"})
		var mu sync.Mutex
		var activity []CommandActivity
		command := fake.command([]string{"wc", "-c"}).Observe(func(event CommandActivity) {
			mu.Lock()
			activity = append(activity, event)
			mu.Unlock()
		})
		input := bytes.NewReader(payload)
		var output CommandOutput
		var err error
		if cancellable {
			output, err = NewCancellableProcessExecutor(new(atomic.Bool)).ExecuteWithStdin(&command, input)
		} else {
			output, err = ProcessExecutor{}.ExecuteWithStdin(&command, input)
		}
		if err != nil {
			t.Fatalf("the third attempt starts: %v", err)
		}
		if output.Status != 0 {
			t.Fatalf("status %d: %s", output.Status, output.Stderr)
		}
		if strings.TrimSpace(string(output.Stdout)) != strconv.Itoa(len(payload)) {
			t.Fatalf("stdout %q", output.Stdout)
		}
		if got := fake.attempts(t); got != 3 {
			t.Fatalf("cancellable: %v: %d attempts", cancellable, got)
		}
		mu.Lock()
		events := activity
		mu.Unlock()
		if len(events) < 2 || events[0].Kind != CommandActivityStdinStarted || events[len(events)-1].Kind != CommandActivityStdinFinished {
			t.Fatal("only the admitted attempt should report stdin activity")
		}
		writes := events[1 : len(events)-1]
		total := 0
		for _, event := range writes {
			if event.Kind != CommandActivityStdinBytes {
				t.Fatal("unexpected activity inside the admitted attempt")
			}
			total += event.Bytes
		}
		if len(writes) <= 1 {
			t.Fatalf("%d writes", len(writes))
		}
		if total != len(payload) {
			t.Fatalf("%d bytes reported", total)
		}
	}
}

// Hard-won: #1583: a command still running after EOF was reported as a stalled stdin stream.
func TestStreamedStdinFinishesBeforeTheCommandExits(t *testing.T) {
	cancelled := new(atomic.Bool)
	executor := NewCancellableProcessExecutor(cancelled)
	finished := make(chan struct{}, 1)
	command := NewCommandSpec("sh", []string{"-c", "cat >/dev/null; sleep 600"}).Observe(func(event CommandActivity) {
		if event.Kind == CommandActivityStdinFinished {
			select {
			case finished <- struct{}{}:
			default:
			}
		}
	})
	outcome := make(chan error, 1)
	go func() {
		_, err := executor.ExecuteWithStdin(&command, bytes.NewReader(bytes.Repeat([]byte{'x'}, 128*1024)))
		outcome <- err
	}()
	var ended bool
	select {
	case <-finished:
		ended = true
	case <-time.After(10 * time.Second):
	}
	pending := len(outcome) == 0
	cancelled.Store(true)
	err := <-outcome
	if !ended {
		t.Fatal("stdin completion should arrive while the command is still running")
	}
	if !pending {
		t.Fatal("the command finished before cancellation")
	}
	if err == nil || !strings.Contains(err.Error(), "cancelled") {
		t.Fatalf("%v", err)
	}
}

func TestFileOutputPreservesLargeSshStdoutAcrossRefusalsAndCancels(t *testing.T) {
	fast := newFastRetries()
	defer fast.close()
	SetSSHConnectionSharingForTest(&SshSharingForTest{Kind: SshSharingForTestDisabled})
	fake := newFakeSSH(t, []string{"refuse-session", "refuse-auth"})
	command := fake.command([]string{"sh", "-c", "head -c 307200 /dev/zero; printf diagnostic >&2"})
	file, err := os.CreateTemp(t.TempDir(), "output")
	if err != nil {
		t.Fatal(err)
	}
	defer file.Close()
	if _, err := file.Write([]byte("old output that must be truncated")); err != nil {
		t.Fatal(err)
	}
	output, err := NewCancellableProcessExecutor(new(atomic.Bool)).ExecuteToFile(&command, file)
	if err != nil {
		t.Fatal(err)
	}
	if output.Status != 0 || len(output.Stdout) != 0 || string(output.Stderr) != "diagnostic" {
		t.Fatalf("status %d stdout %d bytes stderr %q", output.Status, len(output.Stdout), output.Stderr)
	}
	if info, err := file.Stat(); err != nil || info.Size() != 307200 {
		t.Fatalf("file size %v, %v", info, err)
	}
	if got := fake.attempts(t); got != 3 {
		t.Fatalf("%d attempts", got)
	}
	received, err := os.ReadFile(file.Name())
	if err != nil {
		t.Fatal(err)
	}
	if !bytes.Equal(received, make([]byte, 307200)) {
		t.Fatal("the file does not hold the command's output")
	}
	SetSSHConnectionSharingForTest(nil)

	token, cancel := context.WithCancel(context.Background())
	defer cancel()
	executor := NewCancellableProcessExecutor(new(atomic.Bool)).WithCancellation(token)
	pendingFile, err := os.CreateTemp(t.TempDir(), "pending")
	if err != nil {
		t.Fatal(err)
	}
	defer pendingFile.Close()
	pending := make(chan error, 1)
	go func() {
		command := NewCommandSpec("sh", []string{"-c", "head -c 307200 /dev/zero; sleep 600"})
		_, err := executor.ExecuteToFile(&command, pendingFile)
		pending <- err
	}()
	deadline := time.Now().Add(10 * time.Second)
	for {
		info, err := os.Stat(pendingFile.Name())
		if err != nil {
			t.Fatal(err)
		}
		if info.Size() >= 307200 {
			break
		}
		if time.Now().After(deadline) {
			t.Fatal("child did not produce its output")
		}
		time.Sleep(10 * time.Millisecond)
	}
	cancel()
	if err := <-pending; err == nil || !strings.Contains(err.Error(), "cancelled") {
		t.Fatalf("%v", err)
	}
}

// Hard-won: #1544: a failed output spool was observed only after the child exited.
func TestFileOutputFailureTerminatesAChildThatKeepsRunning(t *testing.T) {
	const child = "MJ_TEST_FETCH_WRITE_FAILURE"
	if os.Getenv(child) != "" {
		// Restrict only this test subprocess's regular-file writes. Pipe
		// writes are unaffected, so the producer can still fill stdout.
		signalIgnoreSIGXFSZ()
		limit := syscall.Rlimit{Cur: 128 * 1024, Max: 128 * 1024}
		if err := syscall.Setrlimit(syscall.RLIMIT_FSIZE, &limit); err != nil {
			t.Fatal(err)
		}
		file, err := os.CreateTemp("", "mj-fetch-write-failure")
		if err != nil {
			t.Fatal(err)
		}
		defer os.Remove(file.Name())
		defer file.Close()
		command := NewCommandSpec("sh", []string{"-c", "head -c 1048576 /dev/zero; sleep 600"})
		_, err = NewCancellableProcessExecutor(new(atomic.Bool)).ExecuteToFile(&command, file)
		if !errors.Is(err, syscall.EFBIG) {
			t.Fatalf("%v", err)
		}
		return
	}
	executable, err := os.Executable()
	if err != nil {
		t.Fatal(err)
	}
	command := NewCommandSpec(executable, []string{
		"-test.run", "^TestFileOutputFailureTerminatesAChildThatKeepsRunning$", "-test.v",
	})
	command.Env[child] = "1"
	output, err := CancellableProcessExecutorWithTimeout(10 * time.Second).Execute(&command)
	if err != nil {
		t.Fatal(err)
	}
	if output.Status != 0 {
		t.Fatalf("%s %s", output.Stdout, output.Stderr)
	}
}

// An `scp` exits 1, not ssh's 255, while relaying the refusal of the `ssh`
// it runs; the copy is still retried instead of failing.
// Hard-won: af9848339: scp's own exit status hid an SSH session refusal from the retry
func TestAnScpThatRelaysARefusedSessionIsRetried(t *testing.T) {
	fast := newFastRetries()
	defer fast.close()
	SetSSHConnectionSharingForTest(&SshSharingForTest{Kind: SshSharingForTestDisabled})
	defer SetSSHConnectionSharingForTest(nil)
	directory := t.TempDir()
	script := `#!/bin/sh
dir=$(dirname "$0")
count=$(cat "$dir/attempts" 2>/dev/null || echo 0)
echo $((count + 1)) > "$dir/attempts"
if [ "$count" -eq 0 ]; then
  echo 'mux_client_request_session: session request failed: Session open refused by peer' >&2
  echo 'scp: Connection closed' >&2
  exit 1
fi
exit 0
`
	InstallFakeCommand(directory, "scp", script)
	ssh := SshTarget{Destination: "fake-host"}
	command := SCPDownload(&ssh, "remote-file", "local-file")
	command.Env["PATH"] = directory + ":" + os.Getenv("PATH")

	output, err := ProcessExecutor{}.Execute(&command)
	if err != nil {
		t.Fatalf("scp runs: %v", err)
	}
	if output.Status != 0 {
		t.Fatalf("status %d", output.Status)
	}
	attempts, err := os.ReadFile(filepath.Join(directory, "attempts"))
	if err != nil || strings.TrimSpace(string(attempts)) != "2" {
		t.Fatalf("attempts %q, %v", attempts, err)
	}
}

// A caller's sensitive stdin is part of the command, so each attempt sends
// it afresh.
func TestSensitiveStdinIsSentAgainOnEachRetry(t *testing.T) {
	fast := newFastRetries()
	defer fast.close()
	SetSSHConnectionSharingForTest(&SshSharingForTest{Kind: SshSharingForTestDisabled})
	defer SetSSHConnectionSharingForTest(nil)
	fake := newFakeSSH(t, []string{"refuse-session"})
	command := fake.command([]string{"cat"}).WithSensitiveStdin([]byte("secret\n"))
	output, err := ProcessExecutor{}.Execute(&command)
	if err != nil {
		t.Fatalf("ssh runs: %v", err)
	}
	if string(output.Stdout) != "secret\n" {
		t.Fatalf("stdout %q", output.Stdout)
	}
	if got := fake.attempts(t); got != 2 {
		t.Fatalf("%d attempts", got)
	}
}

// signalIgnoreSIGXFSZ keeps the test process alive when a write passes
// RLIMIT_FSIZE, so the write fails with EFBIG instead.
func signalIgnoreSIGXFSZ() {
	signal.Ignore(syscall.SIGXFSZ)
}
