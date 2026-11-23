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
	"os"
	"os/exec"
	"path/filepath"
	"runtime"
	"slices"
	"strings"

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
	// detaches: the child outlives this handle (Rust: kill_on_drop off).
	detaches bool
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
	select {
	case <-c.done:
	case <-ctx.Done():
		return ProxyExit{}, ctx.Err()
	}
	if c.waitErr != nil {
		return ProxyExit{}, c.waitErr
	}
	c.killGroup()
	return c.exit, nil
}

// TryWait is the exit when the child has ended, or nil while it runs.
// Synchronous, because the relay's drop reaper runs without a runtime.
func (c *ProxyChild) TryWait() (*ProxyExit, error) {
	select {
	case <-c.done:
	default:
		return nil, nil
	}
	if c.waitErr != nil {
		return nil, c.waitErr
	}
	c.killGroup()
	exit := c.exit
	return &exit, nil
}

func (c *ProxyChild) StartKill() error {
	c.killGroup()
	select {
	case <-c.done:
		return errExitedProcess
	default:
	}
	if err := c.child.Process.Kill(); err != nil {
		if errors.Is(err, os.ErrProcessDone) {
			return errExitedProcess
		}
		return err
	}
	return nil
}

// errExitedProcess is tokio's refusal to kill a child already reaped.
var errExitedProcess = errors.New("invalid argument: can't kill an exited process")

// Close is Rust's Drop: it kills the child (unless it detaches) and its
// process group.
func (c *ProxyChild) Close() {
	if c.detaches {
		return
	}
	select {
	case <-c.done:
	default:
		_ = c.child.Process.Kill()
	}
	c.killGroup()
}

// CollectBounded reads a child's output to end, keeping at most limit
// bytes.
//
// The reader always drains to EOF so a child never blocks on a full pipe
// after the retained output reaches the limit.
func CollectBounded(reader io.Reader, limit int) ([]byte, error) {
	collected := []byte{}
	buffer := make([]byte, 16*1024)
	for {
		read, err := reader.Read(buffer)
		if read > 0 {
			room := max(limit-len(collected), 0)
			collected = append(collected, buffer[:min(read, room)]...)
		}
		if errors.Is(err, io.EOF) {
			return collected, nil
		}
		if err != nil {
			return nil, err
		}
	}
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
	if len(buffer) == 0 {
		return 0, nil
	}
	for {
		if s.offset < len(s.ready) {
			count := copy(buffer, s.ready[s.offset:])
			s.offset += count
			if s.offset == len(s.ready) {
				s.ready = s.ready[:0]
				s.offset = 0
			}
			return count, nil
		}
		if s.finished {
			return 0, io.EOF
		}
		chunk := make([]byte, 8192)
		read, err := s.inner.Read(chunk)
		if read > 0 && s.scan.Push(chunk[:read], &s.ready) {
			s.gate.MarkStarted()
		}
		switch {
		case errors.Is(err, io.EOF):
			s.finished = true
			s.scan.Finish(&s.ready)
			s.gate.MarkEnded()
		case err != nil:
			s.finished = true
			s.gate.MarkEnded()
			return 0, err
		}
	}
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
	command := exec.Command(spec.Program, spec.Args...)
	if spec.ClearEnv || len(spec.Env) > 0 || len(spec.EnvRemove) > 0 {
		command.Env = proxyEnvironment(spec)
	}
	// As Rust's Command does, a PATH given to the child is also where its
	// program is looked up.
	if path, ok := spec.Env["PATH"]; ok && !strings.ContainsAny(spec.Program, `/\`) {
		if resolved, found := proxyLookPathIn(spec.Program, path); found {
			command.Path = resolved
			command.Err = nil
		}
	}
	if spec.Cwd != nil {
		command.Dir = *spec.Cwd
	}
	// Its own process group, so the whole tree can be torn down at once.
	setProxyProcessGroup(command)
	var pipes [6]*os.File
	closeAll := func(files ...*os.File) {
		for _, file := range files {
			if file != nil {
				_ = file.Close()
			}
		}
	}
	for i := 0; i < 3; i++ {
		read, write, err := os.Pipe()
		if err != nil {
			closeAll(pipes[:]...)
			return nil, fmt.Errorf("start session relay proxy for %s: %w", spec.Purpose, err)
		}
		pipes[2*i], pipes[2*i+1] = read, write
	}
	stdinRead, stdinWrite := pipes[0], pipes[1]
	stdoutRead, stdoutWrite := pipes[2], pipes[3]
	stderrRead, stderrWrite := pipes[4], pipes[5]
	command.Stdin = stdinRead
	command.Stdout = stdoutWrite
	// Never inherit: the controller owns a TUI alternate screen, so a child
	// writing to the shared stderr corrupts the display outside the
	// renderer's buffer. Drain it into the log instead.
	command.Stderr = stderrWrite
	err := core.SpawnChild(command)
	closeAll(stdinRead, stdoutWrite, stderrWrite)
	if err != nil {
		closeAll(stdinWrite, stdoutRead, stderrRead)
		return nil, fmt.Errorf("start session relay proxy for %s: %w", spec.Purpose, err)
	}
	child := &ProxyChild{child: command, detaches: spec.Detaches, done: make(chan struct{})}
	if !spec.Detaches {
		child.group = core.NewProcessGroupGuard(command.Process.Pid)
	}
	go func() {
		defer close(child.done)
		err := command.Wait()
		var exitErr *exec.ExitError
		if err != nil && !errors.As(err, &exitErr) {
			child.waitErr = err
			return
		}
		if code := command.ProcessState.ExitCode(); code >= 0 {
			child.exit = ProxyExit{code: &code}
		}
	}()
	var stdout ProxyReader = stdoutRead
	if lease != nil && spec.ExpectsSSHStartMarker() {
		stdout = &stripStart{inner: stdoutRead, scan: core.NewStartScan(), gate: lease.Gate()}
	}
	return &ProxyStdio{Child: child, Stdin: stdinWrite, Stdout: stdout, Stderr: stderrRead}, nil
}

// proxyEnvironment is the child's environment: the inherited one unless
// cleared, with spec.Env set and spec.EnvRemove removed.
func proxyEnvironment(spec *core.CommandSpec) []string {
	sameName := func(left, right string) bool {
		if runtime.GOOS == "windows" {
			return strings.EqualFold(left, right)
		}
		return left == right
	}
	removed := func(name string) bool {
		return slices.ContainsFunc(spec.EnvRemove, func(entry string) bool { return sameName(entry, name) })
	}
	overridden := func(name string) bool {
		for key := range spec.Env {
			if sameName(key, name) {
				return true
			}
		}
		return removed(name)
	}
	environment := []string{}
	if !spec.ClearEnv {
		for _, entry := range os.Environ() {
			name, _, _ := strings.Cut(entry, "=")
			if !overridden(name) {
				environment = append(environment, entry)
			}
		}
	}
	names := make([]string, 0, len(spec.Env))
	for name := range spec.Env {
		if !removed(name) {
			names = append(names, name)
		}
	}
	slices.Sort(names)
	for _, name := range names {
		environment = append(environment, name+"="+spec.Env[name])
	}
	return environment
}

// proxyLookPathIn finds program on the search path path.
func proxyLookPathIn(program, path string) (string, bool) {
	for _, directory := range filepath.SplitList(path) {
		if directory == "" {
			directory = "."
		}
		if resolved, err := exec.LookPath(filepath.Join(directory, program)); err == nil {
			return resolved, true
		}
	}
	return "", false
}
