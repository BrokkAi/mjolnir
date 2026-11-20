//go:build windows

// Port of mj-core/src/subprocess.rs: the Windows parts (process groups,
// kill-on-close job objects and detached spawning).

package core

import (
	"errors"
	"fmt"
	"os"
	"os/exec"
	"syscall"
	"unsafe"

	"golang.org/x/sys/windows"
)

// prepareOwnedGroup gives command's child a process group of its own.
func prepareOwnedGroup(command *exec.Cmd) {
	if command.SysProcAttr == nil {
		command.SysProcAttr = &syscall.SysProcAttr{}
	}
	command.SysProcAttr.CreationFlags |= windows.CREATE_NEW_PROCESS_GROUP
}

// killProcessGroup does nothing on Windows: the child's job object owns its
// descendants.
func killProcessGroup(int) {}

// childJob is a Windows job object that kills its processes when closed.
type childJob struct {
	handle uintptr
}

func childJobAttach(process *os.Process) (*childJob, error) {
	handle, err := windows.CreateJobObject(nil, nil)
	if err != nil {
		return nil, fmt.Errorf("create child job: %w", err)
	}
	job := &childJob{handle: uintptr(handle)}
	var limits windows.JOBOBJECT_EXTENDED_LIMIT_INFORMATION
	limits.BasicLimitInformation.LimitFlags = windows.JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE
	if _, err := windows.SetInformationJobObject(handle, windows.JobObjectExtendedLimitInformation,
		uintptr(unsafe.Pointer(&limits)), uint32(unsafe.Sizeof(limits))); err != nil {
		job.close()
		return nil, fmt.Errorf("configure child job: %w", err)
	}
	child, err := windows.OpenProcess(windows.PROCESS_SET_QUOTA|windows.PROCESS_TERMINATE, false, uint32(process.Pid))
	if err != nil {
		job.close()
		return nil, fmt.Errorf("missing child process handle: %w", err)
	}
	defer windows.CloseHandle(child)
	if err := windows.AssignProcessToJobObject(handle, child); err != nil {
		job.close()
		return nil, fmt.Errorf("assign child job: %w", err)
	}
	return job, nil
}

// close is the job's Drop.
func (j *childJob) close() {
	if j == nil || j.handle == 0 {
		return
	}
	_ = windows.CloseHandle(windows.Handle(j.handle))
	j.handle = 0
}

// isBrokenPipe is whether a write failed because the reader is gone.
func isBrokenPipe(err error) bool {
	return errors.Is(err, syscall.EPIPE) || errors.Is(err, windows.ERROR_BROKEN_PIPE) ||
		errors.Is(err, windows.ERROR_NO_DATA)
}

// SignalProcessGroup sends signal to the process group led by pid. Windows
// has no process-group signals.
func SignalProcessGroup(pid int, signal syscall.Signal) error {
	return fmt.Errorf("signalling process group %d is only supported on Unix", pid)
}

func groupSignalErrorIsIgnorable(error) bool { return false }

func spawnDetachedPlatform(command *exec.Cmd) (uint32, error) {
	disinheritStandardHandles()
	if command.SysProcAttr == nil {
		command.SysProcAttr = &syscall.SysProcAttr{}
	}
	// A hidden console and a process group of its own, as setsid gives a
	// Unix child its own session: Ctrl-C in the launcher's terminal and
	// closing it do not reach the child, and the console programs it runs
	// open no window.
	command.SysProcAttr.CreationFlags |= windows.CREATE_NO_WINDOW | windows.CREATE_NEW_PROCESS_GROUP
	if err := command.Start(); err != nil {
		return 0, fmt.Errorf("spawn detached child process: %w", err)
	}
	pid := uint32(command.Process.Pid)
	// Windows has no zombie state: releasing the handle leaves the process
	// running.
	_ = command.Process.Release()
	return pid, nil
}

// disinheritStandardHandles keeps this process's standard handles out of
// every child that is not handed them explicitly. A detached child that
// kept the launcher's stdout pipe would hold it open for its whole life, so
// a script reading `mj daemon restart` output would wait for the daemon to
// exit. Children given these handles still get them, as duplicates.
func disinheritStandardHandles() {
	for _, id := range []uint32{windows.STD_INPUT_HANDLE, windows.STD_OUTPUT_HANDLE, windows.STD_ERROR_HANDLE} {
		handle, err := windows.GetStdHandle(id)
		if err == nil && handle != 0 && handle != windows.InvalidHandle {
			_ = windows.SetHandleInformation(handle, windows.HANDLE_FLAG_INHERIT, 0)
		}
	}
}

func spawnDetachedUnix(*exec.Cmd) (uint32, error) {
	return 0, errors.New("the Unix detached spawn is not available on Windows")
}

func inheritedDescriptorLimit() int { return 3 }

func markInheritedDescriptorsCloexec(int) {}

func markRangeCloexec() bool { return false }

func markListedDescriptorsCloexec() bool { return false }

// raiseOpenFileLimitImpl: Windows has no descriptor limit to raise.
func raiseOpenFileLimitImpl() error { return nil }

func openFileLimitTarget(hard uint64) (uint64, error) { return hard, nil }

// ForegroundGroup is the login terminal's foreground hand-off, which
// Windows does not have.
type ForegroundGroup struct{}

func foregroundGroupPrepare(*exec.Cmd) (*ForegroundGroup, error) { return nil, nil }

// Close restores nothing.
func (g *ForegroundGroup) Close() {}
