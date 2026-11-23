// Port of mj-worker/src/worker_stop/suspension.rs: the Linux parts.

//go:build linux

package worker

import (
	"fmt"
	"os"
	"path/filepath"
	"strings"
	"syscall"

	"golang.org/x/sys/unix"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// suspendSupported: a Linux worker quiesces its checkout only while it reaps
// the children reparented to it.
func suspendSupported() bool { return core.OrphansActive() }

func sealSuspendSpawns() { core.OrphansSealSpawns() }

func suspendNoChildren() (bool, error) {
	if err := core.OrphansReapAdoptedChildren(); err != nil {
		return false, err
	}
	hasChildren, err := core.OrphansHasChildren()
	return !hasChildren, err
}

// signalVerified signals the process expected describes while it is still
// the same process and stillOwned holds for it.
func signalVerified(expected *processEntry, signalNumber syscall.Signal, stillOwned func(*processEntry) bool) error {
	// First pin the kernel process identity. The subsequent stat and
	// ownership reads can reject a recycled PID, and signalling never uses
	// the number.
	descriptor, err := unix.PidfdOpen(int(expected.pid), 0)
	if err != nil {
		return disappeared(err)
	}
	defer unix.Close(descriptor)
	entry, found, err := processTableLookup(expected.pid)
	if err != nil {
		return err
	}
	if found && entry.isLiveInstanceOf(expected) && stillOwned(&entry) {
		if err := unix.PidfdSendSignal(descriptor, signalNumber, nil, 0); err != nil {
			return disappeared(err)
		}
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
	table, err := processTableList()
	if err != nil {
		return nil, err
	}
	inside := func(path string) bool {
		target, err := os.Readlink(path)
		return err == nil && suspensionPathStartsWith(target, checkout)
	}
	var holders []string
	for i := range table {
		process := &table[i]
		if process.zombie {
			continue
		}
		directory := fmt.Sprintf("/proc/%d", process.pid)
		holdsFiles := inside(filepath.Join(directory, "cwd")) || inside(filepath.Join(directory, "root"))
		if !holdsFiles {
			if entries, err := os.ReadDir(filepath.Join(directory, "fd")); err == nil {
				for _, entry := range entries {
					if inside(filepath.Join(directory, "fd", entry.Name())) {
						holdsFiles = true
						break
					}
				}
			}
		}
		if !holdsFiles {
			continue
		}
		current, found, err := processTableLookup(process.pid)
		if err != nil {
			return nil, err
		}
		if found && current.isLiveInstanceOf(process) {
			// The short command name, never the arguments: an unrelated
			// process's command line can carry credentials, and this text
			// reaches the user's refusal.
			name, _ := os.ReadFile(filepath.Join(directory, "comm"))
			holders = append(holders, fmt.Sprintf("%d %s", process.pid, strings.TrimSpace(strings.ToValidUTF8(string(name), "�"))))
		}
	}
	return holders, nil
}

// suspensionPathStartsWith is Path::starts_with: whole components only.
func suspensionPathStartsWith(path, base string) bool {
	return path == base || strings.HasPrefix(path, strings.TrimSuffix(base, "/")+"/")
}

func markedProcess(pid uint32, sessionID string) bool {
	return sessionMarker(pid, []byte("MJ_SESSION_ID="+sessionID)) == WorkFactsMarkerPresent
}
