//go:build linux

// Port of mj-microvm/src/sys.rs.
//
// Host tools and system facts the engine uses.

package microvm

import (
	"errors"
	"fmt"
	"os"
	"os/exec"
	"os/signal"
	"path/filepath"
	"runtime"
	"strconv"
	"strings"
	"sync"
	"sync/atomic"
	"syscall"
	"time"
	"unicode"

	"golang.org/x/sys/unix"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// microvmToolDirectories are searched before PATH: where the host
// bootstrap installs Firecracker and crane.
var microvmToolDirectories = []string{
	"/usr/local/bin",
	"/usr/local/sbin",
	"/usr/sbin",
	"/usr/bin",
	"/sbin",
	"/bin",
}

// MicrovmFindTool locates an installed tool. The engine runs under `sudo`,
// whose PATH is not the caller's.
func MicrovmFindTool(name string) (string, error) {
	for _, directory := range microvmToolDirectories {
		candidate := filepath.Join(directory, name)
		if info, err := os.Stat(candidate); err == nil && info.Mode().IsRegular() {
			return candidate, nil
		}
	}
	return "", fmt.Errorf("%s is not installed on this host (searched %s)", name, strings.Join(microvmToolDirectories, ", "))
}

// MicrovmRunUnchecked runs program and captures its output, whatever its
// exit status.
func MicrovmRunUnchecked(program string, args []string) (core.ProcessOutput, error) {
	output, err := core.RunWithInput(exec.Command(program, args...), nil)
	if err != nil {
		return core.ProcessOutput{}, fmt.Errorf("run %s: %w", program, err)
	}
	return output, nil
}

// MicrovmRunChecked runs program, failing with its error output unless it
// exits zero.
func MicrovmRunChecked(program string, args []string) (core.ProcessOutput, error) {
	output, err := MicrovmRunUnchecked(program, args)
	if err != nil {
		return core.ProcessOutput{}, err
	}
	if !output.Status.Success() {
		return core.ProcessOutput{}, fmt.Errorf("%s failed (%s): %s", program, microvmExitStatus(output.Status), MicrovmTail(microvmLossy(output.Stderr), 20))
	}
	return output, nil
}

// MicrovmRunTool runs a tool found by name.
func MicrovmRunTool(name string, args []string) (core.ProcessOutput, error) {
	program, err := MicrovmFindTool(name)
	if err != nil {
		return core.ProcessOutput{}, err
	}
	return MicrovmRunChecked(program, args)
}

// MicrovmTail is the last lines lines of text.
func MicrovmTail(text string, lines int) string {
	trimmed := strings.TrimRightFunc(text, unicode.IsSpace)
	if trimmed == "" {
		return ""
	}
	all := strings.Split(trimmed, "\n")
	for i, line := range all {
		all[i] = strings.TrimSuffix(line, "\r")
	}
	return strings.Join(all[max(len(all)-lines, 0):], "\n")
}

// MicrovmKernelRelease is the running kernel's release (`uname -r`).
func MicrovmKernelRelease() (string, error) {
	var name unix.Utsname
	if err := unix.Uname(&name); err != nil {
		return "", fmt.Errorf("uname: %w", microvmIOError(err))
	}
	return microvmLossy([]byte(unix.ByteSliceToString(name.Release[:]))), nil
}

// MicrovmIsRoot reports whether the engine runs as root.
func MicrovmIsRoot() bool {
	return os.Geteuid() == 0
}

// MicrovmHostArchitecture is the container platform architecture name for
// this host.
func MicrovmHostArchitecture() (string, error) {
	switch runtime.GOARCH {
	case "amd64":
		return "amd64", nil
	case "arm64":
		return "arm64", nil
	}
	return "", fmt.Errorf("unsupported host architecture %s", runtime.GOARCH)
}

// MicrovmSudoOwner is the user that ran `sudo`, if any: files handed back
// to the caller belong to them.
func MicrovmSudoOwner() *MicrovmOwner {
	uid, err := strconv.ParseUint(os.Getenv("SUDO_UID"), 10, 32)
	if err != nil {
		return nil
	}
	gid, err := strconv.ParseUint(os.Getenv("SUDO_GID"), 10, 32)
	if err != nil {
		return nil
	}
	return &MicrovmOwner{UID: uint32(uid), GID: uint32(gid)}
}

// MicrovmNowRFC3339 is the current UTC time in RFC 3339, to the second.
func MicrovmNowRFC3339() string {
	return time.Now().UTC().Format("2006-01-02T15:04:05Z")
}

// MicrovmScratchSuffix is a unique suffix for scratch names: the process id
// and the time in microseconds.
func MicrovmScratchSuffix() string {
	return fmt.Sprintf("%d-%d", os.Getpid(), time.Now().UnixMicro())
}

// MicrovmInterrupted is set by the termination signal handler during a
// launch, which then rolls back.
var MicrovmInterrupted atomic.Bool

var microvmInterruptHandlers sync.Once

// MicrovmInstallInterruptHandlers turns SIGINT, SIGTERM and SIGHUP into a
// flag the launch loops check, so an interrupted `run` removes what it
// created instead of dying half-built.
func MicrovmInstallInterruptHandlers() {
	microvmInterruptHandlers.Do(func() {
		signals := make(chan os.Signal, 1)
		signal.Notify(signals, syscall.SIGINT, syscall.SIGTERM, syscall.SIGHUP)
		go func() {
			for range signals {
				MicrovmInterrupted.Store(true)
			}
		}()
	})
}

// MicrovmCheckInterrupted fails once a termination signal has arrived.
func MicrovmCheckInterrupted() error {
	if MicrovmInterrupted.Load() {
		return errors.New("interrupted")
	}
	return nil
}

// MicrovmFilesystemBytes is the total bytes of the filesystem that holds
// path.
func MicrovmFilesystemBytes(path string) (uint64, error) {
	var stat unix.Statfs_t
	if err := unix.Statfs(path, &stat); err != nil {
		return 0, fmt.Errorf("statvfs %s: %w", path, microvmIOError(err))
	}
	// statvfs's f_frsize is statfs's, or the block size where that is unset.
	fragment := uint64(stat.Frsize)
	if fragment == 0 {
		fragment = uint64(stat.Bsize)
	}
	return stat.Blocks * fragment, nil
}
