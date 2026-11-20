// Port of mj-core/src/local_sockets.rs.
//
// Unix domain socket binding and connecting that tolerates long paths.
//
// The kernel copies the socket path into sockaddr_un.sun_path, which holds
// 104 bytes on macOS and 108 on Linux. Worker sockets live under
// <data_dir>/workers/<32-hex session id>/, which routinely exceeds the macOS
// limit even though every individual component is short.
//
// The limit applies to the string handed to the kernel, not to the resolved
// location, so a bare file name always fits. When a path is too long we
// switch the process working directory to the socket's parent, bind or
// connect the bare file name, and switch back. A process-wide mutex
// serializes those switches and the guard restores the previous directory on
// every exit path, so the switch is invisible to the rest of the process.
// Callers must hold no other relative-path assumptions for the duration of
// the call.

package core

import (
	"errors"
	"fmt"
	"log/slog"
	"net"
	"os"
	"path/filepath"
	"runtime"
	"sync"
	"syscall"
)

var cwdSwitch sync.Mutex

// UnixSocketPathLimit is the number of path bytes the platform accepts in a
// Unix socket address: sun_path's length minus the terminating NUL. Zero
// where Unix domain sockets are not supported.
func UnixSocketPathLimit() int {
	if runtime.GOOS == "windows" {
		return 0
	}
	return len(syscall.RawSockaddrUnix{}.Path) - 1
}

// BindUnixListener binds a Unix listener at path, working around sun_path
// length limits.
func BindUnixListener(path string) (*net.UnixListener, error) {
	if runtime.GOOS == "windows" {
		return nil, errors.New("Unix domain sockets require Unix")
	}
	listener, err := withShortSocketName(path, func(short string) (*net.UnixListener, error) {
		listener, err := net.ListenUnix("unix", &net.UnixAddr{Name: short, Net: "unix"})
		if err == nil {
			// Rust leaves the socket file to its owner. Go would unlink the
			// name on Close, and a bare name bound after a directory switch
			// would then resolve against whatever the working directory is.
			listener.SetUnlinkOnClose(false)
		}
		return listener, err
	})
	if err != nil {
		return nil, fmt.Errorf("bind unix socket %s: %w", path, err)
	}
	return listener, nil
}

// ConnectUnixStream connects to a Unix socket at path, working around
// sun_path length limits.
func ConnectUnixStream(path string) (*net.UnixConn, error) {
	if runtime.GOOS == "windows" {
		return nil, errors.New("Unix domain sockets require Unix")
	}
	conn, err := withShortSocketName(path, func(short string) (*net.UnixConn, error) {
		return net.DialUnix("unix", nil, &net.UnixAddr{Name: short, Net: "unix"})
	})
	if err != nil {
		return nil, fmt.Errorf("connect unix socket %s: %w", path, err)
	}
	return conn, nil
}

// withShortSocketName runs act on a socket address short enough for
// sun_path.
//
// Uses path directly when it already fits; otherwise runs act on the bare
// file name with the process working directory moved to path's parent.
func withShortSocketName[T any](path string, act func(string) (T, error)) (T, error) {
	if len(path) <= UnixSocketPathLimit() {
		return act(path)
	}
	var zero T
	parent, name := filepath.Dir(path), filepath.Base(path)
	if name == "" || name == "." || name == string(filepath.Separator) {
		return zero, fmt.Errorf("socket path has no file name: %s", path)
	}
	cwdSwitch.Lock()
	defer cwdSwitch.Unlock()
	guard, err := switchWorkingDirectory(parent)
	if err != nil {
		return zero, err
	}
	defer guard.restore()
	return act(name)
}

// workingDirectoryGuard restores the previous working directory.
type workingDirectoryGuard struct {
	previous string
}

func switchWorkingDirectory(directory string) (*workingDirectoryGuard, error) {
	previous, err := os.Getwd()
	if err != nil {
		return nil, fmt.Errorf("read current working directory: %w", err)
	}
	if err := os.Chdir(directory); err != nil {
		return nil, fmt.Errorf("enter socket directory %s: %w", directory, err)
	}
	return &workingDirectoryGuard{previous: previous}, nil
}

// restore is Rust's Drop for the guard.
func (g *workingDirectoryGuard) restore() {
	if err := os.Chdir(g.previous); err != nil {
		slog.Error("failed to restore working directory after binding a unix socket",
			"directory", g.previous, "error", err)
	}
}
