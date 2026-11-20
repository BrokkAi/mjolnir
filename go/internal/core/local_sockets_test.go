//go:build unix

// Port of the tests in mj-core/src/local_sockets.rs.

package core

import (
	"os"
	"os/exec"
	"path/filepath"
	"testing"
)

// deepDirectory makes nested directories whose joined length is guaranteed
// to exceed sun_path on every supported platform.
func deepDirectory(t *testing.T, base string) string {
	t.Helper()
	directory := base
	for len(directory) < 200 {
		directory = filepath.Join(directory, "nested-directory-component")
	}
	if err := os.MkdirAll(directory, 0o755); err != nil {
		t.Fatalf("create nested directories: %v", err)
	}
	return directory
}

// Hard-won: d1744be0: long Unix socket paths made most macOS and some Linux worker launches fail
func TestBindAndConnectWorkThroughAPathLongerThanSunPath(t *testing.T) {
	// Changing cwd affects every goroutine, including concurrent subprocess
	// launches. Exercise the long-path fallback in its own process.
	const child = "MJ_LONG_SOCKET_PATH_TEST_CHILD"
	if os.Getenv(child) == "" {
		executable, err := os.Executable()
		if err != nil {
			t.Fatal(err)
		}
		command := exec.Command(executable, "-test.run=^TestBindAndConnectWorkThroughAPathLongerThanSunPath$", "-test.v")
		command.Env = append(os.Environ(), child+"=1")
		output, err := command.CombinedOutput()
		if err != nil {
			t.Fatalf("long socket path test failed: %v\n%s", err, output)
		}
		return
	}
	socket := filepath.Join(deepDirectory(t, t.TempDir()), "control.sock")
	if len(socket) <= UnixSocketPathLimit() {
		t.Fatalf("socket path %d is not longer than the limit %d", len(socket), UnixSocketPathLimit())
	}

	before, err := os.Getwd()
	if err != nil {
		t.Fatal(err)
	}
	listener, err := BindUnixListener(socket)
	if err != nil {
		t.Fatalf("bind long path: %v", err)
	}
	defer listener.Close()
	if now, _ := os.Getwd(); now != before {
		t.Fatalf("cwd changed from %s to %s", before, now)
	}

	accepted := make(chan byte, 1)
	failed := make(chan error, 1)
	go func() {
		stream, err := listener.Accept()
		if err != nil {
			failed <- err
			return
		}
		defer stream.Close()
		var b [1]byte
		if _, err := stream.Read(b[:]); err != nil {
			failed <- err
			return
		}
		accepted <- b[0]
	}()

	client, err := ConnectUnixStream(socket)
	if err != nil {
		t.Fatalf("connect long path: %v", err)
	}
	if now, _ := os.Getwd(); now != before {
		t.Fatalf("cwd changed from %s to %s", before, now)
	}
	if _, err := client.Write([]byte{7}); err != nil {
		t.Fatalf("write: %v", err)
	}
	client.Close()

	select {
	case b := <-accepted:
		if b != 7 {
			t.Fatalf("read %d", b)
		}
	case err := <-failed:
		t.Fatal(err)
	}
}
