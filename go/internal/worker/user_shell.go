// Port of mj-worker/src/user_shell.rs.

// Supervised user-authored shell commands run by the target worker.

package worker

import (
	"context"
	"errors"
	"fmt"
	"io"
	"log/slog"
	"maps"
	"os"
	"os/exec"
	"path/filepath"
	"slices"
	"strings"
	"sync"
	"syscall"
	"time"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

const MaxConcurrentUserShells = 4

const UserShellTimeout = 60 * time.Minute

const userShellOutputHeadBytes = 16 * 1024

const userShellOutputTailBytes = 16 * 1024

const userShellLiveOutputBytes = 16 * 1024

const userShellReadChunkBytes = 8 * 1024

// userShellDrainTimeout is how long output may keep arriving after the shell
// exits. A backgrounded descendant such as `(sleep 300 &)` inherits the pipes
// and keeps them open; without a bound the entry would read "running" for the
// descendant's whole life although the command finished. The same bound as
// Codex's `IO_DRAIN_TIMEOUT_MS`.
const userShellDrainTimeout = 2 * time.Second

type UserShellSpec struct {
	Command     string
	Cwd         string
	Environment map[string]string
}

// userShellCancellation is one running shell's cancellation slot (Rust's
// Option<oneshot::Sender<()>>).
type userShellCancellation struct {
	// requested is set once a cancellation was sent (Rust took the sender).
	requested bool
	// cancel is closed to cancel the shell.
	cancel chan struct{}
	// finished is closed once the supervisor stopped listening for
	// cancellation (Rust's dropped oneshot receiver).
	finished <-chan struct{}
}

// UserShellRegistry runs the session's `!` shell commands. It is owned by
// one coordinator and is not safe for concurrent use.
type UserShellRegistry struct {
	cwd           string
	environment   map[string]string
	events        chan<- core.RuntimeEvent
	cancellations map[string]*userShellCancellation
	// tasks are the supervisors still to be joined, by request id.
	tasks map[string]*core.JoinHandle[struct{}]
	// lifetimes counts every supervisor until its process cleanup is done
	// (Rust's TaskTracker).
	lifetimes *sync.WaitGroup
	// stopped is closed by Close. Rust's coordinator owns both this registry
	// and the event receiver, so they go away together; a supervisor whose
	// result can no longer be delivered gives it up when stopped closes, as
	// Rust's send fails once the receiver is dropped.
	stopped   chan struct{}
	closeOnce sync.Once
}

// UserShellCancelOutcome is what a cancellation request found.
type UserShellCancelOutcome string

const (
	UserShellCancelOutcomeRequested        UserShellCancelOutcome = "Requested"
	UserShellCancelOutcomeAlreadyRequested UserShellCancelOutcome = "AlreadyRequested"
	UserShellCancelOutcomeNotRunning       UserShellCancelOutcome = "NotRunning"
)

func NewUserShellRegistry(cwd string, environment map[string]string, events chan<- core.RuntimeEvent) *UserShellRegistry {
	return &UserShellRegistry{
		cwd:           cwd,
		environment:   environment,
		events:        events,
		cancellations: map[string]*userShellCancellation{},
		tasks:         map[string]*core.JoinHandle[struct{}]{},
		lifetimes:     &sync.WaitGroup{},
		stopped:       make(chan struct{}),
	}
}

func (r *UserShellRegistry) AvailableSlots() int {
	return max(MaxConcurrentUserShells-len(r.cancellations), 0)
}

// completionTracker counts every shell this registry started until its
// process cleanup is done. Wait on it after dropping the registry to wait
// for cancelled shells' process cleanup, even if the coordinator failed.
func (r *UserShellRegistry) completionTracker() *sync.WaitGroup {
	return r.lifetimes
}

func (r *UserShellRegistry) Start(requestID string, command string) error {
	spec := UserShellSpec{
		Command:     command,
		Cwd:         r.cwd,
		Environment: maps.Clone(r.environment),
	}
	cancel := make(chan struct{})
	finished := make(chan struct{})
	task, err := spawnUserShell(requestID, spec, cancel, finished, r.events, r.stopped, r.lifetimes)
	if err != nil {
		slog.Warn("could not start user shell",
			"request_id", requestID, "operation", "user_shell_start", "error", err)
		return err
	}
	r.cancellations[requestID] = &userShellCancellation{cancel: cancel, finished: finished}
	r.tasks[requestID] = core.Spawn(func() struct{} {
		if _, err := task.Join(context.Background()); err != nil {
			slog.Error("user shell task stopped", "request_id", requestID, "error", err)
		}
		return struct{}{}
	})
	return nil
}

func (r *UserShellRegistry) Cancel(requestID string) UserShellCancelOutcome {
	cancellation, ok := r.cancellations[requestID]
	if !ok {
		return UserShellCancelOutcomeNotRunning
	}
	if cancellation.requested {
		return UserShellCancelOutcomeAlreadyRequested
	}
	cancellation.requested = true
	select {
	case <-cancellation.finished:
		// The child already finished and its terminal event is waiting to
		// be folded. Interrupting its durable dispatch here would make that
		// legitimate completion look like a duplicate.
		slog.Debug("user shell cancellation receiver was already closed",
			"request_id", requestID, "operation", "user_shell_cancel")
		return UserShellCancelOutcomeAlreadyRequested
	default:
	}
	close(cancellation.cancel)
	return UserShellCancelOutcomeRequested
}

func (r *UserShellRegistry) Completed(requestID string) {
	delete(r.cancellations, requestID)
	for id, task := range r.tasks {
		select {
		case <-task.Done():
		default:
			continue
		}
		delete(r.tasks, id)
		if _, err := task.Join(context.Background()); err != nil {
			slog.Error("user shell supervisor failed", "error", err)
		}
	}
}

// Close is the registry's Drop, for the coordinator to call when it stops
// reading events: every shell it still tracks is cancelled, as dropping
// Rust's cancellation senders cancels them, and results that can no longer
// be delivered are given up. Wait on completionTracker afterwards for the
// shells' process cleanup.
func (r *UserShellRegistry) Close() {
	r.closeOnce.Do(func() { close(r.stopped) })
	for requestID, cancellation := range r.cancellations {
		if !cancellation.requested {
			cancellation.requested = true
			close(cancellation.cancel)
		}
		delete(r.cancellations, requestID)
	}
}

// spawnUserShell starts spec's command and a supervisor that reports its
// output and result on events. lifetimes counts the supervisor until it has
// stopped the shell's process group. The supervisor closes finished once it
// no longer listens for cancellation (Rust's dropped oneshot receiver), and
// gives its result up once stopped closes (Rust's dropped event receiver).
func spawnUserShell(
	requestID string,
	spec UserShellSpec,
	cancelled <-chan struct{},
	finished chan<- struct{},
	events chan<- core.RuntimeEvent,
	stopped <-chan struct{},
	lifetimes *sync.WaitGroup,
) (*core.JoinHandle[struct{}], error) {
	shell, err := userShellProgram(spec.Environment)
	if err != nil {
		return nil, err
	}
	environment := maps.Clone(spec.Environment)
	delete(environment, "GH_TOKEN")
	delete(environment, "GITHUB_TOKEN")
	command := childCommand(shell, []string{"-c", githubCLILoginShellCommand(spec.Command)}, environment)
	command.Dir = spec.Cwd
	setOwnProcessGroup(command)
	stdout, stderr, err := startWithOutputPipes(command)
	if err != nil {
		return nil, fmt.Errorf("start shell %s: %w", shell, err)
	}
	group := core.NewProcessGroupGuard(command.Process.Pid)
	lifetimes.Add(1)
	return core.Spawn(func() struct{} {
		defer lifetimes.Done()
		defer close(finished)
		superviseUserShell(requestID, spec.Command, command, group, stdout, stderr, cancelled, events, stopped)
		return struct{}{}
	}), nil
}

// superviseUserShell waits for the shell, drains its output for at most
// userShellDrainTimeout after the shell exits, stops whatever it left in its
// process group, and reports the result.
func superviseUserShell(
	requestID string,
	commandText string,
	child *exec.Cmd,
	group *core.ProcessGroupGuard,
	stdoutPipe *os.File,
	stderrPipe *os.File,
	cancelled <-chan struct{},
	events chan<- core.RuntimeEvent,
	stopped <-chan struct{},
) {
	started := time.Now()
	stdoutBuffer := &userShellOutput{}
	stderrBuffer := &userShellOutput{}
	// The readers are goroutines of their own so a descendant that keeps a
	// pipe open cannot hold this supervisor: closing our end of a pipe ends
	// its reader.
	type readerResult struct {
		name string
		err  error
	}
	readers := make(chan readerResult, 2)
	pipes := []struct {
		name     string
		isStdout bool
		pipe     *os.File
	}{{"stdout", true, stdoutPipe}, {"stderr", false, stderrPipe}}
	for _, pipe := range pipes {
		go func() {
			err := userShellDrainPipe(pipe.pipe, requestID, commandText, stdoutBuffer, stderrBuffer, pipe.isStdout, events)
			readers <- readerResult{name: pipe.name, err: err}
		}()
	}

	waited := make(chan error, 1)
	go func() { waited <- child.Wait() }()
	timeout := time.NewTimer(UserShellTimeout)
	defer timeout.Stop()
	timedOut := false
	wasCancelled := false
	// The command completes when the shell (the leader) exits, not when every
	// descendant closes the pipes it inherited.
	var waitErr error
	select {
	case waitErr = <-waited:
	case <-cancelled:
		wasCancelled = true
		group.Kill()
		waitErr = <-waited
	case <-timeout.C:
		timedOut = true
		group.Kill()
		waitErr = <-waited
	}
	// The entry's duration is the leader's, not that of a descendant.
	durationMs := uint64(time.Since(started).Milliseconds())

	// Collect what is still readable, for at most userShellDrainTimeout.
	// Cancellation stays live: it ends the drain by killing the group.
	var readErrors []string
	drainDeadline := time.NewTimer(userShellDrainTimeout)
	defer drainDeadline.Stop()
	listening := cancelled
	if wasCancelled || timedOut {
		listening = nil
	}
	running := len(pipes)
drain:
	for running > 0 {
		select {
		case result := <-readers:
			running--
			if result.err != nil {
				readErrors = append(readErrors, fmt.Sprintf("read %s: %v", result.name, result.err))
			}
		case <-drainDeadline.C:
			break drain
		case <-listening:
			wasCancelled = true
			listening = nil
			group.Kill()
		}
	}
	// A descendant that outlives the drain keeps its pipes; stop reading
	// them. The buffers already hold everything read so far.
	stdoutPipe.Close()
	stderrPipe.Close()
	for ; running > 0; running-- {
		<-readers
	}
	// Terminate whatever the shell left behind in its process group.
	group.Close()

	stdout, stdoutTruncated := stdoutBuffer.finalText()
	stderr, stderrTruncated := stderrBuffer.finalText()

	var exitCode *int32
	var signal *string
	if child.ProcessState != nil {
		var waitError *string
		exitCode, signal, waitError = userShellStatusParts(child.ProcessState)
		if waitError != nil {
			readErrors = append(readErrors, *waitError)
		}
	} else {
		readErrors = append(readErrors, fmt.Sprintf("wait for user shell: %v", waitErr))
	}
	var status core.UserShellStatus
	switch {
	case len(readErrors) > 0:
		status = core.UserShellStatusFailed
	case timedOut:
		status = core.UserShellStatusTimedOut
	case wasCancelled:
		status = core.UserShellStatusCancelled
	case signal != nil:
		status = core.UserShellStatusSignaled
	default:
		status = core.UserShellStatusExited
	}
	result := core.UserShellResult{
		Command:         commandText,
		Stdout:          stdout,
		Stderr:          stderr,
		StdoutTruncated: stdoutTruncated,
		StderrTruncated: stderrTruncated,
		ExitCode:        exitCode,
		Signal:          signal,
		DurationMs:      durationMs,
		Status:          status,
	}
	if len(readErrors) > 0 {
		joined := strings.Join(readErrors, "; ")
		result.Error = &joined
	}
	select {
	case events <- core.RuntimeEventUserShellFinished{RequestID: requestID, Result: result}:
	case <-stopped:
		slog.Warn("user shell result was lost because the relay coordinator stopped",
			"request_id", requestID, "operation", "user_shell_finished")
	}
}

// userShellDrainPipe reads one of the shell's pipes into its buffer and
// publishes the live output after every read, dropping an update the relay
// has no room for.
func userShellDrainPipe(
	pipe io.Reader,
	requestID string,
	command string,
	stdout *userShellOutput,
	stderr *userShellOutput,
	isStdout bool,
	events chan<- core.RuntimeEvent,
) error {
	chunk := make([]byte, userShellReadChunkBytes)
	for {
		read, err := pipe.Read(chunk)
		if read > 0 {
			target := stderr
			if isStdout {
				target = stdout
			}
			target.append(chunk[:read])
			stdoutText, stdoutTruncated := stdout.liveText()
			stderrText, stderrTruncated := stderr.liveText()
			select {
			case events <- core.RuntimeEventUserShellOutput{
				RequestID:       requestID,
				Command:         command,
				Stdout:          stdoutText,
				Stderr:          stderrText,
				StdoutTruncated: stdoutTruncated,
				StderrTruncated: stderrTruncated,
			}:
			default:
			}
		}
		if errors.Is(err, io.EOF) {
			return nil
		}
		if err != nil {
			return fmt.Errorf("read user shell pipe: %w", err)
		}
	}
}

func (o *userShellOutput) append(data []byte) {
	o.mu.Lock()
	defer o.mu.Unlock()
	o.buffer.append(data)
}

func (o *userShellOutput) liveText() (string, bool) {
	o.mu.Lock()
	defer o.mu.Unlock()
	return o.buffer.liveText()
}

func (o *userShellOutput) finalText() (string, bool) {
	o.mu.Lock()
	defer o.mu.Unlock()
	return o.buffer.finalText()
}

// userShellOutput is one stream's buffer shared by its reader and the
// supervisor (Rust's Arc<Mutex<HeadTailBuffer>>).
type userShellOutput struct {
	mu     sync.Mutex
	buffer headTailBuffer
}

// headTailBuffer keeps the first and last bytes of a stream and counts the
// rest.
type headTailBuffer struct {
	head []byte
	// tail is at most userShellOutputTailBytes, oldest first.
	tail  []byte
	total int
}

func (b *headTailBuffer) append(data []byte) {
	b.total += len(data)
	headBytes := min(max(userShellOutputHeadBytes-len(b.head), 0), len(data))
	b.head = append(b.head, data[:headBytes]...)
	b.tail = append(b.tail, data[headBytes:]...)
	if excess := len(b.tail) - userShellOutputTailBytes; excess > 0 {
		b.tail = append(b.tail[:0], b.tail[excess:]...)
	}
}

func (b *headTailBuffer) liveText() (string, bool) {
	visible := b.head[:min(len(b.head), userShellLiveOutputBytes)]
	return fromUTF8Lossy(visible), b.total > userShellLiveOutputBytes
}

func (b *headTailBuffer) finalText() (string, bool) {
	truncated := b.total > userShellOutputHeadBytes+userShellOutputTailBytes
	text := slices.Clone(b.head)
	if truncated {
		dropped := b.total - (userShellOutputHeadBytes + userShellOutputTailBytes)
		text = fmt.Appendf(text, "\n[mj dropped %d middle bytes]\n", dropped)
	}
	text = append(text, b.tail...)
	return fromUTF8Lossy(text), truncated
}

// userShellProgram is the shell that runs a `!` command: the session's
// `$SHELL` when it names a POSIX-compatible shell by absolute path,
// otherwise `/bin/sh`. The path is absolute so the child's own `PATH` (which
// a profile may point anywhere) is never searched for it.
func userShellProgram(environment map[string]string) (string, error) {
	posixShells := []string{"sh", "bash", "zsh", "dash", "ksh", "mksh", "ash"}
	shell := "/bin/sh"
	if candidate, ok := environment["SHELL"]; ok && filepath.IsAbs(candidate) &&
		slices.Contains(posixShells, filepath.Base(candidate)) {
		shell = candidate
	}
	if info, err := os.Stat(shell); err != nil || !info.Mode().IsRegular() {
		return "", fmt.Errorf("shell %s not found", shell)
	}
	return shell, nil
}

// userShellStatusParts is the exit code, the signal name and a wait error of
// a finished shell. Only Unix reports signals.
func userShellStatusParts(state *os.ProcessState) (exitCode *int32, signal *string, waitError *string) {
	if code := state.ExitCode(); code >= 0 {
		// Rust reads a Windows exit code as an i32, wrapping past i32's range.
		value := int32(code)
		exitCode = &value
	}
	if status, ok := state.Sys().(syscall.WaitStatus); ok && status.Signaled() {
		name := fmt.Sprintf("SIG%d", int(status.Signal()))
		signal = &name
	}
	return exitCode, signal, nil
}
