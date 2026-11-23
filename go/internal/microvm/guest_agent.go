//go:build linux

// Port of mj-microvm/src/guest_agent.rs.
//
// The guest agent: PID 1 inside a microVM, serving the vsock protocol.
//
// The initrd's `/init` prepares the machine (modules, mounts, network) and
// executes this binary as `mj-worker microvm guest-agent` in the new root.
// The agent reaps children, listens on vsock port MicrovmAgentPort, runs
// exec sessions and copies for the host engine, and powers the VM off when
// the container's main process exits or the engine asks it to stop.

package microvm

import (
	"encoding/binary"
	"encoding/json"
	"errors"
	"fmt"
	"os"
	"os/exec"
	"path/filepath"
	"runtime"
	"sort"
	"strconv"
	"strings"
	"sync"
	"sync/atomic"
	"syscall"
	"time"
	"unicode/utf8"

	"golang.org/x/sys/unix"
)

// microvmDefaultPath is the PATH a command gets when its environment has
// none.
const microvmDefaultPath = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"

// ---------------------------------------------------------------------------
// System operations the agent performs on the machine it runs in.

// MicrovmGuestSystem is the whole-machine operations. The real
// implementation acts on the VM; tests record the calls.
type MicrovmGuestSystem interface {
	// Quiesce kills every other process, writes all data to disk, and
	// unmounts.
	Quiesce()
	// PowerOff turns the machine off. The real implementation does not
	// return.
	PowerOff()
}

// MicrovmWaitResult is a finished-or-failed wait on a child: Err is set
// when the wait failed, Status otherwise.
type MicrovmWaitResult struct {
	Status syscall.WaitStatus
	Err    error
}

// MicrovmSpawned is a process the agent started.
//
// Rust's Spawned also hands over the child's piped stdio; in Go the caller
// makes its pipes (exec.Cmd's StdinPipe and StdoutPipe, or os.Pipe) before
// the spawn and keeps their parent ends.
type MicrovmSpawned struct {
	Pid uint32
	// Exit delivers the child's end exactly once. It is buffered, so the
	// reaper never waits for a starter that stopped listening.
	Exit <-chan MicrovmWaitResult
}

// MicrovmProcessSpawner is how the agent starts processes and learns how
// they ended.
//
// As PID 1 the agent must reap orphans too, so a single reaper owns every
// `waitpid` and hands exit statuses to whoever started the process. No
// caller may call Wait on a command it spawned through one.
type MicrovmProcessSpawner interface {
	Spawn(command *exec.Cmd) (*MicrovmSpawned, error)
}

// MicrovmDirectSpawner waits for each child on its own goroutine. Used
// where the process is not PID 1.
type MicrovmDirectSpawner struct{}

// Spawn starts command and waits for it with exec.Cmd.Wait.
func (MicrovmDirectSpawner) Spawn(command *exec.Cmd) (*MicrovmSpawned, error) {
	if err := command.Start(); err != nil {
		return nil, err
	}
	exit := make(chan MicrovmWaitResult, 1)
	go func() {
		state, err := microvmWaitStatus(command)
		if err != nil {
			// Nobody listens when the process was detached; the channel's
			// buffer takes the result either way.
			exit <- MicrovmWaitResult{Err: err}
			return
		}
		status, _ := state.Sys().(syscall.WaitStatus)
		exit <- MicrovmWaitResult{Status: status}
	}()
	return &MicrovmSpawned{Pid: uint32(command.Process.Pid), Exit: exit}, nil
}

// MicrovmReaper is the PID 1 reaper: one goroutine owns `waitpid` and
// routes statuses by pid.
//
// It never depends on SIGCHLD delivery: it blocks in `waitid(WNOWAIT)`,
// which reports that a child can be reaped without reaping it, and then
// reaps under the lock that spawns hold. A spawn whose exec failed waits
// for its half-started child itself, so the reaper must not collect
// anything while a spawn is in progress.
type MicrovmReaper struct {
	mu      sync.Mutex
	spawned *sync.Cond
	state   microvmReaperState
}

type microvmReaperState struct {
	waiters map[int32]chan<- MicrovmWaitResult
	// spawns counts spawns, so the reaper can tell "a child appeared" from
	// "nothing changed".
	spawns uint64
}

// MicrovmStartReaper is Reaper::start: it starts the reaper goroutine.
// The goroutine keeps its OS thread, since waitid blocks it.
func MicrovmStartReaper() (*MicrovmReaper, error) {
	reaper := &MicrovmReaper{state: microvmReaperState{waiters: map[int32]chan<- MicrovmWaitResult{}}}
	reaper.spawned = sync.NewCond(&reaper.mu)
	go reaper.run()
	return reaper, nil
}

func (r *MicrovmReaper) run() {
	runtime.LockOSThread()
	for {
		r.mu.Lock()
		seen := r.state.spawns
		r.mu.Unlock()
		exited, err := microvmWaitForExitedChild()
		switch {
		case err != nil:
			MicrovmKlog(fmt.Sprintf("reaper: waitid failed: %s", microvmIOError(err)))
			time.Sleep(time.Second)
		case exited:
			r.mu.Lock()
			for {
				var status unix.WaitStatus
				pid, err := unix.Wait4(-1, &status, unix.WNOHANG, nil)
				if err != nil || pid <= 0 {
					break
				}
				if waiter, ok := r.state.waiters[int32(pid)]; ok {
					delete(r.state.waiters, int32(pid))
					// The starter may have stopped listening (a detached
					// exec); the channel's buffer takes the status.
					waiter <- MicrovmWaitResult{Status: syscall.WaitStatus(status)}
				}
			}
			r.mu.Unlock()
		default:
			// No children at all: sleep until a spawn creates one. Orphans
			// only exist while some descendant does.
			r.mu.Lock()
			for r.state.spawns == seen {
				r.spawned.Wait()
			}
			r.mu.Unlock()
		}
	}
}

// microvmWaitForExitedChild blocks until some child has exited (without
// collecting it); false means the process has no children.
func microvmWaitForExitedChild() (bool, error) {
	for {
		var info unix.Siginfo
		err := unix.Waitid(unix.P_ALL, 0, &info, unix.WEXITED|unix.WNOWAIT, nil)
		switch err {
		case nil:
			return true, nil
		case unix.ECHILD:
			return false, nil
		case unix.EINTR:
			continue
		}
		return false, err
	}
}

// Spawn starts command while holding the reaper's lock, so the reaper
// cannot collect the child before its waiter is registered, or a failed
// exec's child before the spawn has.
func (r *MicrovmReaper) Spawn(command *exec.Cmd) (*MicrovmSpawned, error) {
	r.mu.Lock()
	defer r.mu.Unlock()
	if err := command.Start(); err != nil {
		return nil, err
	}
	pid := command.Process.Pid
	exit := make(chan MicrovmWaitResult, 1)
	r.state.waiters[int32(pid)] = exit
	r.state.spawns++
	r.spawned.Broadcast()
	return &MicrovmSpawned{Pid: uint32(pid), Exit: exit}, nil
}

// ---------------------------------------------------------------------------
// Users

// MicrovmResolvedUser is a user spec resolved against the passwd and group
// files.
type MicrovmResolvedUser struct {
	UID    uint32
	GID    uint32
	Groups []uint32
	Home   string
	// Name is nil for a numeric user without a passwd entry.
	Name *string
}

// MicrovmResolveUser resolves `name`, `uid`, `name:group` or `uid:gid`
// against passwd and group files.
func MicrovmResolveUser(spec string, passwd string, group string) (MicrovmResolvedUser, error) {
	userPart, groupPart, hasGroup := strings.Cut(spec, ":")
	if userPart == "" {
		userPart = "0"
	}
	entry := func(matches func([]string) bool) []string {
		for _, line := range microvmLines(passwd) {
			fields := strings.Split(line, ":")
			if len(fields) >= 6 && matches(fields) {
				return fields
			}
		}
		return nil
	}
	numeric, isNumeric := microvmParseU32(userPart)
	var found []string
	if isNumeric {
		found = entry(func(fields []string) bool {
			uid, ok := microvmParseU32(fields[2])
			return ok && uid == numeric
		})
	} else {
		found = entry(func(fields []string) bool { return fields[0] == userPart })
	}
	var uid, primaryGID uint32
	var home string
	var name *string
	switch {
	case found != nil:
		parsedUID, err := microvmParseUint(found[2], 32)
		if err != nil {
			return MicrovmResolvedUser{}, fmt.Errorf("passwd uid: %w", err)
		}
		parsedGID, err := microvmParseUint(found[3], 32)
		if err != nil {
			return MicrovmResolvedUser{}, fmt.Errorf("passwd gid: %w", err)
		}
		uid, primaryGID, home = uint32(parsedUID), uint32(parsedGID), found[5]
		owner := found[0]
		name = &owner
	case isNumeric:
		uid, primaryGID, home = numeric, numeric, "/"
	default:
		return MicrovmResolvedUser{}, fmt.Errorf("unable to find user %s: no matching entries in passwd file", userPart)
	}
	var groupEntries [][]string
	for _, line := range microvmLines(group) {
		if fields := strings.Split(line, ":"); len(fields) >= 3 {
			groupEntries = append(groupEntries, fields)
		}
	}
	gid := primaryGID
	if hasGroup {
		if parsed, ok := microvmParseU32(groupPart); ok {
			gid = parsed
		} else {
			resolved := false
			for _, fields := range groupEntries {
				if fields[0] == groupPart {
					gid, resolved = microvmParseU32(fields[2])
					break
				}
			}
			if !resolved {
				return MicrovmResolvedUser{}, fmt.Errorf("unable to find group %s", groupPart)
			}
		}
	}
	groups := []uint32{}
	if name != nil {
		for _, fields := range groupEntries {
			if len(fields) <= 3 {
				continue
			}
			member := false
			for _, m := range strings.Split(fields[3], ",") {
				if m == *name {
					member = true
					break
				}
			}
			if member {
				if parsed, ok := microvmParseU32(fields[2]); ok {
					groups = append(groups, parsed)
				}
			}
		}
	}
	return MicrovmResolvedUser{UID: uid, GID: gid, Groups: groups, Home: home, Name: name}, nil
}

// microvmReadTextOrEmpty is Rust's `fs::read_to_string(path).unwrap_or_default()`.
func microvmReadTextOrEmpty(path string) string {
	data, err := os.ReadFile(path)
	if err != nil {
		return ""
	}
	return microvmUTF8OrEmpty(data)
}

func microvmResolveUserInGuest(spec string) (MicrovmResolvedUser, error) {
	// No user named means the agent's own identity (root in a real guest).
	if spec == "" {
		spec = strconv.Itoa(os.Geteuid())
	}
	return MicrovmResolveUser(spec, microvmReadTextOrEmpty("/etc/passwd"), microvmReadTextOrEmpty("/etc/group"))
}

// ---------------------------------------------------------------------------
// The agent

// MicrovmAgent serves the vsock protocol inside the VM.
type MicrovmAgent struct {
	defaultsMu sync.Mutex
	defaults   MicrovmStartRequest
	started    atomic.Bool
	// shuttingDown is true once shutdown has begun. Spawns hold the read
	// side of shutdownMu, so a process is either admitted before the
	// shutdown sweep kills it or refused.
	shutdownMu   sync.RWMutex
	shuttingDown bool
	spawner      MicrovmProcessSpawner
	system       MicrovmGuestSystem
	// runUserRoot is where `/run/user/<uid>` directories are made.
	runUserRoot string
}

// microvmSpawnFailure is a spawn failure that maps to a shell exit status.
type microvmSpawnFailure struct {
	status  int32
	message string
}

// NewMicrovmAgent is Agent::new.
func NewMicrovmAgent(spawner MicrovmProcessSpawner, system MicrovmGuestSystem) *MicrovmAgent {
	return &MicrovmAgent{spawner: spawner, system: system, runUserRoot: "/run/user"}
}

func (a *MicrovmAgent) currentDefaults() MicrovmStartRequest {
	a.defaultsMu.Lock()
	defer a.defaultsMu.Unlock()
	return a.defaults
}

// Serve serves one connection: hello, one request, its data frames.
func (a *MicrovmAgent) Serve(transport MicrovmTransport) error {
	// Rust closes the connection when its last handle drops; here the
	// connection ends with the request, and a goroutine still writing (an
	// exec monitor whose client left) sees its sends fail, as Rust's do.
	defer transport.Close()
	sender := NewMicrovmFrameSender(transport)
	frames := NewMicrovmFrameReader(transport)
	hello, err := frames.Expect("the client hello")
	if err != nil {
		return err
	}
	if hello.Kind != MicrovmFrameKindHello {
		return fmt.Errorf("expected a hello frame, received %s", hello.Kind)
	}
	var theirs MicrovmHello
	if err := json.Unmarshal(hello.Payload, &theirs); err != nil {
		return fmt.Errorf("decode client hello: %w", err)
	}
	ours := MicrovmCurrentHello()
	if _, err := MicrovmNegotiate(ours, theirs); err != nil {
		// Tell the peer why, then close; a peer too old to read this still
		// sees the connection end.
		_ = sender.Send(MicrovmFrameKindError, []byte(err.Error()))
		_ = sender.Shutdown()
		return err
	}
	if err := sender.SendJSON(MicrovmFrameKindHello, ours); err != nil {
		return err
	}
	frame, err := frames.Expect("a request")
	if err != nil {
		return err
	}
	if frame.Kind != MicrovmFrameKindRequest {
		return fmt.Errorf("expected a request frame, received %s", frame.Kind)
	}
	request, err := MicrovmDecodeRequest(frame.Payload)
	if err != nil {
		return fmt.Errorf("decode request: %w", err)
	}
	var result error
	switch request := request.(type) {
	case MicrovmRequestStart:
		result = a.handleStart(sender, request.Request)
	case MicrovmRequestExec:
		result = a.handleExec(sender, frames, request.Request)
	case MicrovmRequestCopyIn:
		result = a.handleCopyIn(sender, frames, request.Destination, microvmCopyMode(request.Contents))
	case MicrovmRequestCopyOut:
		result = a.handleCopyOut(sender, request.Source, microvmCopyMode(request.Contents))
	case MicrovmRequestStop:
		result = a.handleStop(sender)
	}
	if result != nil {
		// The client may already be gone; its absence is not a second failure.
		_ = sender.Send(MicrovmFrameKindError, []byte(result.Error()))
	}
	return result
}

func (a *MicrovmAgent) handleStart(sender *MicrovmFrameSender, start MicrovmStartRequest) error {
	if a.started.Swap(true) {
		return errors.New("the container has already been started")
	}
	pid, release, err := a.startMain(start)
	if err != nil {
		a.started.Store(false)
		return err
	}
	answered := sender.SendJSON(MicrovmFrameKindAccepted, MicrovmAccepted{Pid: pid})
	// A command that has already exited may power the VM off now, and not
	// before the host has its answer. Releasing the monitor happens whether
	// or not the answer was delivered.
	release()
	return answered
}

// startMain starts the main process. Until release is called, the process
// monitor is held back from powering the machine off.
func (a *MicrovmAgent) startMain(start MicrovmStartRequest) (pid uint32, release func(), err error) {
	user, err := microvmResolveUserInGuest(start.User)
	if err != nil {
		return 0, nil, err
	}
	for _, volume := range start.Volumes {
		if !volume.Chown {
			continue
		}
		if err := microvmChownTree(volume.Destination, user.UID, user.GID); err != nil {
			return 0, nil, fmt.Errorf("give %s to the container user: %w", volume.Destination, err)
		}
	}
	a.defaultsMu.Lock()
	a.defaults = start
	a.defaultsMu.Unlock()
	request := MicrovmExecRequest{Argv: start.Argv, Detach: true}
	spawned, failure := a.spawnProcess(request, nil, nil, nil)
	if failure != nil {
		return 0, nil, fmt.Errorf("start the container command: %s", failure.message)
	}
	released := make(chan struct{})
	var once sync.Once
	release = func() { once.Do(func() { close(released) }) }
	go func() {
		result := <-spawned.Exit
		if result.Err != nil {
			MicrovmKlog(fmt.Sprintf("waiting for the container command failed: %s", result.Err))
		} else {
			MicrovmKlog("container command exited: " + microvmWaitStatusString(result.Status))
		}
		// Returns when release is called.
		<-released
		a.beginShutdown()
		a.system.Quiesce()
		a.system.PowerOff()
	}()
	return spawned.Pid, release, nil
}

// beginShutdown refuses new processes from now on. Returns once every
// spawn in progress has finished, so the kill sweep that follows sees all
// of them.
func (a *MicrovmAgent) beginShutdown() {
	a.shutdownMu.Lock()
	a.shuttingDown = true
	a.shutdownMu.Unlock()
}

func (a *MicrovmAgent) handleStop(sender *MicrovmFrameSender) error {
	a.beginShutdown()
	a.system.Quiesce()
	code := int32(0)
	if err := sender.SendJSON(MicrovmFrameKindExit, MicrovmExitInfo{Code: &code}); err != nil {
		return err
	}
	a.system.PowerOff()
	return nil
}

// spawnProcess builds and spawns the command for an exec request (or the
// main process). stdin, stdout and stderr are the child's ends; nil is
// `/dev/null`.
func (a *MicrovmAgent) spawnProcess(request MicrovmExecRequest, stdin, stdout, stderr *os.File) (*MicrovmSpawned, *microvmSpawnFailure) {
	defaults := a.currentDefaults()
	userSpec := defaults.User
	if request.User != nil {
		userSpec = *request.User
	}
	user, err := microvmResolveUserInGuest(userSpec)
	if err != nil {
		return nil, microvmFailure(126, err.Error())
	}
	type variable struct{ key, value string }
	var environment []variable
	for _, entry := range append(append([]string{}, defaults.Env...), request.Env...) {
		key, value, _ := strings.Cut(entry, "=")
		kept := environment[:0]
		for _, existing := range environment {
			if existing.key != key {
				kept = append(kept, existing)
			}
		}
		environment = append(kept, variable{key, value})
	}
	lookup := func(key string) (string, bool) {
		for _, existing := range environment {
			if existing.key == key {
				return existing.value, true
			}
		}
		return "", false
	}
	if _, ok := lookup("PATH"); !ok {
		environment = append(environment, variable{"PATH", microvmDefaultPath})
	}
	if _, ok := lookup("HOME"); !ok {
		environment = append(environment, variable{"HOME", user.Home})
	}
	currentUID := uint32(os.Geteuid())
	if _, ok := lookup("XDG_RUNTIME_DIR"); user.UID != 0 && user.UID != currentUID && !ok {
		// Rootless nested containers expect this directory; no logind makes it here.
		directory := filepath.Join(a.runUserRoot, strconv.FormatUint(uint64(user.UID), 10))
		if err := microvmEnsureRuntimeDirectory(directory, user); err != nil {
			return nil, microvmFailure(126, fmt.Sprintf("prepare %s: %s", directory, err))
		}
		environment = append(environment, variable{"XDG_RUNTIME_DIR", directory})
	}
	path, _ := lookup("PATH")
	cwd := "/"
	if request.Cwd != nil && *request.Cwd != "" {
		cwd = *request.Cwd
	} else if defaults.Cwd != "" {
		cwd = defaults.Cwd
	}
	if len(request.Argv) == 0 {
		return nil, microvmFailure(125, "the command is empty")
	}
	program, ok := microvmResolveProgram(request.Argv[0], path, cwd)
	if !ok {
		return nil, microvmFailure(127, fmt.Sprintf("executable file `%s` not found in $PATH", request.Argv[0]))
	}
	// Rust's Command gives the child its environment sorted by name.
	sort.SliceStable(environment, func(i, j int) bool { return environment[i].key < environment[j].key })
	env := make([]string, len(environment))
	for i, variable := range environment {
		env[i] = variable.key + "=" + variable.value
	}
	command := &exec.Cmd{
		Path: program,
		Args: append([]string{program}, request.Argv[1:]...),
		Env:  env,
		Dir:  cwd,
	}
	if stdin != nil {
		command.Stdin = stdin
	}
	if stdout != nil {
		command.Stdout = stdout
	}
	if stderr != nil {
		command.Stderr = stderr
	}
	// A root agent always applies the requested group and supplementary
	// groups, even for uid 0; a non-root agent (tests) can only keep its
	// identity.
	attributes := &syscall.SysProcAttr{Setsid: request.Detach}
	if currentUID == 0 || user.UID != currentUID {
		attributes.Credential = &syscall.Credential{Uid: user.UID, Gid: user.GID, Groups: user.Groups}
	}
	command.SysProcAttr = attributes
	// Admit the process under the read side of the shutdown lock, held
	// across the spawn: shutdown takes the write side before its kill sweep,
	// so no child can appear after the sweep.
	a.shutdownMu.RLock()
	defer a.shutdownMu.RUnlock()
	if a.shuttingDown {
		return nil, microvmFailure(125, "the VM is shutting down")
	}
	spawned, err := a.spawner.Spawn(command)
	if err != nil {
		var errno syscall.Errno
		errors.As(err, &errno)
		switch errno {
		case syscall.ENOENT:
			return nil, microvmFailure(127, fmt.Sprintf("%s: %s", request.Argv[0], microvmIOError(err)))
		case syscall.EACCES, syscall.EPERM:
			return nil, microvmFailure(126, fmt.Sprintf("%s: %s", request.Argv[0], microvmIOError(err)))
		}
		return nil, microvmFailure(126, fmt.Sprintf("start %s: %s", request.Argv[0], microvmIOError(err)))
	}
	return spawned, nil
}

// microvmPipe is a pipe whose ends are blocking files Go's poller does not
// own, so the agent can make its own end non-blocking and wait on it with
// poll(2).
func microvmPipe() (read *os.File, write *os.File, err error) {
	var fds [2]int
	if err := unix.Pipe2(fds[:], unix.O_CLOEXEC); err != nil {
		return nil, nil, err
	}
	return os.NewFile(uintptr(fds[0]), "pipe"), os.NewFile(uintptr(fds[1]), "pipe"), nil
}

func (a *MicrovmAgent) handleExec(sender *MicrovmFrameSender, frames *MicrovmFrameReader, request MicrovmExecRequest) error {
	if request.Detach {
		spawned, failure := a.spawnProcess(request, nil, nil, nil)
		if failure != nil {
			return errors.New(failure.message)
		}
		return sender.SendJSON(MicrovmFrameKindAccepted, MicrovmAccepted{Pid: spawned.Pid})
	}
	var parentEnds, childEnds []*os.File
	closeAll := func(files []*os.File) {
		for _, file := range files {
			_ = file.Close()
		}
	}
	pipe := func(parentReads bool) (parent *os.File, child *os.File, err error) {
		read, write, err := microvmPipe()
		if err != nil {
			return nil, nil, err
		}
		if parentReads {
			parent, child = read, write
		} else {
			parent, child = write, read
		}
		parentEnds = append(parentEnds, parent)
		childEnds = append(childEnds, child)
		return parent, child, nil
	}
	var stdinParent, stdinChild, stdoutParent, stdoutChild, stderrParent, stderrChild *os.File
	var spawned *MicrovmSpawned
	var failure *microvmSpawnFailure
	var pipeErr error
	if request.Stdin {
		stdinParent, stdinChild, pipeErr = pipe(false)
	}
	if pipeErr == nil {
		stdoutParent, stdoutChild, pipeErr = pipe(true)
	}
	if pipeErr == nil {
		stderrParent, stderrChild, pipeErr = pipe(true)
	}
	if pipeErr != nil {
		// Rust's spawn makes these pipes, and fails as a spawn does.
		program := ""
		if len(request.Argv) > 0 {
			program = request.Argv[0]
		}
		failure = microvmFailure(126, fmt.Sprintf("start %s: %s", program, microvmIOError(pipeErr)))
	} else {
		spawned, failure = a.spawnProcess(request, stdinChild, stdoutChild, stderrChild)
	}
	closeAll(childEnds)
	if failure != nil {
		closeAll(parentEnds)
		_ = sender.Send(MicrovmFrameKindStderr, []byte(fmt.Sprintf("Error: %s\n", failure.message)))
		status := failure.status
		return sender.SendJSON(MicrovmFrameKindExit, MicrovmExitInfo{Code: &status})
	}
	wake, err := microvmEventfd()
	if err != nil {
		closeAll(parentEnds)
		return err
	}
	// The wake descriptor is shared by the pumps (joined by the monitor),
	// the monitor and this goroutine's stdin writes; the last of the
	// monitor and this goroutine to finish closes it.
	var wakeHolders atomic.Int32
	wakeHolders.Store(2)
	releaseWake := func() {
		if wakeHolders.Add(-1) == 0 {
			_ = wake.Close()
		}
	}
	defer releaseWake()
	var pumps sync.WaitGroup
	for _, output := range []struct {
		source *os.File
		kind   MicrovmFrameKind
		name   string
	}{
		{stdoutParent, MicrovmFrameKindStdout, "stdout"},
		{stderrParent, MicrovmFrameKindStderr, "stderr"},
	} {
		pumps.Add(1)
		go func() {
			defer pumps.Done()
			if err := microvmPump(output.source, wake, output.kind, sender); err != nil {
				MicrovmKlog(fmt.Sprintf("exec %s pump failed: %s", output.name, err))
			}
		}()
	}
	go func() {
		defer releaseWake()
		result := <-spawned.Exit
		microvmWakePumps(wake)
		pumps.Wait()
		var info MicrovmExitInfo
		if result.Err != nil {
			MicrovmKlog(fmt.Sprintf("waiting for an exec process failed: %s", result.Err))
			code := int32(125)
			info.Code = &code
		} else {
			if result.Status.Exited() {
				code := int32(result.Status.ExitStatus())
				info.Code = &code
			}
			if result.Status.Signaled() {
				signal := int32(result.Status.Signal())
				info.Signal = &signal
			}
		}
		// A client that went away cannot be told; that is not an error.
		_ = sender.SendJSON(MicrovmFrameKindExit, info)
		_ = sender.Shutdown()
	}()
	// Feed stdin until the client says it is done, or goes away.
	stdin := stdinParent
	closeStdin := func() {
		if stdin != nil {
			_ = stdin.Close()
			stdin = nil
		}
	}
	defer closeStdin()
	if stdin != nil {
		if err := microvmSetNonblocking(int(stdin.Fd())); err != nil {
			return fmt.Errorf("make exec stdin non-blocking: %w", microvmIOError(err))
		}
	}
	for {
		frame, err := frames.NextFrame()
		if err != nil || frame == nil {
			// The monitor shut the connection down after the exit frame, or
			// the client left. Either way the child's stdin ends here.
			break
		}
		switch frame.Kind {
		case MicrovmFrameKindStdin:
			if stdin != nil {
				if err := microvmWriteStdin(stdin, frame.Payload, wake); err != nil {
					// The process closed its stdin or exited; later input
					// has nowhere to go and is dropped.
					if !errors.Is(err, syscall.EPIPE) {
						MicrovmKlog(fmt.Sprintf("writing exec stdin failed: %s", microvmIOError(err)))
					}
					closeStdin()
				}
			}
		case MicrovmFrameKindStdinEOF:
			closeStdin()
		default:
			return fmt.Errorf("unexpected %s frame during exec", frame.Kind)
		}
	}
	return nil
}

func (a *MicrovmAgent) handleCopyIn(sender *MicrovmFrameSender, frames *MicrovmFrameReader, destination string, mode MicrovmCopyMode) error {
	defaults := a.currentDefaults()
	user, err := microvmResolveUserInGuest(defaults.User)
	if err != nil {
		return err
	}
	var owner *MicrovmOwner
	if os.Geteuid() == 0 {
		owner = &MicrovmOwner{UID: user.UID, GID: user.GID}
	}
	// A copy is writing to the guest's disks from a goroutine of this
	// process, which the shutdown kill sweep does not reach. Hold the
	// admission lock for its whole run: shutdown waits for it, and later
	// copies are refused.
	release, err := a.admitWork()
	if err != nil {
		return err
	}
	defer release()
	reader := NewMicrovmTarFrameReader(frames)
	if err := MicrovmTarcopyUnpack(reader, destination, owner, mode); err != nil {
		return err
	}
	if err := reader.Finish(); err != nil {
		return err
	}
	code := int32(0)
	return sender.SendJSON(MicrovmFrameKindExit, MicrovmExitInfo{Code: &code})
}

// admitWork admits work that runs in this process rather than as a child.
// Until release is called, shutdown cannot start.
func (a *MicrovmAgent) admitWork() (release func(), err error) {
	a.shutdownMu.RLock()
	if a.shuttingDown {
		a.shutdownMu.RUnlock()
		return nil, fmt.Errorf("the VM is shutting down")
	}
	return a.shutdownMu.RUnlock, nil
}

func (a *MicrovmAgent) handleCopyOut(sender *MicrovmFrameSender, source string, mode MicrovmCopyMode) error {
	writer := NewMicrovmTarFrameWriter(sender)
	if err := MicrovmTarcopyPack(source, writer, mode); err != nil {
		return err
	}
	if err := writer.Finish(); err != nil {
		return fmt.Errorf("finish tar stream: %w", err)
	}
	code := int32(0)
	return sender.SendJSON(MicrovmFrameKindExit, MicrovmExitInfo{Code: &code})
}

func microvmCopyMode(contents bool) MicrovmCopyMode {
	if contents {
		return MicrovmCopyModeContents
	}
	return MicrovmCopyModeItem
}

func microvmFailure(status int32, message string) *microvmSpawnFailure {
	return &microvmSpawnFailure{status: status, message: message}
}

// microvmResolveProgram finds program in path. A relative entry, and an
// empty one, mean the command's working directory, so they are resolved
// against cwd; the result is absolute for those. ok is false when none
// holds an executable file of that name.
func microvmResolveProgram(program string, path string, cwd string) (string, bool) {
	if strings.Contains(program, "/") {
		return program, true
	}
	for _, directory := range strings.Split(path, ":") {
		base := cwd
		if directory != "" {
			base = microvmRustJoin(cwd, directory)
		}
		candidate := microvmRustJoin(base, program)
		if info, err := os.Stat(candidate); err == nil && info.Mode().IsRegular() && info.Mode().Perm()&0o111 != 0 {
			return candidate, true
		}
	}
	return "", false
}

func microvmEnsureRuntimeDirectory(directory string, user MicrovmResolvedUser) error {
	if microvmIsDir(directory) {
		return nil
	}
	if err := os.MkdirAll(directory, 0o777); err != nil {
		return microvmIOError(err)
	}
	if err := os.Chmod(directory, 0o700); err != nil {
		return microvmIOError(err)
	}
	return microvmLchown(directory, user.UID, user.GID)
}

// microvmChownTree is Podman's `U` volume option: change the owner of
// everything below root, without following symlinks and without crossing
// into other filesystems.
func microvmChownTree(root string, uid uint32, gid uint32) error {
	var rootStat unix.Stat_t
	if err := unix.Lstat(root, &rootStat); err != nil {
		return microvmIOError(err)
	}
	device := rootStat.Dev
	pending := []string{root}
	for len(pending) > 0 {
		path := pending[len(pending)-1]
		pending = pending[:len(pending)-1]
		var stat unix.Stat_t
		if err := unix.Lstat(path, &stat); err != nil {
			return microvmIOError(err)
		}
		if stat.Dev != device {
			continue
		}
		if stat.Uid != uid || stat.Gid != gid {
			if err := microvmLchown(path, uid, gid); err != nil {
				return err
			}
		}
		if stat.Mode&unix.S_IFMT == unix.S_IFDIR {
			names, err := microvmReadDirNames(path)
			if err != nil {
				return microvmIOError(err)
			}
			for _, name := range names {
				pending = append(pending, microvmRustJoin(path, name))
			}
		}
	}
	return nil
}

func microvmLchown(path string, uid uint32, gid uint32) error {
	if err := unix.Lchown(path, int(uid), int(gid)); err != nil {
		return fmt.Errorf("change owner of %s: %w", path, microvmIOError(err))
	}
	return nil
}

// ---------------------------------------------------------------------------
// Output pumps
//
// As in Rust, the agent makes its exec pipes non-blocking itself and waits
// on them with poll(2); the wake eventfd tells a pump or a stdin writer
// that the process has exited. So the pipes are made with unix.Pipe2 and
// wrapped with os.NewFile, not made with os.Pipe: a file Go's poller owns
// parks a read instead of returning EAGAIN.

func microvmSetNonblocking(fd int) error {
	return unix.SetNonblock(fd, true)
}

// microvmWriteStdin writes all of data to a non-blocking pipe, waiting for
// room while the process is alive. When the process exits (wake fires) with
// room never coming, because a descendant holds the pipe open and does not
// read it, the remaining input is abandoned as if the pipe had closed.
func microvmWriteStdin(pipe *os.File, data []byte, wake *os.File) error {
	fd, wakeFD := int(pipe.Fd()), int(wake.Fd())
	defer runtime.KeepAlive(pipe)
	defer runtime.KeepAlive(wake)
	for len(data) > 0 {
		count, err := unix.Write(fd, data)
		switch {
		case err == nil:
			data = data[count:]
		case err == unix.EAGAIN:
			fds := []unix.PollFd{
				{Fd: int32(fd), Events: unix.POLLOUT},
				{Fd: int32(wakeFD), Events: unix.POLLIN},
			}
			if _, err := unix.Poll(fds, -1); err != nil {
				if err != unix.EINTR {
					return err
				}
			} else if fds[0].Revents == 0 && fds[1].Revents != 0 {
				return syscall.EPIPE
			}
		case err == unix.EINTR:
		default:
			return err
		}
	}
	return nil
}

func microvmEventfd() (*os.File, error) {
	fd, err := unix.Eventfd(0, unix.EFD_CLOEXEC)
	if err != nil {
		return nil, fmt.Errorf("create eventfd: %w", microvmIOError(err))
	}
	return os.NewFile(uintptr(fd), "eventfd"), nil
}

func microvmWakePumps(wake *os.File) {
	var one [8]byte
	binary.NativeEndian.PutUint64(one[:], 1)
	written, err := unix.Write(int(wake.Fd()), one[:])
	runtime.KeepAlive(wake)
	if err != nil || written != 8 {
		MicrovmKlog("could not wake the output pumps")
	}
}

// microvmPump copies a child's output into frames until the pipe closes, or
// until the child has exited and what it left in the pipe is drained. A
// descendant that keeps the pipe open after the process exits must not hold
// the exec session open. The pump owns source and closes it.
func microvmPump(source *os.File, wake *os.File, kind MicrovmFrameKind, sender *MicrovmFrameSender) error {
	defer source.Close()
	fd, wakeFD := int(source.Fd()), int(wake.Fd())
	defer runtime.KeepAlive(wake)
	if err := microvmSetNonblocking(fd); err != nil {
		return fmt.Errorf("make output pipe non-blocking: %w", microvmIOError(err))
	}
	buffer := make([]byte, MicrovmChunkSize)
	clientGone := false
	exited := false
	for {
		if !exited {
			fds := []unix.PollFd{
				{Fd: int32(fd), Events: unix.POLLIN},
				{Fd: int32(wakeFD), Events: unix.POLLIN},
			}
			if _, err := unix.Poll(fds, -1); err != nil {
				if err == unix.EINTR {
					continue
				}
				return fmt.Errorf("poll exec output: %w", microvmIOError(err))
			}
			exited = fds[1].Revents != 0
		}
		// Drain what is readable now.
	drain:
		for {
			count, err := unix.Read(fd, buffer)
			switch {
			case err == nil && count == 0:
				return nil
			case err == nil:
				if !clientGone && sender.Send(kind, buffer[:count]) != nil {
					// The client left. Keep draining so the process is not
					// blocked on a full pipe.
					clientGone = true
				}
			case err == unix.EAGAIN:
				break drain
			case err == unix.EINTR:
			default:
				return fmt.Errorf("read exec output: %w", microvmIOError(err))
			}
		}
		if exited {
			return nil
		}
	}
}

// ---------------------------------------------------------------------------
// Logging

// MicrovmKlog writes a line to the kernel log, which the VM's console
// shows.
func MicrovmKlog(message string) {
	// Level 3 (error) shows on the console, which init limits to errors and
	// worse.
	line := fmt.Sprintf("<3>mj-microvm-agent: %s\n", message)
	kmsg, err := os.OpenFile("/dev/kmsg", os.O_WRONLY, 0)
	if err == nil {
		_, err = kmsg.WriteString(line)
		_ = kmsg.Close()
	}
	if err != nil {
		// No kernel log (not in a VM): stderr is the next best place.
		fmt.Fprint(os.Stderr, line)
	}
}

// ---------------------------------------------------------------------------
// vsock

// MicrovmVsockStream is a connected AF_VSOCK stream socket.
type MicrovmVsockStream struct {
	file *os.File
}

func (s *MicrovmVsockStream) Read(buffer []byte) (int, error) { return s.file.Read(buffer) }

func (s *MicrovmVsockStream) Write(buffer []byte) (int, error) { return s.file.Write(buffer) }

func (s *MicrovmVsockStream) Close() error { return s.file.Close() }

// Shutdown shuts the socket down in both directions, waking a blocked
// reader.
func (s *MicrovmVsockStream) Shutdown() error {
	// Control keeps the descriptor from being closed, and its number
	// reused, while the shutdown runs.
	raw, err := s.file.SyscallConn()
	if err != nil {
		return microvmIOError(err)
	}
	var shutdownErr error
	if err := raw.Control(func(fd uintptr) {
		shutdownErr = unix.Shutdown(int(fd), unix.SHUT_RDWR)
	}); err != nil {
		return microvmIOError(err)
	}
	return microvmIOError(shutdownErr)
}

// microvmVsockListen returns a listening AF_VSOCK socket on port.
func microvmVsockListen(port uint32) (*os.File, error) {
	fd, err := unix.Socket(unix.AF_VSOCK, unix.SOCK_STREAM|unix.SOCK_CLOEXEC, 0)
	if err != nil {
		return nil, fmt.Errorf("create vsock socket: %w", microvmIOError(err))
	}
	listener := os.NewFile(uintptr(fd), "vsock")
	if err := unix.Bind(fd, &unix.SockaddrVM{CID: unix.VMADDR_CID_ANY, Port: port}); err != nil {
		_ = listener.Close()
		return nil, fmt.Errorf("bind vsock port: %w", microvmIOError(err))
	}
	if err := unix.Listen(fd, 64); err != nil {
		_ = listener.Close()
		return nil, fmt.Errorf("listen on vsock port: %w", microvmIOError(err))
	}
	return listener, nil
}

// microvmVsockAccept accepts the next connection from the host. A
// connection from any other CID (the guest's own, over loopback) is closed
// unanswered: the protocol carries no credentials, and exec as root must
// not be open to guest processes.
func microvmVsockAccept(listener *os.File) (*MicrovmVsockStream, error) {
	for {
		fd, peer, err := unix.Accept4(int(listener.Fd()), unix.SOCK_CLOEXEC)
		runtime.KeepAlive(listener)
		if err == unix.EINTR {
			continue
		}
		if err != nil {
			return nil, err
		}
		stream := &MicrovmVsockStream{file: os.NewFile(uintptr(fd), "vsock")}
		cid := uint32(0)
		if address, ok := peer.(*unix.SockaddrVM); ok {
			cid = address.CID
		}
		if cid == unix.VMADDR_CID_HOST {
			return stream, nil
		}
		MicrovmKlog(fmt.Sprintf("refused a connection from vsock CID %d", cid))
		_ = stream.Close()
	}
}

// ---------------------------------------------------------------------------
// PID 1

var microvmPoweringOff atomic.Bool

// microvmMachineSystem is the real machine.
type microvmMachineSystem struct{}

// microvmParkForever is Rust's `loop { thread::park() }`.
func microvmParkForever() {
	for {
		time.Sleep(time.Hour)
	}
}

func (microvmMachineSystem) Quiesce() {
	if microvmPoweringOff.Swap(true) {
		// Another path (stop request or main-process exit) is already
		// shutting the machine down.
		microvmParkForever()
	}
	// SIGKILL to every process except this one (PID 1 is exempt).
	_ = unix.Kill(-1, unix.SIGKILL)
	// A process that is still exiting can hold a mount or write to it, so
	// flush and unmount only once every other process is gone.
	for microvmOtherProcessesAlive() {
		time.Sleep(10 * time.Millisecond)
	}
	unix.Sync()
	microvmUnmountDataFilesystems()
	// Remount the root read-only so its journal is clean.
	_ = unix.Mount("", "/", "", unix.MS_REMOUNT|unix.MS_RDONLY, "")
	unix.Sync()
}

func (microvmMachineSystem) PowerOff() {
	// Powers the machine off; the call does not return on success.
	err := unix.Reboot(unix.LINUX_REBOOT_CMD_POWER_OFF)
	MicrovmKlog(fmt.Sprintf("power off failed: %s", microvmIOError(err)))
	microvmParkForever()
}

// microvmOtherProcessesAlive reports whether any process other than PID 1
// is still running. Zombies (already dead, waiting to be reaped) and kernel
// threads do not count.
func microvmOtherProcessesAlive() bool {
	names, err := microvmReadDirNames("/proc")
	if err != nil {
		return false
	}
	for _, name := range names {
		pid, ok := microvmParseU32(name)
		if !ok || pid == 1 || pid == 2 {
			continue
		}
		// /proc/PID/stat is "pid (comm) state ppid ..."; comm may contain ")".
		stat, err := os.ReadFile(filepath.Join("/proc", name, "stat"))
		if err != nil || !utf8.Valid(stat) {
			continue // exited while we looked
		}
		index := strings.LastIndex(string(stat), ") ")
		if index < 0 {
			continue
		}
		fields := strings.Split(string(stat)[index+2:], " ")
		state := fields[0]
		parent, hasParent := "", len(fields) > 1
		if hasParent {
			parent = fields[1]
		}
		if state != "Z" && !(hasParent && parent == "2") {
			return true
		}
	}
	return false
}

// microvmUnmountDataFilesystems unmounts every ext4 filesystem except the
// root, newest first.
func microvmUnmountDataFilesystems() {
	data, err := os.ReadFile("/proc/self/mountinfo")
	if err != nil || !utf8.Valid(data) {
		MicrovmKlog("cannot read mountinfo; leaving filesystems mounted")
		return
	}
	var points []string
	for _, line := range microvmLines(string(data)) {
		before, after, ok := strings.Cut(line, " - ")
		if !ok {
			continue
		}
		fstype, _, _ := strings.Cut(after, " ")
		fields := strings.Split(before, " ")
		if len(fields) < 5 {
			continue
		}
		if mountPoint := fields[4]; fstype == "ext4" && mountPoint != "/" {
			points = append(points, strings.ReplaceAll(mountPoint, "\\040", " "))
		}
	}
	for i := len(points) - 1; i >= 0; i-- {
		point := points[i]
		if strings.ContainsRune(point, 0) {
			continue
		}
		if err := unix.Unmount(point, 0); err != nil {
			MicrovmKlog(fmt.Sprintf("unmount %s failed: %s", point, microvmIOError(err)))
		}
	}
}

// MicrovmRunGuestAgent is `mj-worker microvm guest-agent`: run as PID 1 in
// the guest.
func MicrovmRunGuestAgent() error {
	reaper, err := MicrovmStartReaper()
	if err != nil {
		return err
	}
	agent := NewMicrovmAgent(reaper, microvmMachineSystem{})
	listener, err := microvmVsockListen(MicrovmAgentPort)
	if err != nil {
		return err
	}
	MicrovmKlog("listening")
	for {
		connection, err := microvmVsockAccept(listener)
		if err != nil {
			MicrovmKlog(fmt.Sprintf("accept failed: %s", microvmIOError(err)))
			continue
		}
		go func() {
			if err := agent.Serve(connection); err != nil {
				MicrovmKlog(fmt.Sprintf("connection failed: %s", err))
			}
		}()
	}
}
