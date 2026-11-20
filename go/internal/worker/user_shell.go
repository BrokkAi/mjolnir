// Port of mj-worker/src/user_shell.rs.

// Supervised user-authored shell commands run by the target worker.

package worker

import (
	"fmt"
	"io"
	"os"
	"path/filepath"
	"slices"
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
	panic("unported: mj-worker/src/user_shell.rs UserShellRegistry::start")
}

func (r *UserShellRegistry) Cancel(requestID string) UserShellCancelOutcome {
	panic("unported: mj-worker/src/user_shell.rs UserShellRegistry::cancel")
}

func (r *UserShellRegistry) Completed(requestID string) {
	panic("unported: mj-worker/src/user_shell.rs UserShellRegistry::completed")
}

// spawnUserShell starts spec's command and a supervisor that reports its
// output and result on events. lifetimes counts the supervisor until it has
// stopped the shell's process group.
func spawnUserShell(
	requestID string,
	spec UserShellSpec,
	cancelled <-chan struct{},
	events chan<- core.RuntimeEvent,
	lifetimes *sync.WaitGroup,
) (*core.JoinHandle[struct{}], error) {
	panic("unported: mj-worker/src/user_shell.rs spawn_user_shell")
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
	panic("unported: mj-worker/src/user_shell.rs drain_pipe")
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
