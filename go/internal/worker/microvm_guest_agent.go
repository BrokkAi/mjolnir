//go:build linux

// Port of mj-worker/src/microvm/guest_agent.rs.
//
// The guest agent: PID 1 inside a microVM, serving the vsock protocol.
//
// The initrd's `/init` prepares the machine (modules, mounts, network) and
// executes this binary as `mj-worker microvm guest-agent` in the new root.
// The agent reaps children, listens on vsock port MicrovmAgentPort, runs
// exec sessions and copies for the host engine, and powers the VM off when
// the container's main process exits or the engine asks it to stop.

package worker

import (
	"fmt"
	"os"
	"os/exec"
	"sync"
	"sync/atomic"
	"syscall"

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
	panic("unported: mj-worker/src/microvm/guest_agent.rs DirectSpawner::spawn")
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
	panic("unported: mj-worker/src/microvm/guest_agent.rs Reaper::start")
}

func (r *MicrovmReaper) run() {
	panic("unported: mj-worker/src/microvm/guest_agent.rs Reaper::run")
}

// microvmWaitForExitedChild blocks until some child has exited (without
// collecting it); false means the process has no children.
func microvmWaitForExitedChild() (bool, error) {
	panic("unported: mj-worker/src/microvm/guest_agent.rs wait_for_exited_child")
}

// Spawn starts command while holding the reaper's lock, so the reaper
// cannot collect the child before its waiter is registered, or a failed
// exec's child before the spawn has.
func (r *MicrovmReaper) Spawn(command *exec.Cmd) (*MicrovmSpawned, error) {
	panic("unported: mj-worker/src/microvm/guest_agent.rs Reaper::spawn")
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
	panic("unported: mj-worker/src/microvm/guest_agent.rs resolve_user")
}

func microvmResolveUserInGuest(spec string) (MicrovmResolvedUser, error) {
	panic("unported: mj-worker/src/microvm/guest_agent.rs resolve_user_in_guest")
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

// Serve serves one connection: hello, one request, its data frames.
func (a *MicrovmAgent) Serve(transport MicrovmTransport) error {
	panic("unported: mj-worker/src/microvm/guest_agent.rs Agent::serve")
}

func (a *MicrovmAgent) handleStart(sender *MicrovmFrameSender, start MicrovmStartRequest) error {
	panic("unported: mj-worker/src/microvm/guest_agent.rs Agent::handle_start")
}

// startMain starts the main process. Until release is called, the process
// monitor is held back from powering the machine off.
func (a *MicrovmAgent) startMain(start MicrovmStartRequest) (pid uint32, release func(), err error) {
	panic("unported: mj-worker/src/microvm/guest_agent.rs Agent::start_main")
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
	panic("unported: mj-worker/src/microvm/guest_agent.rs Agent::handle_stop")
}

// spawnProcess builds and spawns the command for an exec request (or the
// main process). stdin, stdout and stderr are the child's ends; nil is
// `/dev/null`.
func (a *MicrovmAgent) spawnProcess(request MicrovmExecRequest, stdin, stdout, stderr *os.File) (*MicrovmSpawned, *microvmSpawnFailure) {
	panic("unported: mj-worker/src/microvm/guest_agent.rs Agent::spawn_process")
}

func (a *MicrovmAgent) handleExec(sender *MicrovmFrameSender, frames *MicrovmFrameReader, request MicrovmExecRequest) error {
	panic("unported: mj-worker/src/microvm/guest_agent.rs Agent::handle_exec")
}

func (a *MicrovmAgent) handleCopyIn(sender *MicrovmFrameSender, frames *MicrovmFrameReader, destination string, mode MicrovmCopyMode) error {
	panic("unported: mj-worker/src/microvm/guest_agent.rs Agent::handle_copy_in")
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
	panic("unported: mj-worker/src/microvm/guest_agent.rs Agent::handle_copy_out")
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
	panic("unported: mj-worker/src/microvm/guest_agent.rs resolve_program")
}

func microvmEnsureRuntimeDirectory(directory string, user MicrovmResolvedUser) error {
	panic("unported: mj-worker/src/microvm/guest_agent.rs ensure_runtime_directory")
}

// microvmChownTree is Podman's `U` volume option: change the owner of
// everything below root, without following symlinks and without crossing
// into other filesystems.
func microvmChownTree(root string, uid uint32, gid uint32) error {
	panic("unported: mj-worker/src/microvm/guest_agent.rs chown_tree")
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
	panic("unported: mj-worker/src/microvm/guest_agent.rs write_stdin")
}

func microvmEventfd() (*os.File, error) {
	fd, err := unix.Eventfd(0, unix.EFD_CLOEXEC)
	if err != nil {
		return nil, fmt.Errorf("create eventfd: %w", microvmIOError(err))
	}
	return os.NewFile(uintptr(fd), "eventfd"), nil
}

func microvmWakePumps(wake *os.File) {
	panic("unported: mj-worker/src/microvm/guest_agent.rs wake_pumps")
}

// microvmPump copies a child's output into frames until the pipe closes, or
// until the child has exited and what it left in the pipe is drained. A
// descendant that keeps the pipe open after the process exits must not hold
// the exec session open.
func microvmPump(source *os.File, wake *os.File, kind MicrovmFrameKind, sender *MicrovmFrameSender) error {
	panic("unported: mj-worker/src/microvm/guest_agent.rs pump")
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
	panic("unported: mj-worker/src/microvm/guest_agent.rs VsockStream::shutdown")
}

// microvmVsockListen returns a listening AF_VSOCK socket on port.
func microvmVsockListen(port uint32) (*os.File, error) {
	panic("unported: mj-worker/src/microvm/guest_agent.rs vsock_listen")
}

// microvmVsockAccept accepts the next connection from the host. A
// connection from any other CID (the guest's own, over loopback) is closed
// unanswered: the protocol carries no credentials, and exec as root must
// not be open to guest processes.
func microvmVsockAccept(listener *os.File) (*MicrovmVsockStream, error) {
	panic("unported: mj-worker/src/microvm/guest_agent.rs vsock_accept")
}

// ---------------------------------------------------------------------------
// PID 1

var microvmPoweringOff atomic.Bool

// microvmMachineSystem is the real machine.
type microvmMachineSystem struct{}

func (microvmMachineSystem) Quiesce() {
	panic("unported: mj-worker/src/microvm/guest_agent.rs MachineSystem::quiesce")
}

func (microvmMachineSystem) PowerOff() {
	panic("unported: mj-worker/src/microvm/guest_agent.rs MachineSystem::power_off")
}

// microvmOtherProcessesAlive reports whether any process other than PID 1
// is still running. Zombies (already dead, waiting to be reaped) and kernel
// threads do not count.
func microvmOtherProcessesAlive() bool {
	panic("unported: mj-worker/src/microvm/guest_agent.rs other_processes_alive")
}

// microvmUnmountDataFilesystems unmounts every ext4 filesystem except the
// root, newest first.
func microvmUnmountDataFilesystems() {
	panic("unported: mj-worker/src/microvm/guest_agent.rs unmount_data_filesystems")
}

// MicrovmRunGuestAgent is `mj-worker microvm guest-agent`: run as PID 1 in
// the guest.
func MicrovmRunGuestAgent() error {
	panic("unported: mj-worker/src/microvm/guest_agent.rs run_guest_agent")
}
