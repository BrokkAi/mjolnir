// Port of mj-worker/src/worker_runtime/root_owner.rs.

// Exclusive ownership of every mutable file in a worker root. The Unix-only
// methods are in worker_runtime_root_owner_unix.go.

package worker

import (
	"errors"
	"fmt"
	"io/fs"
	"os"
	"path/filepath"
	"runtime"
	"time"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// WorkerRootOwner holds a worker root's `worker.lock` for as long as it
// lives. Release is Rust's Drop.
type WorkerRootOwner struct {
	root string
	file *core.FileLock
}

const probeHoldAllowance = 250 * time.Millisecond

// WorkerRootOwnerIsHeld reports whether a worker holds the lock of `root`
// right now. A missing lock file means no worker has ever run there, which
// is the same answer. The probe takes a shared lock and drops it at once, so
// concurrent probes agree and only a starting worker could notice it (see
// WorkerRootOwnerAcquire).
func WorkerRootOwnerIsHeld(root string) (bool, error) {
	path := filepath.Join(root, "worker.lock")
	file, err := os.Open(path)
	if errors.Is(err, fs.ErrNotExist) {
		return false, nil
	}
	if err != nil {
		return false, fmt.Errorf("open %s: %w", path, err)
	}
	defer file.Close()
	locked, err := rootOwnerTryLock(file, true)
	if err != nil {
		return false, fmt.Errorf("probe %s: %w", path, err)
	}
	if !locked {
		return true, nil
	}
	if err := rootOwnerUnlock(file); err != nil {
		return false, fmt.Errorf("release the worker lock probe: %w", err)
	}
	return false, nil
}

// WorkerRootOwnerAcquire creates the root if needed and takes its exclusive
// lock, refusing a root another worker (or a legacy worker's control
// socket) already owns.
func WorkerRootOwnerAcquire(root string) (*WorkerRootOwner, error) {
	if err := core.CreateOwnerOnlyDirectory(root); err != nil {
		return nil, fmt.Errorf("create worker root %s: %w", root, err)
	}
	root, err := workerRuntimeCanonicalize(root)
	if err != nil {
		return nil, err
	}
	file, err := os.OpenFile(filepath.Join(root, "worker.lock"), os.O_RDWR|os.O_CREATE, 0o666)
	if err != nil {
		return nil, err
	}
	// A liveness probe (`is_held`) takes the lock for the length of two
	// system calls. A worker that starts at that instant sees a held lock
	// that is not a worker's, so it asks again before concluding that
	// another worker owns the root. The window is far longer than a probe
	// holds the lock and short enough that a real owner is still reported
	// at once.
	deadline := time.Now().Add(probeHoldAllowance)
	for {
		locked, err := rootOwnerTryLock(file, false)
		if err != nil {
			file.Close()
			return nil, fmt.Errorf("lock worker root: %w", err)
		}
		if locked {
			break
		}
		if !time.Now().Before(deadline) {
			file.Close()
			return nil, fmt.Errorf("a worker already owns %s", root)
		}
		time.Sleep(5 * time.Millisecond)
	}
	owner := &WorkerRootOwner{root: root, file: core.NewFileLock(file)}
	// Legacy workers do not hold worker.lock. Never take their files away
	// during the first upgrade to a worker that enforces root ownership.
	if runtime.GOOS != "windows" {
		if conn, err := core.ConnectUnixStream(filepath.Join(root, "control.sock")); err == nil {
			conn.Close()
			owner.Release()
			return nil, fmt.Errorf("a worker is already running at %s", root)
		}
	}
	return owner, nil
}

// Root is the canonical worker root.
func (o *WorkerRootOwner) Root() string { return o.root }

// Release gives up ownership of the root (Rust's Drop of the lock): the lock
// is released explicitly, so a forked copy of the descriptor cannot keep it,
// and the descriptor is closed.
func (o *WorkerRootOwner) Release() {
	if o.file == nil {
		return
	}
	file := o.file.File()
	o.file.Drop()
	o.file = nil
	file.Close()
}

// workerRuntimeCanonicalize is Rust's std::fs::canonicalize: the absolute
// path with every symbolic link resolved. An empty path names no file, as
// in Rust.
func workerRuntimeCanonicalize(path string) (string, error) {
	if path == "" {
		return "", &fs.PathError{Op: "canonicalize", Path: path, Err: fs.ErrNotExist}
	}
	absolute, err := filepath.Abs(path)
	if err != nil {
		return "", err
	}
	return filepath.EvalSymlinks(absolute)
}
