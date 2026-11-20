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
// command's process group.

package core

import (
	"context"
	"errors"
	"fmt"
	"os"
	"os/exec"
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

// NewExitWatch must be called before the child is reaped, so the pid cannot
// be reused.
func NewExitWatch(process *os.Process) *ExitWatch {
	panic("unported: mj-core/src/subprocess.rs ExitWatch::new")
}

// Wait sleeps up to timeout, returning early once the child exits. Returns
// whether the child may have exited.
func (w *ExitWatch) Wait(timeout time.Duration) bool {
	panic("unported: mj-core/src/subprocess.rs ExitWatch::wait")
}

// Close releases the pidfd.
func (w *ExitWatch) Close() {
	panic("unported: mj-core/src/subprocess.rs ExitWatch drop")
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
	panic("unported: mj-core/src/subprocess.rs ProcessGroupGuard::kill")
}

// Close is the guard's Drop: it kills the group.
func (g *ProcessGroupGuard) Close() { g.Kill() }

// childJob is a Windows job object that kills its processes when closed.
type childJob struct {
	handle uintptr
}

func childJobAttach(process *os.Process) (*childJob, error) {
	panic("unported: mj-core/src/subprocess.rs ChildJob::attach")
}

// close is the job's Drop.
func (j *childJob) close() {
	panic("unported: mj-core/src/subprocess.rs ChildJob drop")
}

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
	panic("unported: mj-core/src/subprocess.rs SupervisedChild::spawn")
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

// Capture captures a finite command without an application-imposed deadline
// or size limit. Cancelling ctx kills its process group; output pipes drain
// concurrently.
func Capture(ctx context.Context, command *exec.Cmd) (ProcessOutput, error) {
	panic("unported: mj-core/src/subprocess.rs capture")
}

// RunToFileAsync streams combined output straight to a log without a
// deadline or pipe buffers. Cancelling ctx kills the process group, including
// package-manager children.
func RunToFileAsync(ctx context.Context, command *exec.Cmd, output *os.File) (*os.ProcessState, error) {
	panic("unported: mj-core/src/subprocess.rs run_to_file_async")
}

// RunBounded captures a process with byte/time bounds, draining both pipes
// concurrently. It kills the process group before returning on overflow,
// timeout, or cancellation.
func RunBounded(ctx context.Context, command *exec.Cmd, maxBytes int, timeout time.Duration) (ProcessOutput, error) {
	return RunBoundedObserved(ctx, command, maxBytes, timeout, func(int) {})
}

// RunBoundedObserved is RunBounded that tells spawned the child's process ID
// as soon as it exists, before anything is read from it. A caller that must
// know which processes it started (to tell them from the work it observes)
// records the ID there.
func RunBoundedObserved(ctx context.Context, command *exec.Cmd, maxBytes int, timeout time.Duration, spawned func(pid int)) (ProcessOutput, error) {
	panic("unported: mj-core/src/subprocess.rs run_bounded_observed")
}

// RunBoundedWithInput captures a process with byte and time bounds while
// writing its complete stdin concurrently. This avoids filling either output
// pipe while a child is still consuming a large input.
func RunBoundedWithInput(ctx context.Context, command *exec.Cmd, input []byte, maxBytes int, timeout time.Duration) (ProcessOutput, error) {
	panic("unported: mj-core/src/subprocess.rs run_bounded_with_input")
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
	panic("unported: mj-core/src/subprocess.rs spawn_detached")
}

// disinheritStandardHandles keeps this process's standard handles out of
// every child that is not handed them explicitly (Windows only). A detached
// child that kept the launcher's stdout pipe would hold it open for its
// whole life.
func disinheritStandardHandles() {
	panic("unported: mj-core/src/subprocess.rs disinherit_standard_handles")
}

// spawnDetachedUnix double-forks command and reports the grandchild's PID.
func spawnDetachedUnix(command *exec.Cmd) (uint32, error) {
	panic("unported: mj-core/src/subprocess.rs spawn_detached_unix")
}

// inheritedDescriptorLimit is one past the highest descriptor this process
// can hold, for the last-resort per-fd loop in
// markInheritedDescriptorsCloexec.
func inheritedDescriptorLimit() int {
	panic("unported: mj-core/src/subprocess.rs inherited_descriptor_limit")
}

// markInheritedDescriptorsCloexec sets FD_CLOEXEC on every descriptor from 3
// upward, so a detached process keeps nothing of its launcher beyond its
// stdio.
func markInheritedDescriptorsCloexec(limit int) {
	panic("unported: mj-core/src/subprocess.rs mark_inherited_descriptors_cloexec")
}

// markRangeCloexec marks the whole range in one close_range call (Linux
// 5.11+).
func markRangeCloexec() bool {
	panic("unported: mj-core/src/subprocess.rs mark_range_cloexec")
}

// markListedDescriptorsCloexec marks the descriptors the kernel lists for
// this process, and only those. Returns false when the listing was
// unavailable or incomplete, so the caller must scan by number instead.
func markListedDescriptorsCloexec() bool {
	panic("unported: mj-core/src/subprocess.rs mark_listed_descriptors_cloexec")
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

func raiseOpenFileLimitImpl() error {
	panic("unported: mj-core/src/subprocess.rs raise_open_file_limit_impl")
}

// openFileLimitTarget is the soft limit to ask for given the hard limit.
// macOS reports an unlimited hard limit but refuses a soft limit above
// kern.maxfilesperproc. Linux refuses anything above fs.nr_open, which is
// what an unlimited hard limit really means there.
func openFileLimitTarget(hard uint64) (uint64, error) {
	panic("unported: mj-core/src/subprocess.rs open_file_limit_target")
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
	panic("unported: mj-core/src/subprocess.rs run_with_input")
}

// ForegroundGroup hands the login terminal's foreground to a child's process
// group and gives it back on Close (Rust's Drop). Unix only.
type ForegroundGroup struct {
	tty      *os.File
	previous int
}

// foregroundGroup makes group the foreground process group of terminal fd,
// with SIGTTOU blocked around the call.
func foregroundGroup(fd uintptr, group int) error {
	panic("unported: mj-core/src/subprocess.rs foreground_group")
}

// foregroundGroupPrepare arranges for command's process group to take the
// login terminal's foreground; nil when there is no controlling terminal.
func foregroundGroupPrepare(command *exec.Cmd) (*ForegroundGroup, error) {
	panic("unported: mj-core/src/subprocess.rs ForegroundGroup::prepare")
}

// Close restores the previous foreground group.
func (g *ForegroundGroup) Close() {
	panic("unported: mj-core/src/subprocess.rs ForegroundGroup drop")
}

// RunCapturingStdoutCancellable captures an interactive answer while
// retaining cancellation ownership of the command's group. Cancellation
// finishes cleanup before the caller returns.
func RunCapturingStdoutCancellable(ctx context.Context, command *exec.Cmd) (ProcessOutput, error) {
	panic("unported: mj-core/src/subprocess.rs run_capturing_stdout_cancellable")
}

// RunCapturingStdout runs a foreground child that talks to the terminal but
// whose stdout the caller needs to read.
//
// An interactive login prints its prompts and progress on stderr and its one
// machine-readable answer on stdout. Inheriting stdin and stderr keeps the
// prompts and any typed reply on the real terminal, while stdout is captured.
func RunCapturingStdout(command *exec.Cmd) (ProcessOutput, error) {
	panic("unported: mj-core/src/subprocess.rs run_capturing_stdout")
}

// RunInherited runs a foreground child with no stdin and with its output
// inherited.
func RunInherited(command *exec.Cmd) (*os.ProcessState, error) {
	panic("unported: mj-core/src/subprocess.rs run_inherited")
}

// RunToFile runs a noninteractive child with stdout sent directly to a file.
// No payload pipe or in-memory output buffer is created.
func RunToFile(command *exec.Cmd, output *os.File) (*os.ProcessState, error) {
	panic("unported: mj-core/src/subprocess.rs run_to_file")
}

// RunInteractive transfers the terminal to a foreground child, including its
// input. Used for process replacement on platforms without exec; the caller
// must have released its own terminal event reader before waiting here.
func RunInteractive(command *exec.Cmd) (*os.ProcessState, error) {
	panic("unported: mj-core/src/subprocess.rs run_interactive")
}

// SignalProcessGroup sends signal to the process group led by pid (Unix
// only).
//
// Every caller signals a group it created for its own child, and wants that
// group gone. ESRCH means it already is, so it reports success rather than a
// failure. Darwin excludes zombies while counting signalable members of a
// group and returns EPERM once that count reaches zero, which for a group we
// own likewise means only exiting descendants remain. Any other error is a
// real teardown failure and is returned so the caller can report it.
func SignalProcessGroup(pid int, signal syscall.Signal) error {
	panic("unported: mj-core/src/subprocess.rs signal_process_group")
}

// TerminateProcessGroup signals a whole process group. Terminals reuse this
// so process-group termination lives in one place. A group that is already
// gone counts as success; anything else is logged rather than dropped.
func TerminateProcessGroup(pid int, signal syscall.Signal) {
	panic("unported: mj-core/src/subprocess.rs terminate_process_group")
}

func groupSignalErrorIsIgnorable(err error) bool {
	panic("unported: mj-core/src/subprocess.rs group_signal_error_is_ignorable")
}
