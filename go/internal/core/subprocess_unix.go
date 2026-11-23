//go:build unix

// Port of mj-core/src/subprocess.rs: the Unix parts (process groups, the
// detached double fork, descriptor hygiene, the open-file limit and the
// login terminal's foreground group).

package core

import (
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"log/slog"
	"math"
	"os"
	"os/exec"
	"os/signal"
	"runtime"
	"strconv"
	"strings"
	"syscall"

	"golang.org/x/sys/unix"
)

// prepareOwnedGroup puts command's child in a process group of its own
// (Rust's `process_group(0)`).
func prepareOwnedGroup(command *exec.Cmd) {
	if command.SysProcAttr == nil {
		command.SysProcAttr = &syscall.SysProcAttr{}
	}
	command.SysProcAttr.Setpgid = true
	command.SysProcAttr.Pgid = 0
}

func killProcessGroup(pid int) {
	TerminateProcessGroup(pid, syscall.SIGKILL)
}

// childJob is a Windows job object; Unix owns children by process group.
type childJob struct{}

func childJobAttach(*os.Process) (*childJob, error) { return nil, nil }

// close is the job's Drop.
func (j *childJob) close() {}

// isBrokenPipe is whether a write failed because the reader is gone.
func isBrokenPipe(err error) bool { return errors.Is(err, syscall.EPIPE) }

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
	// The negated pid targets only the process group this process created
	// for its own child.
	err := syscall.Kill(-pid, signal)
	if err == nil || groupSignalErrorIsIgnorable(err) {
		return nil
	}
	return err
}

func groupSignalErrorIsIgnorable(err error) bool {
	if errors.Is(err, syscall.ESRCH) {
		return true
	}
	return runtime.GOOS == "darwin" && errors.Is(err, syscall.EPERM)
}

// spawnDetachedHelperVariable marks the intermediate process of
// SpawnDetached: this program, run again, which starts the real child and
// exits at once. Rust forks inside `pre_exec` for this; Go cannot run code
// between fork and exec, so the intermediate is a fresh copy of the program.
const spawnDetachedHelperVariable = "MJ_GO_SPAWN_DETACHED_INTERMEDIATE"

// The intermediate's extra descriptors: where it reports, and where it reads
// what to start.
const (
	spawnDetachedReportFD = 3
	spawnDetachedSpecFD   = 4
)

// spawnDetachedSpec is what the intermediate starts.
type spawnDetachedSpec struct {
	Path string   `json:"path"`
	Args []string `json:"args"`
	Dir  string   `json:"dir"`
	Env  []string `json:"env"`
}

func init() {
	if os.Getenv(spawnDetachedHelperVariable) == "1" {
		runSpawnDetachedIntermediate()
	}
}

// runSpawnDetachedIntermediate is the intermediate: it starts the real
// program in a session of its own, reports its PID, and leaves at once, so
// the program is reparented to init.
func runSpawnDetachedIntermediate() {
	report := os.NewFile(spawnDetachedReportFD, "detached spawn report")
	spec := os.NewFile(spawnDetachedSpecFD, "detached spawn spec")
	fail := func(errno syscall.Errno, message string) {
		fmt.Fprintf(report, "error %d %s", int(errno), message)
		os.Exit(0)
	}
	encoded, err := io.ReadAll(spec)
	_ = spec.Close()
	var target spawnDetachedSpec
	if err == nil {
		err = json.Unmarshal(encoded, &target)
	}
	if err != nil {
		fail(syscall.EINVAL, "read the detached spawn request: "+err.Error())
	}
	// The long-lived process keeps nothing of its launcher beyond its stdio.
	// A descriptor the launcher held without close-on-exec would otherwise
	// stay open for the daemon's whole life, and whoever reads that pipe
	// would never see EOF.
	markInheritedDescriptorsCloexec(inheritedDescriptorLimit())
	process, err := os.StartProcess(target.Path, target.Args, &os.ProcAttr{
		Dir:   target.Dir,
		Env:   target.Env,
		Files: []*os.File{os.Stdin, os.Stdout, os.Stderr},
		// A new session, and so a new process group, which is the group
		// callers terminate by the returned PID.
		Sys: &syscall.SysProcAttr{Setsid: true},
	})
	if err != nil {
		var errno syscall.Errno
		if !errors.As(err, &errno) {
			errno = syscall.EINVAL
		}
		fail(errno, err.Error())
	}
	fmt.Fprintf(report, "pid %d", process.Pid)
	os.Exit(0)
}

// spawnDetachedPlatform double-forks command and reports the grandchild's
// PID.
//
// The intermediate is this program run again as an ordinary child. It starts
// the real program in a session of its own, reports the program's PID down a
// pipe, and exits at once. The intermediate is waited for here, which
// returns immediately, so this process has nothing left to reap and the
// grandchild is reparented to init. The program's exec error, if any, comes
// back through the same pipe before this returns.
func spawnDetachedPlatform(command *exec.Cmd) (uint32, error) {
	if command.Err != nil {
		return 0, fmt.Errorf("spawn detached child process: %w", command.Err)
	}
	environment := command.Env
	if environment == nil {
		environment = os.Environ()
	}
	encoded, err := json.Marshal(spawnDetachedSpec{
		Path: command.Path, Args: command.Args, Dir: command.Dir, Env: environment,
	})
	if err != nil {
		return 0, fmt.Errorf("spawn detached child process: %w", err)
	}
	reportRead, reportWrite, err := os.Pipe()
	if err != nil {
		return 0, fmt.Errorf("create detached spawn pid pipe: %w", err)
	}
	defer reportRead.Close()
	specRead, specWrite, err := os.Pipe()
	if err != nil {
		_ = reportWrite.Close()
		return 0, fmt.Errorf("create detached spawn pid pipe: %w", err)
	}
	intermediate := exec.Command(spawnDetachedProgram())
	intermediate.Args = []string{"mj-spawn-detached"}
	intermediate.Env = []string{spawnDetachedHelperVariable + "=1"}
	intermediate.Stdin, intermediate.Stdout, intermediate.Stderr = command.Stdin, command.Stdout, command.Stderr
	intermediate.ExtraFiles = []*os.File{reportWrite, specRead}
	// The intermediate leads a group of its own, so it never signals the
	// caller's group.
	intermediate.SysProcAttr = &syscall.SysProcAttr{Setpgid: true}
	spawned := SpawnChild(intermediate)
	// Close this process's ends, so the read below sees EOF instead of
	// blocking if the intermediate died without reporting.
	_ = reportWrite.Close()
	_ = specRead.Close()
	if spawned != nil {
		_ = specWrite.Close()
		return 0, fmt.Errorf("spawn detached child process: %w", spawned)
	}
	_, writeErr := specWrite.Write(encoded)
	_ = specWrite.Close()
	reported, readErr := io.ReadAll(reportRead)
	waitErr := intermediate.Wait()
	var exit *exec.ExitError
	if waitErr != nil && !errors.As(waitErr, &exit) {
		return 0, fmt.Errorf("wait for detached spawn intermediate: %w", waitErr)
	}
	if readErr == nil && len(reported) == 0 {
		readErr = io.ErrUnexpectedEOF
	}
	if readErr != nil {
		return 0, fmt.Errorf("read detached child pid from the spawn intermediate: %w", readErr)
	}
	if !intermediate.ProcessState.Success() {
		return 0, fmt.Errorf("detached spawn intermediate exited with %s, so the child may not have started", intermediate.ProcessState)
	}
	if writeErr != nil && !isBrokenPipe(writeErr) {
		return 0, fmt.Errorf("spawn detached child process: %w", writeErr)
	}
	kind, rest, _ := strings.Cut(string(reported), " ")
	switch kind {
	case "pid":
		pid, err := strconv.ParseUint(rest, 10, 32)
		if err != nil {
			return 0, fmt.Errorf("read detached child pid from the spawn intermediate: %w", err)
		}
		return uint32(pid), nil
	case "error":
		number, message, _ := strings.Cut(rest, " ")
		errno, err := strconv.Atoi(number)
		if err != nil {
			return 0, fmt.Errorf("read detached child pid from the spawn intermediate: %w", err)
		}
		return 0, fmt.Errorf("spawn detached child process: %w",
			&spawnDetachedError{message: message, errno: syscall.Errno(errno)})
	}
	return 0, fmt.Errorf("read detached child pid from the spawn intermediate: unexpected report %q", reported)
}

// spawnDetachedError is the grandchild's exec failure, with its OS error so
// callers can tell "not found" from other failures.
type spawnDetachedError struct {
	message string
	errno   syscall.Errno
}

func (e *spawnDetachedError) Error() string { return e.message }
func (e *spawnDetachedError) Unwrap() error { return e.errno }

// spawnDetachedProgram is this program, run again as the intermediate. On
// Linux /proc/self/exe still names it after an upgrade replaced the file.
func spawnDetachedProgram() string {
	if runtime.GOOS == "linux" {
		return "/proc/self/exe"
	}
	if program, err := os.Executable(); err == nil {
		return program
	}
	return os.Args[0]
}

// disinheritStandardHandles keeps this process's standard handles out of
// every child that is not handed them explicitly (Windows only).
func disinheritStandardHandles() {}

// spawnDetachedUnix double-forks command and reports the grandchild's PID.
func spawnDetachedUnix(command *exec.Cmd) (uint32, error) {
	return spawnDetachedPlatform(command)
}

// inheritedDescriptorLimit is one past the highest descriptor this process
// can hold, for the last-resort per-fd loop in
// markInheritedDescriptorsCloexec.
func inheritedDescriptorLimit() int {
	var limit syscall.Rlimit
	current := int64(math.MaxInt32)
	if err := syscall.Getrlimit(syscall.RLIMIT_NOFILE, &limit); err == nil && uint64(limit.Cur) != rlimitInfinity {
		current = int64(min(uint64(limit.Cur), uint64(math.MaxInt32)))
	}
	return int(max(current, 3))
}

// rlimitInfinity is RLIM_INFINITY.
const rlimitInfinity = ^uint64(0)

// markInheritedDescriptorsCloexec sets FD_CLOEXEC on every descriptor from 3
// upward, so a detached process keeps nothing of its launcher beyond its
// stdio.
//
// The cost must follow the descriptors actually open, not the soft limit:
// RaiseOpenFileLimit lifts that limit to a million or more. The kernel
// shortcut and the descriptor listing both qualify. Only a platform with
// neither pays for the per-descriptor loop up to limit.
func markInheritedDescriptorsCloexec(limit int) {
	if markRangeCloexec() {
		return
	}
	if markListedDescriptorsCloexec() {
		return
	}
	for fd := 3; fd < limit; fd++ {
		// F_SETFD on a closed descriptor fails with EBADF and changes nothing.
		_, _ = unix.FcntlInt(uintptr(fd), unix.F_SETFD, unix.FD_CLOEXEC)
	}
}

// markListedDescriptors marks each descriptor named in directory, a
// listing of this process's open descriptors. False when the listing is
// unavailable.
func markListedDescriptors(directory string) bool {
	entries, err := os.ReadDir(directory)
	if err != nil {
		return false
	}
	for _, entry := range entries {
		fd, err := strconv.Atoi(entry.Name())
		if err != nil || fd < 3 {
			continue
		}
		_, _ = unix.FcntlInt(uintptr(fd), unix.F_SETFD, unix.FD_CLOEXEC)
	}
	return true
}

func raiseOpenFileLimitImpl() error {
	var limit syscall.Rlimit
	if err := syscall.Getrlimit(syscall.RLIMIT_NOFILE, &limit); err != nil {
		return err
	}
	target, err := openFileLimitTarget(uint64(limit.Max))
	if err != nil {
		return err
	}
	if uint64(limit.Cur) >= target {
		return nil
	}
	setRlimitValue(&limit.Cur, target)
	// The hard limit is left unchanged. Setting the limit explicitly also
	// stops Go from restoring the launcher's limit in children, so they
	// inherit this one, as Rust's do.
	return syscall.Setrlimit(syscall.RLIMIT_NOFILE, &limit)
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
	// The root is temporarily behind the interactive helper. Block SIGTTOU
	// only around this ioctl.
	restore := blockSIGTTOU()
	defer restore()
	return unix.IoctlSetPointerInt(int(fd), unix.TIOCSPGRP, group)
}

// foregroundGroupPrepare arranges for command's process group to take the
// login terminal's foreground; nil when there is no controlling terminal.
func foregroundGroupPrepare(command *exec.Cmd) (*ForegroundGroup, error) {
	tty, err := os.OpenFile("/dev/tty", os.O_RDWR, 0)
	if errors.Is(err, syscall.ENXIO) || errors.Is(err, syscall.ENODEV) {
		return nil, nil
	}
	if err != nil {
		return nil, fmt.Errorf("open login terminal: %w", err)
	}
	previous, err := unix.IoctlGetInt(int(tty.Fd()), unix.TIOCGPGRP)
	if err != nil {
		_ = tty.Close()
		return nil, fmt.Errorf("read login terminal foreground group: %w", err)
	}
	if command.SysProcAttr == nil {
		command.SysProcAttr = &syscall.SysProcAttr{}
	}
	// Go's child makes its own group the terminal's foreground with signals
	// still blocked, before exec, as Rust's pre_exec hook does.
	command.SysProcAttr.Foreground = true
	command.SysProcAttr.Ctty = int(tty.Fd())
	return &ForegroundGroup{tty: tty, previous: previous}, nil
}

// Close restores the previous foreground group.
func (g *ForegroundGroup) Close() {
	if g.tty == nil {
		return
	}
	if err := foregroundGroup(g.tty.Fd(), g.previous); err != nil {
		slog.Debug("restore login terminal foreground group", "error", err)
	}
	_ = g.tty.Close()
	g.tty = nil
}

// blockSIGTTOUByIgnoring ignores SIGTTOU process-wide for the duration, on
// systems without a per-thread signal mask in golang.org/x/sys.
func blockSIGTTOUByIgnoring() func() {
	if signal.Ignored(syscall.SIGTTOU) {
		return func() {}
	}
	signal.Ignore(syscall.SIGTTOU)
	return func() { signal.Reset(syscall.SIGTTOU) }
}

// setRlimitValue stores value in an rlimit field, which is unsigned on some
// systems and signed on others.
func setRlimitValue[T int64 | uint64](field *T, value uint64) { *field = T(value) }
