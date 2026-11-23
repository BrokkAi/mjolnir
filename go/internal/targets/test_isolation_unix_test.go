// Port of the tests in mj-targets/src/test_isolation.rs (Unix only, as in
// Rust).

//go:build unix

package targets

import (
	"errors"
	"io"
	"net"
	"os"
	"os/exec"
	"path/filepath"
	"testing"
	"time"
)

// Hard-won: #1625: isolated tests leaked surviving process-group members on exit.
func TestIsolatedTestTerminatesItsProcessGroupOnExit(t *testing.T) {
	const name = "TestIsolatedTestTerminatesItsProcessGroupOnExit"
	const socket = "MJ_TEST_ISOLATED_PGID_LEAK_SOCKET"
	if socketPath, ok := os.LookupEnv(socket); ok {
		stream, err := net.Dial("unix", socketPath)
		if err != nil {
			t.Fatalf("connect leak sentinel: %v", err)
		}
		descriptor, err := stream.(*net.UnixConn).File()
		if err != nil {
			t.Fatal(err)
		}
		command := exec.Command("sleep", "600")
		command.Stdin = descriptor
		// Intentionally left running without Wait to verify IsolatedTest
		// teardown.
		if err := command.Start(); err != nil {
			t.Fatalf("spawn sleep in isolated process group: %v", err)
		}
		return
	}
	socketPath := filepath.Join(t.TempDir(), "sentinel.sock")
	listener, err := net.Listen("unix", socketPath)
	if err != nil {
		t.Fatal(err)
	}
	defer listener.Close()
	accepted := make(chan net.Conn, 1)
	go func() {
		stream, err := listener.Accept()
		if err != nil {
			t.Errorf("accept sentinel: %v", err)
			close(accepted)
			return
		}
		accepted <- stream
	}()
	NewIsolatedTest(TestName("targets::test_isolation::tests", name)).
		Env(socket, socketPath).
		Run()
	sentinel, ok := <-accepted
	if !ok {
		t.FailNow()
	}
	defer sentinel.Close()
	// The kernel closes all file descriptors of a killed process even if
	// container PID 1 does not reap zombies, so EOF confirms termination.
	if err := sentinel.SetReadDeadline(time.Now().Add(2 * time.Second)); err != nil {
		t.Fatal(err)
	}
	var byte [1]byte
	read, err := sentinel.Read(byte[:])
	if err != nil && !errors.Is(err, io.EOF) {
		t.Fatalf("isolated process-group member should be terminated on exit: %v", err)
	}
	if read != 0 {
		t.Fatal("sentinel stream must hit EOF once sleep is killed")
	}
}
