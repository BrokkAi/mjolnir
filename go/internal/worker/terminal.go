// Port of mj-worker/src/terminal.rs.

// Terminals Hel runs on behalf of an ACP agent.
//
// An agent that asks for `terminal/create` gets a shell command executed in
// the worker process — inside the container for containerized targets — with
// its output streamed into a capped buffer and its exit status reported back.
// Every terminal owns a process group, so one signal reaches the descendants
// that inherited its pipes.

package worker

import (
	"context"
	"crypto/rand"
	"errors"
	"fmt"
	"io"
	"os/exec"
	"path/filepath"
	"slices"
	"strings"
	"sync"
	"sync/atomic"
	"time"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// DefaultTerminalOutputBytes is the output retained per terminal when the
// agent names no limit. Kimi asks for exactly this much; the constant covers
// the agents that ask for nothing.
const DefaultTerminalOutputBytes = 4 * 1024 * 1024

// terminalBufferSlackBytes is how far a buffer may grow past its limit
// before it drops from the front. Trimming on every append would move the
// whole retained tail per read.
const terminalBufferSlackBytes = 64 * 1024

// terminalReadChunkBytes is the read size for a terminal's pipes.
const terminalReadChunkBytes = 16 * 1024

// terminalReapTimeout is how long connection teardown waits for one
// supervisor to reap its child after the process group was killed.
const terminalReapTimeout = 5 * time.Second

// TerminalSpawn is what to run for one `terminal/create`, already resolved
// against the session: no ACP types survive this boundary.
type TerminalSpawn struct {
	// Command is the command string exactly as the agent sent it.
	Command string
	// Args are the arguments the agent sent, if any.
	Args []string
	// Env is the variables to overlay on the daemon environment, as
	// name/value pairs in the agent's order.
	Env             [][2]string
	Cwd             string
	OutputByteLimit int
}

// TerminalExit is how a terminal's child ended, in ACP's terms. The zero
// value (neither code nor signal) is Rust's Default.
type TerminalExit struct {
	ExitCode *uint32
	Signal   *string
}

// TerminalSnapshot is what `terminal/output` serves: the retained output
// plus the exit status once the child has been reaped.
type TerminalSnapshot struct {
	Output    string
	Truncated bool
	// Exit is nil until the child has been reaped.
	Exit *TerminalExit
}

// TerminalBuffer is a terminal's captured output, capped at the limit the
// agent asked for. It is not safe for concurrent use; the registry guards it.
type TerminalBuffer struct {
	bytes   []byte
	limit   int
	dropped bool
}

// NewTerminalBuffer is a buffer that retains at most limit bytes (at least
// one).
func NewTerminalBuffer(limit int) *TerminalBuffer {
	return &TerminalBuffer{limit: max(limit, 1)}
}

// Append adds chunk, dropping from the front once the buffer has grown past
// its limit by the slack.
func (b *TerminalBuffer) Append(chunk []byte) {
	b.bytes = append(b.bytes, chunk...)
	if len(b.bytes)-b.limit > terminalBufferSlackBytes {
		excess := len(b.bytes) - b.limit
		b.bytes = append(b.bytes[:0], b.bytes[excess:]...)
		b.dropped = true
	}
}

// Read is the retained tail as valid UTF-8, and whether anything was
// dropped.
func (b *TerminalBuffer) Read() (string, bool) {
	start := max(len(b.bytes)-b.limit, 0)
	truncated := b.dropped || start > 0
	if truncated {
		// A front cut can land inside a multi-byte character, and ACP
		// requires the served output to start on a character boundary.
		for start < len(b.bytes) && b.bytes[start]&0b1100_0000 == 0b1000_0000 {
			start++
		}
	}
	return fromUTF8Lossy(b.bytes[start:]), truncated
}

// BufferedLen is the bytes held right now, including slack a compaction has
// not reclaimed.
func (b *TerminalBuffer) BufferedLen() int { return len(b.bytes) }

// sharedTerminalBuffer is a TerminalBuffer shared by a terminal's registry
// entry and its supervisor (Rust's Arc<Mutex<TerminalBuffer>>).
type sharedTerminalBuffer struct {
	mu     sync.Mutex
	buffer *TerminalBuffer
}

func newSharedTerminalBuffer(limit int) *sharedTerminalBuffer {
	return &sharedTerminalBuffer{buffer: NewTerminalBuffer(limit)}
}

func (s *sharedTerminalBuffer) append(chunk []byte) {
	s.mu.Lock()
	defer s.mu.Unlock()
	s.buffer.Append(chunk)
}

func (s *sharedTerminalBuffer) read() (string, bool) {
	s.mu.Lock()
	defer s.mu.Unlock()
	return s.buffer.Read()
}

// terminalProcessGroup is a terminal's process group, killed when the
// registry entry holding it goes away. Explicit teardown is the normal path;
// Rust's Drop also covers a connection future dropped mid-flight when the ACP
// bridge dies. Go has no Drop: every path that forgets an entry calls
// killIfLive itself. Nothing else in the container would stop these
// processes.
type terminalProcessGroup struct {
	// pid is the group leader, and the target of a group signal.
	pid  int
	exit *core.Watch[*TerminalExit]
}

// killIfLive signals the group only while its child has not been reaped:
// the kernel may already have handed its ID to somebody else.
func (g *terminalProcessGroup) killIfLive() {
	if g.exit.Borrow() == nil {
		killTerminalProcessGroup(g.pid)
	}
}

type terminalEntry struct {
	group      terminalProcessGroup
	buffer     *sharedTerminalBuffer
	exit       *core.Watch[*TerminalExit]
	supervisor *core.JoinHandle[struct{}]
}

// TerminalRegistry is the terminals one ACP connection owns.
//
// Rust clones share the registry; in Go the ACP handlers and connection
// teardown hold the same *TerminalRegistry.
type TerminalRegistry struct {
	mu        sync.Mutex
	terminals map[string]*terminalEntry
	// connection is random per registry. The session transcript outlives
	// this connection and attaches output by terminal id, so ids must not
	// repeat after a harness restart or worker replacement opens a new
	// registry.
	connection string
	nextID     atomic.Uint64
}

// NewTerminalRegistry is an empty registry with a fresh random connection
// id.
func NewTerminalRegistry() (*TerminalRegistry, error) {
	var random [8]byte
	if _, err := rand.Read(random[:]); err != nil {
		return nil, fmt.Errorf("generate terminal connection id: %w", err)
	}
	return &TerminalRegistry{
		terminals:  map[string]*terminalEntry{},
		connection: core.LowerHex(random[:]),
	}, nil
}

// Create spawns spawn in its own process group and registers it under a
// fresh terminal id.
func (r *TerminalRegistry) Create(spawn TerminalSpawn, events chan<- core.RuntimeEvent) (string, error) {
	return r.createWithID(r.allocateID(), spawn, events)
}

func (r *TerminalRegistry) connectionID() string { return r.connection }

// terminalRegistryOwnsTerminal is TerminalRegistry::owns_terminal: whether
// terminalID was allocated by the registry whose connection id is
// connectionID.
func terminalRegistryOwnsTerminal(connectionID, terminalID string) bool {
	id, ok := strings.CutPrefix(terminalID, "term-")
	if !ok {
		return false
	}
	suffix, ok := strings.CutPrefix(id, connectionID)
	return ok && strings.HasPrefix(suffix, "-")
}

func (r *TerminalRegistry) allocateID() string {
	return fmt.Sprintf("term-%s-%d", r.connection, r.nextID.Add(1))
}

// createWithID spawns and registers under terminalID, which the caller
// registered with its relay owner before spawn.
func (r *TerminalRegistry) createWithID(terminalID string, spawn TerminalSpawn, events chan<- core.RuntimeEvent) (string, error) {
	panic("unported: mj-worker/src/terminal.rs TerminalRegistry::create_with_id")
}

// Output is the output served for `terminal/output`; false for an unknown
// id.
func (r *TerminalRegistry) Output(terminalID string) (TerminalSnapshot, bool) {
	r.mu.Lock()
	defer r.mu.Unlock()
	entry, ok := r.terminals[terminalID]
	if !ok {
		return TerminalSnapshot{}, false
	}
	output, truncated := entry.buffer.read()
	snapshot := TerminalSnapshot{Output: output, Truncated: truncated}
	if exit := entry.exit.Borrow(); exit != nil {
		published := *exit
		snapshot.Exit = &published
	}
	return snapshot, true
}

// ExitReceiver is a watch a `terminal/wait_for_exit` task can await; false
// for an unknown id.
func (r *TerminalRegistry) ExitReceiver(terminalID string) (*core.Watch[*TerminalExit], bool) {
	r.mu.Lock()
	defer r.mu.Unlock()
	entry, ok := r.terminals[terminalID]
	if !ok {
		return nil, false
	}
	return entry.exit, true
}

// Kill kills a terminal's process group. The terminal stays registered, so
// `terminal/output` and `terminal/wait_for_exit` keep working after it.
// False for an unknown id.
func (r *TerminalRegistry) Kill(terminalID string) bool {
	r.mu.Lock()
	defer r.mu.Unlock()
	entry, ok := r.terminals[terminalID]
	if !ok {
		return false
	}
	entry.group.killIfLive()
	return true
}

// KillLive signals every live terminal's process group without forgetting
// it. Cancel uses this so an agent blocked in `terminal/wait_for_exit` can
// finish a cooperative `session/cancel`.
func (r *TerminalRegistry) KillLive() {
	r.mu.Lock()
	defer r.mu.Unlock()
	for _, entry := range r.terminals {
		entry.group.killIfLive()
	}
}

// Release kills a terminal if it still runs, then forgets it. The supervisor
// keeps the child until it reaps it, so the close report still arrives
// exactly once; the returned handle is that supervisor. False for an unknown
// id.
func (r *TerminalRegistry) Release(terminalID string) (*core.JoinHandle[struct{}], bool) {
	r.mu.Lock()
	entry, ok := r.terminals[terminalID]
	delete(r.terminals, terminalID)
	r.mu.Unlock()
	if !ok {
		return nil, false
	}
	// Rust drops what is left of the entry here, which kills the group
	// before the caller can await the supervisor.
	entry.group.killIfLive()
	return entry.supervisor, true
}

// Shutdown kills every live terminal and reaps its supervisor. A connection
// must not leave process groups behind: nothing else in the container would
// stop them.
//
// Every group is killed before any supervisor is awaited, and the
// supervisors are then reaped concurrently under one deadline, so teardown
// costs one reap timeout rather than one per terminal. ctx bounds only the
// delivery of the failure reports on events.
func (r *TerminalRegistry) Shutdown(ctx context.Context, events chan<- core.RuntimeEvent) {
	panic("unported: mj-worker/src/terminal.rs TerminalRegistry::shutdown")
}

// DisplayCommand is the concise command a client can show while this
// terminal is live. Kimi normally sends an interpreter plus `-c <script>`;
// showing the script itself matches the tool title it would have published.
func (s TerminalSpawn) DisplayCommand() string {
	switch filepath.Base(s.Command) {
	case "sh", "bash", "dash", "zsh":
		if index := slices.Index(s.Args, "-c"); index >= 0 && index+1 < len(s.Args) {
			return s.Args[index+1]
		}
	}
	return ShellLine(s.Command, s.Args)
}

// WaitForExit awaits a terminal's exit on a watch taken from the registry.
// It returns the zero TerminalExit when ctx ends first, as Rust does when
// the supervisor went away without recording an exit.
func WaitForExit(ctx context.Context, exit *core.Watch[*TerminalExit]) TerminalExit {
	for {
		changed := exit.Changed()
		if value := exit.Borrow(); value != nil {
			return *value
		}
		select {
		case <-changed:
		case <-ctx.Done():
			return TerminalExit{}
		}
	}
}

// ShellLine is one shell line for a create request: the command verbatim,
// plus every argument single-quoted.
//
// Kimi sends the interpreter in `command` with `["-c", script]` in `args`;
// Grok Build sends the whole line in `command` with no args at all. Running
// the joined line under `sh -c` executes both shapes.
func ShellLine(command string, args []string) string {
	var line strings.Builder
	line.WriteString(command)
	for _, arg := range args {
		line.WriteByte(' ')
		line.WriteString(core.PosixQuote(arg))
	}
	return line.String()
}

// superviseTerminal drains one terminal's pipes into its buffer, reaps the
// child, publishes the exit, and reports the terminal's close exactly once.
func superviseTerminal(
	terminalID string,
	child *exec.Cmd,
	stdout io.ReadCloser,
	stderr io.ReadCloser,
	buffer *sharedTerminalBuffer,
	exit *core.Watch[*TerminalExit],
	events chan<- core.RuntimeEvent,
) {
	panic("unported: mj-worker/src/terminal.rs supervise")
}

// drainPipe copies pipe into buffer until end of file.
func drainPipe(pipe io.Reader, buffer *sharedTerminalBuffer) error {
	chunk := make([]byte, terminalReadChunkBytes)
	for {
		read, err := pipe.Read(chunk)
		if read > 0 {
			buffer.append(chunk[:read])
		}
		if errors.Is(err, io.EOF) {
			return nil
		}
		if err != nil {
			return fmt.Errorf("read client terminal pipe: %w", err)
		}
	}
}

// terminalReport sends message to the relay as a warning.
func terminalReport(events chan<- core.RuntimeEvent, message string) {
	terminalReportEvent(events, core.RuntimeEventWarning{Message: message})
}

// terminalReportEvent sends event to the relay coordinator. A coordinator
// that already stopped is logged at debug level, not treated as a failure.
func terminalReportEvent(events chan<- core.RuntimeEvent, event core.RuntimeEvent) {
	panic("unported: mj-worker/src/terminal.rs report_event")
}
