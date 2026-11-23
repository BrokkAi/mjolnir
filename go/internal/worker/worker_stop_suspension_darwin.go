// Port of mj-worker/src/worker_stop/suspension.rs: the macOS parts, and
// has_marker from suspension/darwin_environment.rs.

//go:build darwin

package worker

import (
	"errors"
	"fmt"
	"os/exec"
	"path/filepath"
	"strings"
	"syscall"

	"golang.org/x/sys/unix"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

func suspendSupported() bool { return true }

func sealSuspendSpawns() {}

func suspendNoChildren() (bool, error) { return true, nil }

// signalVerified signals the process expected describes while it is still
// the same process and stillOwned holds for it. macOS has no pidfd, so
// ownership and birth are rechecked immediately before kill.
func signalVerified(expected *processEntry, signalNumber syscall.Signal, stillOwned func(*processEntry) bool) error {
	entry, found, err := processTableLookup(expected.pid)
	if err != nil {
		return err
	}
	if !found || !entry.isLiveInstanceOf(expected) || !stillOwned(&entry) {
		return nil
	}
	again, found, err := processTableLookup(expected.pid)
	if err != nil {
		return err
	}
	if !found || !again.isLiveInstanceOf(expected) {
		return nil
	}
	if err := syscall.Kill(int(expected.pid), signalNumber); err != nil {
		return disappeared(err)
	}
	return nil
}

func checkoutHolders(checkout string) ([]string, error) {
	checkout, err := filepath.EvalSymlinks(checkout)
	if err != nil {
		return nil, err
	}
	checkout, err = filepath.Abs(checkout)
	if err != nil {
		return nil, err
	}
	output, err := core.RunWithInput(exec.Command("/usr/sbin/lsof", "-nP", "-Fpc", "+D", checkout), nil)
	if err != nil {
		return nil, err
	}
	if output.Status.ExitCode() == 1 && len(output.Stdout) == 0 && len(output.Stderr) == 0 {
		return nil, nil
	}
	if !output.Status.Success() {
		return nil, fmt.Errorf("cannot check open files in %s: %s", checkout,
			strings.TrimSpace(strings.ToValidUTF8(string(output.Stderr), "�")))
	}
	text := string(output.Stdout)
	if !utf8Valid(text) {
		return nil, errors.New("lsof output is not UTF-8")
	}
	return relayRustLines(text), nil
}

func utf8Valid(text string) bool { return strings.ToValidUTF8(text, "") == text }

func markedProcess(pid uint32, sessionID string) bool {
	return darwinHasMarker(pid, []byte("MJ_SESSION_ID="+sessionID))
}

// darwinHasMarker reads the raw Darwin environment for signalling. The
// presentation produced by `ps -E` joins arguments and environment values
// with spaces.
func darwinHasMarker(pid uint32, marker []byte) bool {
	// SysctlRaw sizes the buffer as Rust's KERN_ARGMAX read does.
	arguments, err := unix.SysctlRaw("kern.procargs2", int(pid))
	if err != nil {
		return false
	}
	return containsMarker(arguments, marker)
}
