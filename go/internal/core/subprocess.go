// Port of mj-core/src/subprocess.rs.

// Shared helpers for running child processes.
//
// Starting a child, writing all of its stdin and only then reading its
// output deadlocks once the child writes enough to stdout or stderr to fill
// the OS pipe buffer (commonly 64KB) before it has consumed all of stdin: the
// child blocks writing its output, so it stops reading stdin, so the parent's
// write blocks on the full stdin pipe, and neither side can make progress.
// RunWithInput avoids this by writing stdin from its own goroutine while the
// output pipes drain concurrently.
//
// Every call site runs children through these helpers instead of handling
// os/exec pipes itself.
//
// Rust's std::process::Command and tokio::process::Command are both
// *exec.Cmd here; an ExitStatus is the finished command's *os.ProcessState.
// Async helpers take a context.Context whose cancellation kills the
// command's process group, as dropping the Rust future does.
//
// The platform parts live in subprocess_unix.go, subprocess_linux.go,
// subprocess_darwin.go, subprocess_unix_other.go, subprocess_exitwatch.go
// and subprocess_windows.go.

package core

import (
	"bytes"
	"context"
	"errors"
	"fmt"
	"io"
	"log/slog"
	"os"
	"os/exec"
	"path/filepath"
	"strconv"
	"strings"
	"sync"
	"syscall"
	"time"
)

// preparationCancellationKey is the context key of the preparation
// cancellation token (Rust's task-local CANCELLATION).
type preparationCancellationKey struct{}

// WithPreparationCancellation gives all preparation commands run under the
// returned context the same owner: token. Blocking work receives
// PreparationCancellation(ctx) explicitly before being spawned. The caller
// cancels token when the preparation ends, as Rust's drop guard does.
func WithPreparationCancellation(ctx context.Context, token context.Context) context.Context {
	return context.WithValue(ctx, preparationCancellationKey{}, token)
}

// PreparationCancelled is the error of a preparation stopped because its
// cancellation token fired. It is a type, not a message, so callers that
// treat a failed probe or command as grounds to repair or remove something
// can tell "the probe said the thing is broken" from "the probe was cut
// short" and must propagate the latter.
type PreparationCancelled struct{}

func (PreparationCancelled) Error() string { return "preparation cancelled" }

// IsPreparationCancelled reports whether err, at any depth of its wrapped
// chain, is a cancelled preparation.
func IsPreparationCancelled(err error) bool {
	var cancelled PreparationCancelled
	return errors.As(err, &cancelled)
}

func scopedPreparationCancellation(ctx context.Context) (context.Context, bool) {
	token, ok := ctx.Value(preparationCancellationKey{}).(context.Context)
	return token, ok
}

// PreparationCancellation is the preparation token of ctx, or one that never
// fires.
func PreparationCancellation(ctx context.Context) context.Context {
	if token, ok := scopedPreparationCancellation(ctx); ok {
		return token
	}
	return context.Background()
}

// ExitWatch waits for a started child's exit without polling waitpid.
//
// wait4 on a multithreaded process scans the children of every thread, so a
// daemon with hundreds of threads paid for that on each 25 ms poll of every
// running command. On Linux a pidfd becomes readable once the child exits:
// the loop sleeps in poll on it and reaps only after it fires. Without a
// pidfd (other systems, or a kernel older than 5.3) every check reports a
// possible exit, so callers reap on each tick as before.
type ExitWatch struct {
	// pidfd is the child's pidfd on Linux, or -1.
	pidfd int
}

// ProcessGroupGuard owns a child-created process group through all of its
// I/O and cleanup. The group can outlive its leader, so reaping the child
// does not disarm it; Close (Rust's Drop) kills the group.
type ProcessGroupGuard struct {
	// pid is the group's leader, or 0 for none.
	pid int
}

// NewProcessGroupGuard guards the group led by pid; 0 (Rust's None), 1 and
// negative pids guard nothing.
func NewProcessGroupGuard(pid int) *ProcessGroupGuard {
	if pid <= 1 {
		pid = 0
	}
	return &ProcessGroupGuard{pid: pid}
}

// Kill kills the guarded group (Unix only).
func (g *ProcessGroupGuard) Kill() {
	if g.pid != 0 {
		killProcessGroup(g.pid)
	}
}

// Close is the guard's Drop: it kills the group.
func (g *ProcessGroupGuard) Close() { g.Kill() }

// SupervisedChild is a long-lived child whose supervisor owns termination
// and reaping. Close (Rust's Drop) requests cleanup; Terminate also waits
// for cleanup to finish.
type SupervisedChild struct {
	stopOnce sync.Once
	stop     chan struct{}
	// done is closed once the supervisor has reaped the child and set state
	// or failure.
	done    chan struct{}
	state   *os.ProcessState
	failure string
	stderrM sync.Mutex
	stderr  []byte
}

// SupervisedChildSpawn starts command with null stdin/stdout and
// concurrently captured stderr.
func SupervisedChildSpawn(command *exec.Cmd) (*SupervisedChild, error) {
	command.Stdin = nil
	command.Stdout = nil
	reader, writer, err := os.Pipe()
	if err != nil {
		return nil, fmt.Errorf("start supervised subprocess: %w", err)
	}
	command.Stderr = writer
	prepareOwnedGroup(command)
	err = command.Start()
	_ = writer.Close()
	if err != nil {
		_ = reader.Close()
		return nil, fmt.Errorf("start supervised subprocess: %w", err)
	}
	job, err := childJobAttach(command.Process)
	if err != nil {
		killAndReap(command)
		_ = reader.Close()
		return nil, err
	}
	group := NewProcessGroupGuard(command.Process.Pid)
	child := &SupervisedChild{stop: make(chan struct{}), done: make(chan struct{})}
	readDone := make(chan error, 1)
	go func() {
		defer reader.Close()
		chunk := make([]byte, 8192)
		for {
			count, err := reader.Read(chunk)
			if count > 0 {
				child.stderrM.Lock()
				child.stderr = append(child.stderr, chunk[:count]...)
				child.stderrM.Unlock()
			}
			if errors.Is(err, io.EOF) {
				readDone <- nil
				return
			}
			if err != nil {
				readDone <- err
				return
			}
		}
	}()
	waited := make(chan waitResult, 1)
	go func() { waited <- waitForExit(command) }()
	go func() {
		var outcome waitResult
		select {
		case outcome = <-waited:
			if outcome.err != nil {
				outcome.err = fmt.Errorf("wait for supervised subprocess: %w", outcome.err)
			}
		case <-child.stop:
			group.Kill()
			job.close()
			if err := command.Process.Kill(); err != nil {
				slog.Debug("supervised subprocess already exited during termination", "error", err)
			}
			outcome = <-waited
			if outcome.err != nil {
				outcome.err = fmt.Errorf("reap supervised subprocess: %w", outcome.err)
			}
		}
		group.Close()
		job.close()
		if err := <-readDone; err != nil && outcome.err == nil {
			outcome.err = fmt.Errorf("read supervised stderr: %w", err)
		}
		if outcome.err != nil {
			child.failure = outcome.err.Error()
			slog.Warn("supervised subprocess failed", "error", child.failure)
		} else {
			child.state = outcome.state
		}
		close(child.done)
	}()
	return child, nil
}

// Stderr is the diagnostics captured so far, including output received
// before exit.
func (c *SupervisedChild) Stderr() []byte {
	c.stderrM.Lock()
	defer c.stderrM.Unlock()
	return append([]byte(nil), c.stderr...)
}

// Wait observes exit without cancelling the child if this wait is
// cancelled.
func (c *SupervisedChild) Wait(ctx context.Context) (*os.ProcessState, error) {
	select {
	case <-c.done:
		if c.failure != "" {
			return nil, errors.New(c.failure)
		}
		return c.state, nil
	case <-ctx.Done():
		return nil, ctx.Err()
	}
}

// Terminate kills and reaps the child before returning.
func (c *SupervisedChild) Terminate(ctx context.Context) (*os.ProcessState, error) {
	c.Close()
	return c.Wait(ctx)
}

// Close requests cleanup without waiting for it (Rust's Drop).
func (c *SupervisedChild) Close() {
	c.stopOnce.Do(func() { close(c.stop) })
}

// ProcessBirthScript is a process birth identity, shared with remote
// teardown's POSIX shell. Linux uses boot identity and kernel start ticks;
// other Unix hosts use ps's process start timestamp in UTC and the C locale.
// Recording and checking must not depend on their callers' environments. A
// PID alone is not an identity.
const ProcessBirthScript = `mj_process_birth() {
    if [ -r "/proc/$1/stat" ] && [ -r /proc/sys/kernel/random/boot_id ]; then
        mj_stat=$(cat "/proc/$1/stat") || return 1
        mj_stat=${mj_stat##*) }
        mj_ticks=$(printf '%s\n' "$mj_stat" | awk '{print $20}') || return 1
        [ -n "$mj_ticks" ] || return 1
        mj_boot=$(cat /proc/sys/kernel/random/boot_id) || return 1
        printf '%s:%s\n' "$mj_boot" "$mj_ticks"
    else
        TZ=UTC LC_ALL=C ps -o lstart= -p "$1" | awk '{$1=$1; print}'
    fi
}
`

// ProcessBirthIdentity is the birth identity of process pid.
func ProcessBirthIdentity(pid uint32) (string, error) {
	output, err := RunWithInput(exec.Command("sh", "-c",
		ProcessBirthScript+"\nmj_process_birth \"$1\"", "mj-process-birth", strconv.FormatUint(uint64(pid), 10)), nil)
	if err != nil {
		return "", err
	}
	identity := strings.TrimSpace(string(output.Stdout))
	if !output.Status.Success() || identity == "" {
		return "", fmt.Errorf("process birth identity is unavailable for %d", pid)
	}
	return identity, nil
}

// ioDrainTimeout is how long a bounded command's output may keep arriving
// after its leader exits. A backgrounded descendant that inherited the pipes
// would otherwise hold a finished command until the whole-command timeout.
// The same bound as Codex's IO_DRAIN_TIMEOUT_MS. The owned process group is
// terminated after.
const ioDrainTimeout = 2 * time.Second

// ProcessOutput is a finished command's exit status and captured output
// (Rust's std::process::Output).
type ProcessOutput struct {
	Status *os.ProcessState
	Stdout []byte
	Stderr []byte
}

// waitResult is a reaped child's status, or why it could not be waited for.
type waitResult struct {
	state *os.ProcessState
	err   error
}

// waitForExit waits for command. A nonzero exit or a signal is a status, as
// Rust's ExitStatus is, not an error.
func waitForExit(command *exec.Cmd) waitResult {
	err := command.Wait()
	var exit *exec.ExitError
	if err == nil || errors.As(err, &exit) {
		return waitResult{state: command.ProcessState}
	}
	if command.ProcessState != nil {
		// The child was reaped, but copying its standard streams failed.
		return waitResult{state: command.ProcessState, err: err}
	}
	return waitResult{err: err}
}

// killAndReap kills a started command and waits for it.
func killAndReap(command *exec.Cmd) {
	_ = command.Process.Kill()
	_ = command.Wait()
}

// boundedOutput is pipe output owned by the caller: a reader abandoned at
// the drain deadline leaves what it read so far.
type boundedOutput struct {
	mu    sync.Mutex
	bytes []byte
}

func (o *boundedOutput) take() []byte {
	o.mu.Lock()
	defer o.mu.Unlock()
	taken := o.bytes
	o.bytes = nil
	if taken == nil {
		taken = []byte{}
	}
	return taken
}

// readBounded reads pipe into output until end of file, failing once the
// output passes max bytes.
func readBounded(pipe *os.File, output *boundedOutput, max int) error {
	chunk := make([]byte, 8192)
	for {
		count, err := pipe.Read(chunk)
		if count > 0 {
			output.mu.Lock()
			output.bytes = append(output.bytes, chunk[:count]...)
			over := len(output.bytes) > max
			output.mu.Unlock()
			if over {
				return fmt.Errorf("subprocess output exceeds %d bytes", max)
			}
		}
		if errors.Is(err, io.EOF) {
			return nil
		}
		if err != nil {
			return err
		}
	}
}

// outputPipes makes the stdout and stderr pipes of command, whose write ends
// go to the child and are closed here once it has started.
type outputPipes struct {
	stdoutRead, stdoutWrite *os.File
	stderrRead, stderrWrite *os.File
}

func newOutputPipes(command *exec.Cmd) (*outputPipes, error) {
	stdoutRead, stdoutWrite, err := os.Pipe()
	if err != nil {
		return nil, err
	}
	stderrRead, stderrWrite, err := os.Pipe()
	if err != nil {
		_ = stdoutRead.Close()
		_ = stdoutWrite.Close()
		return nil, err
	}
	command.Stdout, command.Stderr = stdoutWrite, stderrWrite
	return &outputPipes{stdoutRead, stdoutWrite, stderrRead, stderrWrite}, nil
}

func (p *outputPipes) closeWriteEnds() {
	_ = p.stdoutWrite.Close()
	_ = p.stderrWrite.Close()
}

// closeReadEnds closes our ends, which ends any reader still waiting on a
// descendant that holds the pipes.
func (p *outputPipes) closeReadEnds() {
	_ = p.stdoutRead.Close()
	_ = p.stderrRead.Close()
}

// readBoth drains both pipes concurrently; the result is the first failure,
// or nil once both reached end of file.
func (p *outputPipes) readBoth(stdout, stderr *boundedOutput, max int) <-chan error {
	results := make(chan error, 2)
	go func() { results <- readBounded(p.stdoutRead, stdout, max) }()
	go func() { results <- readBounded(p.stderrRead, stderr, max) }()
	done := make(chan error, 1)
	go func() {
		first := <-results
		if first != nil {
			done <- first
			return
		}
		done <- <-results
	}()
	return done
}

// Capture captures a finite command without an application-imposed deadline
// or size limit. Cancelling ctx kills its process group; output pipes drain
// concurrently.
func Capture(ctx context.Context, command *exec.Cmd) (ProcessOutput, error) {
	var stdout, stderr bytes.Buffer
	command.Stdin = nil
	command.Stdout, command.Stderr = &stdout, &stderr
	prepareOwnedGroup(command)
	if err := command.Start(); err != nil {
		return ProcessOutput{}, fmt.Errorf("start captured subprocess: %w", err)
	}
	job, err := childJobAttach(command.Process)
	if err != nil {
		killAndReap(command)
		return ProcessOutput{}, err
	}
	defer job.close()
	group := NewProcessGroupGuard(command.Process.Pid)
	defer group.Close()
	waited := make(chan waitResult, 1)
	go func() { waited <- waitForExit(command) }()
	select {
	case outcome := <-waited:
		if outcome.err != nil {
			return ProcessOutput{}, fmt.Errorf("capture subprocess output: %w", outcome.err)
		}
		return ProcessOutput{Status: outcome.state, Stdout: stdout.Bytes(), Stderr: stderr.Bytes()}, nil
	case <-ctx.Done():
		group.Kill()
		_ = command.Process.Kill()
		<-waited
		return ProcessOutput{}, ctx.Err()
	}
}

// RunToFileAsync streams combined output straight to a log without a
// deadline or pipe buffers. Cancelling ctx kills the process group, including
// package-manager children.
func RunToFileAsync(ctx context.Context, command *exec.Cmd, output *os.File) (*os.ProcessState, error) {
	command.Stdin = nil
	command.Stdout, command.Stderr = output, output
	prepareOwnedGroup(command)
	if err := command.Start(); err != nil {
		return nil, fmt.Errorf("start logged subprocess: %w", err)
	}
	job, err := childJobAttach(command.Process)
	if err != nil {
		killAndReap(command)
		return nil, err
	}
	defer job.close()
	group := NewProcessGroupGuard(command.Process.Pid)
	defer group.Close()
	waited := make(chan waitResult, 1)
	go func() { waited <- waitForExit(command) }()
	select {
	case outcome := <-waited:
		if outcome.err != nil {
			return nil, fmt.Errorf("wait for logged subprocess: %w", outcome.err)
		}
		return outcome.state, nil
	case <-ctx.Done():
		group.Kill()
		_ = command.Process.Kill()
		<-waited
		return nil, ctx.Err()
	}
}

// RunBounded captures a process with byte/time bounds, draining both pipes
// concurrently. It kills the process group before returning on overflow,
// timeout, or cancellation.
func RunBounded(ctx context.Context, command *exec.Cmd, maxBytes int, timeout time.Duration) (ProcessOutput, error) {
	return RunBoundedObserved(ctx, command, maxBytes, timeout, func(int) {})
}

// errSubprocessTimedOut stands for the elapsed deadline of a bounded command.
var errSubprocessTimedOut = errors.New("subprocess timed out")

// RunBoundedObserved is RunBounded that tells spawned the child's process ID
// as soon as it exists, before anything is read from it. A caller that must
// know which processes it started (to tell them from the work it observes)
// records the ID there.
func RunBoundedObserved(ctx context.Context, command *exec.Cmd, maxBytes int, timeout time.Duration, spawned func(pid int)) (ProcessOutput, error) {
	cancellation := PreparationCancellation(ctx)
	if cancellation.Err() != nil {
		return ProcessOutput{}, PreparationCancelled{}
	}
	command.Stdin = nil
	pipes, err := newOutputPipes(command)
	if err != nil {
		return ProcessOutput{}, fmt.Errorf("start bounded subprocess: %w", err)
	}
	prepareOwnedGroup(command)
	err = command.Start()
	pipes.closeWriteEnds()
	defer pipes.closeReadEnds()
	if err != nil {
		return ProcessOutput{}, fmt.Errorf("start bounded subprocess: %w", err)
	}
	spawned(command.Process.Pid)
	job, err := childJobAttach(command.Process)
	if err != nil {
		killAndReap(command)
		return ProcessOutput{}, err
	}
	group := NewProcessGroupGuard(command.Process.Pid)
	var stdout, stderr boundedOutput
	readers := pipes.readBoth(&stdout, &stderr, maxBytes)
	waited := make(chan waitResult, 1)
	go func() { waited <- waitForExit(command) }()
	deadline := time.NewTimer(timeout)
	defer deadline.Stop()

	var status *os.ProcessState
	var failure error
	reaped := false
	select {
	case err := <-readers:
		if err != nil {
			failure = err
			break
		}
		select {
		case outcome := <-waited:
			reaped = true
			status, failure = outcome.state, outcome.err
		case <-deadline.C:
			failure = errSubprocessTimedOut
		case <-cancellation.Done():
			failure = PreparationCancelled{}
		case <-ctx.Done():
			failure = ctx.Err()
		}
	case outcome := <-waited:
		// The command completes when its leader exits. A descendant that
		// keeps the pipes open only gets the drain bound.
		reaped = true
		status, failure = outcome.state, outcome.err
		if failure != nil {
			break
		}
		drain := time.NewTimer(ioDrainTimeout)
		select {
		case err := <-readers:
			failure = err
		case <-drain.C:
		case <-deadline.C:
			failure = errSubprocessTimedOut
		case <-cancellation.Done():
			failure = PreparationCancelled{}
		case <-ctx.Done():
			failure = ctx.Err()
		}
		drain.Stop()
	case <-deadline.C:
		failure = errSubprocessTimedOut
	case <-cancellation.Done():
		failure = PreparationCancelled{}
	case <-ctx.Done():
		failure = ctx.Err()
	}
	if failure == nil {
		// Completed pipes do not authorize a background descendant to keep
		// running after this bounded command releases ownership.
		group.Close()
		job.close()
		pipes.closeReadEnds()
		return ProcessOutput{Status: status, Stdout: stdout.take(), Stderr: stderr.take()}, nil
	}
	group.Close()
	job.close()
	if !reaped {
		if err := command.Process.Kill(); err != nil {
			slog.Debug("bounded subprocess already exited during termination", "error", err)
		}
		// The operation deadline stops execution, not cleanup ownership.
		// Callers may remove working files as soon as this returns.
		if outcome := <-waited; outcome.err != nil && outcome.state == nil {
			return ProcessOutput{}, fmt.Errorf("reap terminated bounded subprocess: %w", outcome.err)
		}
	}
	return ProcessOutput{}, failure
}

// RunBoundedWithInput captures a process with byte and time bounds while
// writing its complete stdin concurrently. This avoids filling either output
// pipe while a child is still consuming a large input.
func RunBoundedWithInput(ctx context.Context, command *exec.Cmd, input []byte, maxBytes int, timeout time.Duration) (ProcessOutput, error) {
	stdinRead, stdinWrite, err := os.Pipe()
	if err != nil {
		return ProcessOutput{}, fmt.Errorf("start bounded subprocess: %w", err)
	}
	command.Stdin = stdinRead
	pipes, err := newOutputPipes(command)
	if err != nil {
		_ = stdinRead.Close()
		_ = stdinWrite.Close()
		return ProcessOutput{}, fmt.Errorf("start bounded subprocess: %w", err)
	}
	prepareOwnedGroup(command)
	err = command.Start()
	_ = stdinRead.Close()
	pipes.closeWriteEnds()
	defer pipes.closeReadEnds()
	if err != nil {
		_ = stdinWrite.Close()
		return ProcessOutput{}, fmt.Errorf("start bounded subprocess: %w", err)
	}
	group := NewProcessGroupGuard(command.Process.Pid)
	var stdout, stderr boundedOutput
	readers := pipes.readBoth(&stdout, &stderr, maxBytes)
	written := make(chan error, 1)
	go func() {
		_, err := stdinWrite.Write(input)
		_ = stdinWrite.Close()
		if err != nil && isBrokenPipe(err) {
			err = nil
		}
		written <- err
	}()
	waited := make(chan waitResult, 1)
	go func() { waited <- waitForExit(command) }()
	deadline := time.NewTimer(timeout)
	defer deadline.Stop()

	var status *os.ProcessState
	var failure error
	reaped := false
	for pending := 3; pending > 0 && failure == nil; pending-- {
		select {
		case outcome := <-waited:
			reaped = true
			status, failure = outcome.state, outcome.err
		case err := <-written:
			failure = err
		case err := <-readers:
			failure = err
		case <-deadline.C:
			failure = errSubprocessTimedOut
		case <-ctx.Done():
			failure = ctx.Err()
		}
	}
	group.Close()
	if failure == nil {
		return ProcessOutput{Status: status, Stdout: stdout.take(), Stderr: stderr.take()}, nil
	}
	_ = stdinWrite.Close()
	if !reaped {
		if err := command.Process.Kill(); err != nil {
			slog.Debug("bounded subprocess already exited during termination", "error", err)
		}
		if outcome := <-waited; outcome.err != nil && outcome.state == nil {
			return ProcessOutput{}, fmt.Errorf("reap terminated bounded subprocess: %w", outcome.err)
		}
	}
	return ProcessOutput{}, failure
}

// SpawnDetached launches a long-lived background process with no inherited
// terminal streams, appending its output to logPath.
//
// The process is genuinely detached: on Unix it is a grandchild reparented
// to init, not a child of the caller, because the caller starts a daemon
// that outlives the spawner and uses the PID for liveness checks. The
// returned PID is the real process rather than an intermediate, and that
// process leads its own session and process group, so group termination
// works. Exec errors are reported before this returns; it is not an
// application-readiness barrier.
func SpawnDetached(command *exec.Cmd, logPath string) (uint32, error) {
	if parent := filepath.Dir(logPath); parent != "" {
		if err := os.MkdirAll(parent, 0o777); err != nil {
			return 0, fmt.Errorf("create detached process log directory %s: %w", parent, err)
		}
	}
	log, err := os.OpenFile(logPath, os.O_CREATE|os.O_APPEND|os.O_WRONLY, 0o600)
	if err != nil {
		return 0, fmt.Errorf("open detached process log %s: %w", logPath, err)
	}
	defer log.Close()
	command.Stdin = nil
	command.Stdout, command.Stderr = log, log
	return spawnDetachedPlatform(command)
}

// RaiseOpenFileLimit raises this process's soft open-file limit as far as
// the system allows, so it and every child it starts (children inherit the
// limit) can hold many descriptors. A launcher's soft limit is often 1024,
// which a process holding a few descriptors per child exhausts at a few
// hundred children.
//
// Call this first in every binary's main. A failure is reported on stderr
// and otherwise ignored: the limit stays what it was and startup continues.
func RaiseOpenFileLimit() {
	if err := raiseOpenFileLimitImpl(); err != nil {
		fmt.Fprintf(os.Stderr, "warning: could not raise the open file limit: %v\n", err)
	}
}

// RunWithInput runs command with input written to its stdin, returning the
// captured output.
//
// It writes input from a separate goroutine (closing stdin when the write
// finishes or fails) while stdout and stderr drain. A write failure is never
// silently discarded -- except a broken pipe, which just means the child
// exited (or closed stdin) before consuming all of input; then the child's
// real exit status and stderr are more useful to the caller than a generic
// I/O error, so the output is still returned.
func RunWithInput(command *exec.Cmd, input []byte) (ProcessOutput, error) {
	stdinRead, stdinWrite, err := os.Pipe()
	if err != nil {
		return ProcessOutput{}, fmt.Errorf("spawn child process: %w", err)
	}
	var stdout, stderr bytes.Buffer
	command.Stdin = stdinRead
	command.Stdout, command.Stderr = &stdout, &stderr
	err = command.Start()
	_ = stdinRead.Close()
	if err != nil {
		_ = stdinWrite.Close()
		return ProcessOutput{}, fmt.Errorf("spawn child process: %w", err)
	}
	written := make(chan error, 1)
	go func() {
		_, err := stdinWrite.Write(input)
		// Close stdin whether or not the write succeeded, so a child that is
		// blocked reading stdin (e.g. waiting for EOF) can proceed.
		_ = stdinWrite.Close()
		written <- err
	}()
	outcome := waitForExit(command)
	writeErr := <-written
	if outcome.err != nil {
		return ProcessOutput{}, fmt.Errorf("wait for child process: %w", outcome.err)
	}
	if writeErr != nil && !isBrokenPipe(writeErr) {
		// The child exited, or otherwise stopped reading, before we finished
		// writing: that is a broken pipe and reports the child's status.
		// Anything else is a real failure.
		return ProcessOutput{}, fmt.Errorf("write child process stdin: %w", writeErr)
	}
	return ProcessOutput{Status: outcome.state, Stdout: stdout.Bytes(), Stderr: stderr.Bytes()}, nil
}

// RunCapturingStdoutCancellable captures an interactive answer while
// retaining cancellation ownership of the command's group. Cancellation
// finishes cleanup before the caller returns.
func RunCapturingStdoutCancellable(ctx context.Context, command *exec.Cmd) (ProcessOutput, error) {
	cancellation := PreparationCancellation(ctx)
	if cancellation.Err() != nil {
		return ProcessOutput{}, PreparationCancelled{}
	}
	var stdout bytes.Buffer
	command.Stdin, command.Stdout, command.Stderr = os.Stdin, &stdout, os.Stderr
	prepareOwnedGroup(command)
	foreground, err := foregroundGroupPrepare(command)
	if err != nil {
		return ProcessOutput{}, err
	}
	if foreground != nil {
		defer foreground.Close()
	}
	if err := command.Start(); err != nil {
		return ProcessOutput{}, fmt.Errorf("spawn interactive command: %w", err)
	}
	job, err := childJobAttach(command.Process)
	if err != nil {
		killAndReap(command)
		return ProcessOutput{}, err
	}
	group := NewProcessGroupGuard(command.Process.Pid)
	waited := make(chan waitResult, 1)
	go func() { waited <- waitForExit(command) }()
	var outcome waitResult
	cancelled := false
	select {
	case outcome = <-waited:
	case <-cancellation.Done():
		outcome.err, cancelled = PreparationCancelled{}, true
	case <-ctx.Done():
		outcome.err, cancelled = ctx.Err(), true
	}
	group.Close()
	job.close()
	if cancelled {
		if err := command.Process.Kill(); err != nil && !errors.Is(err, os.ErrProcessDone) {
			<-waited
			return ProcessOutput{}, fmt.Errorf("terminate interactive command: %w", err)
		}
		if reaped := <-waited; reaped.err != nil && reaped.state == nil {
			return ProcessOutput{}, fmt.Errorf("reap interactive command: %w", reaped.err)
		}
		return ProcessOutput{}, outcome.err
	}
	if outcome.err != nil {
		return ProcessOutput{}, outcome.err
	}
	return ProcessOutput{Status: outcome.state, Stdout: stdout.Bytes(), Stderr: []byte{}}, nil
}

// RunCapturingStdout runs a foreground child that talks to the terminal but
// whose stdout the caller needs to read.
//
// An interactive login prints its prompts and progress on stderr and its one
// machine-readable answer on stdout. Inheriting stdin and stderr keeps the
// prompts and any typed reply on the real terminal, while stdout is captured.
func RunCapturingStdout(command *exec.Cmd) (ProcessOutput, error) {
	var stdout bytes.Buffer
	command.Stdin, command.Stdout, command.Stderr = os.Stdin, &stdout, os.Stderr
	if err := command.Start(); err != nil {
		return ProcessOutput{}, fmt.Errorf("spawn child process: %w", err)
	}
	outcome := waitForExit(command)
	if outcome.err != nil {
		return ProcessOutput{}, fmt.Errorf("wait for child process: %w", outcome.err)
	}
	return ProcessOutput{Status: outcome.state, Stdout: stdout.Bytes(), Stderr: []byte{}}, nil
}

// RunInherited runs a foreground child with no stdin and with its output
// inherited.
func RunInherited(command *exec.Cmd) (*os.ProcessState, error) {
	command.Stdin, command.Stdout, command.Stderr = nil, os.Stdout, os.Stderr
	return runForStatus(command, "run child process")
}

// RunToFile runs a noninteractive child with stdout sent directly to a file.
// No payload pipe or in-memory output buffer is created.
func RunToFile(command *exec.Cmd, output *os.File) (*os.ProcessState, error) {
	command.Stdin, command.Stdout, command.Stderr = nil, output, os.Stderr
	return runForStatus(command, "run child process with file output")
}

// RunInteractive transfers the terminal to a foreground child, including its
// input. Used for process replacement on platforms without exec; the caller
// must have released its own terminal event reader before waiting here.
func RunInteractive(command *exec.Cmd) (*os.ProcessState, error) {
	command.Stdin, command.Stdout, command.Stderr = os.Stdin, os.Stdout, os.Stderr
	return runForStatus(command, "run interactive child process")
}

// runForStatus is Rust's Command::status with context.
func runForStatus(command *exec.Cmd, context string) (*os.ProcessState, error) {
	if err := command.Start(); err != nil {
		return nil, fmt.Errorf("%s: %w", context, err)
	}
	outcome := waitForExit(command)
	if outcome.err != nil {
		return nil, fmt.Errorf("%s: %w", context, outcome.err)
	}
	return outcome.state, nil
}

// TerminateProcessGroup signals a whole process group. Terminals reuse this
// so process-group termination lives in one place. A group that is already
// gone counts as success; anything else is logged rather than dropped.
func TerminateProcessGroup(pid int, signal syscall.Signal) {
	if err := SignalProcessGroup(pid, signal); err != nil {
		slog.Warn("could not signal process group", "pid", pid, "signal", int(signal), "error", err)
	}
}
