// Port of mj-worker/src/worker_runtime/root_owner.rs: the `cfg(unix)`
// methods (descriptor inheritance across the login re-exec and the startup
// log redirection), and Rust's File::try_lock and unlock (flock(2)).

//go:build unix

package worker

import (
	"errors"
	"fmt"
	"io/fs"
	"os"
	"path/filepath"
	"syscall"

	"github.com/BrokkAi/mjolnir/go/internal/core"
	"golang.org/x/sys/unix"
)

// rootOwnerTryLock takes file's lock without waiting; false when another
// open file description holds a conflicting one.
func rootOwnerTryLock(file *os.File, shared bool) (bool, error) {
	how := unix.LOCK_EX
	if shared {
		how = unix.LOCK_SH
	}
	var err error
	for {
		err = unix.Flock(int(file.Fd()), how|unix.LOCK_NB)
		if !errors.Is(err, unix.EINTR) {
			break
		}
	}
	if errors.Is(err, unix.EWOULDBLOCK) {
		return false, nil
	}
	return err == nil, err
}

// rootOwnerUnlock is Rust's File::unlock.
func rootOwnerUnlock(file *os.File) error {
	return unix.Flock(int(file.Fd()), unix.LOCK_UN)
}

// SetReexecInheritance marks the lock descriptor inheritable or not. The
// descriptor is inherited only by login re-exec, never by harnesses.
func (o *WorkerRootOwner) SetReexecInheritance(inherit bool) (int, error) {
	fd := int(o.file.File().Fd())
	flags := unix.FD_CLOEXEC
	if inherit {
		flags = 0
	}
	if _, err := unix.FcntlInt(uintptr(fd), unix.F_SETFD, flags); err != nil {
		return 0, fmt.Errorf("set worker lock inheritance: %w", err)
	}
	return fd, nil
}

// WorkerRootOwnerFromReexec recovers the descriptor carried by the worker's
// own login re-exec.
func WorkerRootOwnerFromReexec(root string, fd int) (*WorkerRootOwner, error) {
	if fd < 3 {
		return nil, errors.New("invalid worker root lock descriptor")
	}
	// Check existence before constructing the descriptor's owning File.
	if _, err := unix.FcntlInt(uintptr(fd), unix.F_GETFD, 0); err != nil {
		return nil, errors.New("worker root lock descriptor was not inherited")
	}
	// The hidden CLI argument transfers this descriptor exactly once.
	file := os.NewFile(uintptr(fd), filepath.Join(root, "worker.lock"))
	lock := core.NewFileLock(file)
	release := func() {
		lock.Drop()
		file.Close()
	}
	root, err := workerRuntimeCanonicalize(root)
	if err != nil {
		release()
		return nil, err
	}
	actual, err := file.Stat()
	if err != nil {
		release()
		return nil, err
	}
	expected, err := os.Stat(filepath.Join(root, "worker.lock"))
	if err != nil {
		release()
		return nil, err
	}
	actualStat, actualOK := actual.Sys().(*syscall.Stat_t)
	expectedStat, expectedOK := expected.Sys().(*syscall.Stat_t)
	if !actualOK || !expectedOK || actualStat.Dev != expectedStat.Dev || actualStat.Ino != expectedStat.Ino {
		release()
		return nil, errors.New("inherited worker lock belongs to another root")
	}
	locked, err := rootOwnerTryLock(file, false)
	if err == nil && !locked {
		err = unix.EWOULDBLOCK
	}
	if err != nil {
		release()
		return nil, fmt.Errorf("retain inherited worker root ownership: %w", err)
	}
	owner := &WorkerRootOwner{root: root, file: lock}
	if _, err := owner.SetReexecInheritance(false); err != nil {
		owner.Release()
		return nil, err
	}
	return owner, nil
}

// PrepareStartup clears the previous diagnostics and redirects stderr into
// worker.log. Only the owner may replace the shared diagnostic log or
// startup records.
func (o *WorkerRootOwner) PrepareStartup(reexec bool) error {
	if !reexec {
		for _, name := range []string{core.WorkerExitFile, core.WorkerStartupFile} {
			err := os.Remove(filepath.Join(o.root, name))
			if err != nil && !errors.Is(err, fs.ErrNotExist) {
				core.DiagnosticStderr("Mjolnir: could not clear worker diagnostic %s: %v", name, err)
			}
		}
	}
	flags := os.O_CREATE | os.O_WRONLY
	if reexec {
		flags |= os.O_APPEND
	} else {
		flags |= os.O_TRUNC
	}
	log, err := os.OpenFile(filepath.Join(o.root, "worker.log"), flags, 0o666)
	if err != nil {
		core.DiagnosticStderr("Mjolnir: could not open worker.log; retaining inherited stderr: %v", err)
		return nil
	}
	defer log.Close()
	// dup2 installs the open log as stderr; log remains valid here.
	if err := unix.Dup2(int(log.Fd()), unix.Stderr); err != nil {
		core.DiagnosticStderr("Mjolnir: could not redirect worker log: %v", err)
	}
	return nil
}
