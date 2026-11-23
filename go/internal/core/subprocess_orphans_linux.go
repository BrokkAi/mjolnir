// Port of mj-core/src/subprocess/orphans.rs (Linux only, as in Rust).

// A worker reaps the children the kernel reparented to it. Children spawned
// through the subprocess helpers keep their normal wait owners.

package core

import (
	"errors"
	"fmt"
	"io/fs"
	"log/slog"
	"os"
	"os/exec"
	"path/filepath"
	"strconv"
	"strings"
	"sync"
	"sync/atomic"
	"syscall"

	"golang.org/x/sys/unix"
)

type orphanChildren struct {
	mu           sync.Mutex
	spawnsSealed bool
	// registered maps each child spawned through the helpers to its start
	// time in clock ticks; 0 when procfs could not tell it.
	registered map[uint32]uint64
}

// orphanState is set once this process becomes a subreaper (Rust's
// `CHILDREN` OnceLock).
var (
	orphanState     atomic.Pointer[orphanChildren]
	orphanStateInit sync.Once
)

// OrphansActive reports whether this process reaps adopted children.
func OrphansActive() bool { return orphanState.Load() != nil }

// OrphansSealSpawns refuses every later spawn through the helpers.
func OrphansSealSpawns() {
	if children := orphanState.Load(); children != nil {
		children.mu.Lock()
		children.spawnsSealed = true
		children.mu.Unlock()
	}
}

// OrphansBecomeSubreaper makes this process accept reparented descendants.
func OrphansBecomeSubreaper() error {
	if err := unix.Prctl(unix.PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0); err != nil {
		return os.NewSyscallError("prctl", err)
	}
	orphanStateInit.Do(func() {
		orphanState.Store(&orphanChildren{registered: map[uint32]uint64{}})
	})
	return nil
}

// errInvalidStartTime stands for io::ErrorKind::InvalidData.
var errInvalidStartTime = errors.New("invalid process start time")

func orphanStartTicks(pid uint32) (uint64, error) {
	stat, err := os.ReadFile(fmt.Sprintf("/proc/%d/stat", pid))
	if err != nil {
		return 0, err
	}
	// The command name may hold any bytes; only fields after its closing
	// parenthesis are parsed.
	text := string(stat)
	closing := strings.LastIndex(text, ")")
	if closing < 0 {
		return 0, errInvalidStartTime
	}
	fields := strings.Fields(text[closing+1:])
	if len(fields) <= 19 {
		return 0, errInvalidStartTime
	}
	ticks, err := strconv.ParseUint(fields[19], 10, 64)
	if err != nil {
		return 0, errInvalidStartTime
	}
	return ticks, nil
}

// orphanTracked spawns through spawn and registers the child pid reports,
// so the reaper leaves its wait to its owner.
func orphanTracked(spawn func() error, pid func() (uint32, bool)) error {
	children := orphanState.Load()
	if children == nil {
		return spawn()
	}
	children.mu.Lock()
	defer children.mu.Unlock()
	if children.spawnsSealed {
		return errors.New("worker is quiescing its checkout")
	}
	if err := spawn(); err != nil {
		return err
	}
	if id, ok := pid(); ok {
		ticks, err := orphanStartTicks(id)
		switch {
		case err == nil:
			children.registered[id] = ticks
		case errors.Is(err, fs.ErrNotExist):
			// A wait owner already collected this process.
		default:
			// Preserve the wait owner even if procfs is temporarily unreadable.
			slog.Warn("could not read spawned child's start time", "pid", id, "error", err)
			children.registered[id] = 0
		}
	}
	return nil
}

func orphanChildPIDs() ([]uint32, error) {
	threads, err := os.ReadDir("/proc/self/task")
	if err != nil {
		return nil, err
	}
	var result []uint32
	for _, thread := range threads {
		children, err := os.ReadFile(filepath.Join("/proc/self/task", thread.Name(), "children"))
		if errors.Is(err, fs.ErrNotExist) {
			continue
		}
		if err != nil {
			return nil, err
		}
		for _, field := range strings.Fields(string(children)) {
			if pid, err := strconv.ParseUint(field, 10, 32); err == nil {
				result = append(result, uint32(pid))
			}
		}
	}
	return result, nil
}

// OrphansReapAdoptedChildren collects every exited child this process was
// given by the kernel, leaving the children the helpers spawned to their own
// wait owners.
func OrphansReapAdoptedChildren() error {
	children := orphanState.Load()
	if children == nil {
		return errors.New("worker is not a subreaper")
	}
	children.mu.Lock()
	defer children.mu.Unlock()
	direct, err := orphanChildPIDs()
	if err != nil {
		return err
	}
	for _, pid := range direct {
		ticks, err := orphanStartTicks(pid)
		if errors.Is(err, fs.ErrNotExist) {
			continue
		}
		if err != nil {
			return err
		}
		if spawn, ok := children.registered[pid]; ok && (spawn == ticks || spawn == 0) {
			continue
		}
		// waitpid checks that this is our child. WNOHANG never waits for a
		// live child, and nobody owns the wait for this adopted child.
		if _, err := unix.Wait4(int(pid), nil, unix.WNOHANG, nil); err != nil &&
			!errors.Is(err, syscall.ECHILD) && !errors.Is(err, syscall.EINTR) {
			return os.NewSyscallError("waitpid", err)
		}
	}
	// A running child still needs its own wait even if a /proc task list
	// changed while it was enumerated. Remove entries only by identity.
	for pid, ticks := range children.registered {
		now, err := orphanStartTicks(pid)
		if err != nil || (now != ticks && ticks != 0) {
			delete(children.registered, pid)
		}
	}
	return nil
}

// OrphansHasChildren reports whether this process has any child at all,
// without collecting one.
func OrphansHasChildren() (bool, error) {
	for {
		// WNOWAIT leaves ownership with the normal waiter (or the orphan
		// reaper).
		var info unix.Siginfo
		err := unix.Waitid(unix.P_ALL, 0, &info, unix.WEXITED|unix.WNOHANG|unix.WNOWAIT, nil)
		switch {
		case err == nil:
			return true, nil
		case errors.Is(err, syscall.ECHILD):
			return false, nil
		case errors.Is(err, syscall.EINTR):
			continue
		}
		return false, os.NewSyscallError("waitid", err)
	}
}

// spawnChild starts command, registering it with the orphan reaper when this
// process is one.
func spawnChild(command *exec.Cmd) error {
	return orphanTracked(command.Start, func() (uint32, bool) {
		if command.Process == nil {
			return 0, false
		}
		return uint32(command.Process.Pid), true
	})
}
