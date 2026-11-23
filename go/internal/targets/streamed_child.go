// Port of mj-targets/src/streamed_child.rs.
//
// A child process that streams: the caller sees a writer, two readers and an
// exit it can wait for, try, or force. The relay proxy uses it, and so do
// the quota and capacity probes that need a live stdin/stdout exchange.

package targets

import (
	"context"
	"errors"
	"fmt"
	"io"
	"io/fs"
	"os/exec"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

type ProxyWriter = io.WriteCloser
type ProxyReader = io.ReadCloser

// IsMissingProgram reports whether a failed local launch means the program
// was not there.
func IsMissingProgram(err error) bool {
	return errors.Is(err, fs.ErrNotExist) || errors.Is(err, exec.ErrNotFound)
}

type ProxyStdio struct {
	Child  *ProxyChild
	Stdin  ProxyWriter
	Stdout ProxyReader
	Stderr ProxyReader
}

// ProxyExit is how a proxy ended: its exit code, or none when a signal ended
// it.
type ProxyExit struct {
	code *int
}

func (e ProxyExit) Code() (int, bool) {
	if e.code == nil {
		return 0, false
	}
	return *e.code, true
}

func (e ProxyExit) Success() bool {
	return e.code != nil && *e.code == 0
}

func (e ProxyExit) String() string {
	if e.code == nil {
		return "terminated without an exit status"
	}
	return fmt.Sprintf("exit status: %d", *e.code)
}

// ProxyChild is the child and its process group. The group is killed once
// the child has exited, when the child is killed, and when this is closed
// (Rust's Drop), so nothing the proxy started outlives it.
type ProxyChild struct {
	child *exec.Cmd
	group *core.ProcessGroupGuard
	// done is closed once the child has been reaped; exit and waitErr then
	// hold the result.
	done    chan struct{}
	exit    ProxyExit
	waitErr error
}

func (c *ProxyChild) killGroup() {
	if c.group != nil {
		c.group.Kill()
	}
}

func (c *ProxyChild) Wait(ctx context.Context) (ProxyExit, error) {
	panic("unported: mj-targets/src/streamed_child.rs ProxyChild::wait")
}

// TryWait is the exit when the child has ended, or nil while it runs.
// Synchronous, because the relay's drop reaper runs without a runtime.
func (c *ProxyChild) TryWait() (*ProxyExit, error) {
	panic("unported: mj-targets/src/streamed_child.rs ProxyChild::try_wait")
}

func (c *ProxyChild) StartKill() error {
	panic("unported: mj-targets/src/streamed_child.rs ProxyChild::start_kill")
}

// Close is Rust's Drop: it kills the child (unless it detaches) and its
// process group.
func (c *ProxyChild) Close() {
	panic("unported: mj-targets/src/streamed_child.rs ProxyChild drop")
}

// CollectBounded reads a child's output to end, keeping at most limit
// bytes.
//
// The reader always drains to EOF so a child never blocks on a full pipe
// after the retained output reaches the limit.
func CollectBounded(reader io.Reader, limit int) ([]byte, error) {
	panic("unported: mj-targets/src/streamed_child.rs collect_bounded")
}

// SpawnProxy starts the relay proxy spec describes.
func SpawnProxy(spec *core.CommandSpec) (*ProxyStdio, error) {
	return SpawnSessionProxy(spec, nil)
}

// stripStart removes the SSH start marker from a proxy's stdout and reports
// the session's start to its lease's gate.
type stripStart struct {
	inner io.ReadCloser
	scan  *core.StartScan
	gate  *core.StartGate
	// ready is bytes the scan released that the reader has not taken yet.
	ready    []byte
	offset   int
	finished bool
}

func (s *stripStart) Read(buffer []byte) (int, error) {
	panic("unported: mj-targets/src/streamed_child.rs StripStart::poll_read")
}

// Close is Rust's Drop: a reader that goes away before the end saw no more
// than it saw.
func (s *stripStart) Close() error {
	s.gate.MarkEnded()
	return s.inner.Close()
}

// SpawnSessionProxy is SpawnProxy for a command that may run as an SSH
// session the daemon leased itself (lease). A command that prints the SSH
// start marker has it removed from the proxy's stdout, and the lease's gate
// learns when the session starts.
func SpawnSessionProxy(spec *core.CommandSpec, lease *core.SshSessionLease) (*ProxyStdio, error) {
	panic("unported: mj-targets/src/streamed_child.rs spawn_session_proxy")
}
