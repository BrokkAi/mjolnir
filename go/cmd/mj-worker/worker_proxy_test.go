// Port of mj-worker-bin/tests/worker_proxy_exit.rs and
// mj-worker-bin/tests/worker_proxy_long_root.rs.

//go:build unix

package main

import (
	"errors"
	"io"
	"net"
	"os"
	"os/exec"
	"path/filepath"
	"testing"
	"time"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// reapedChild is a real child process the test kills however it ends.
type reapedChild struct {
	// done is closed once the child has exited; err is then its Wait.
	done chan struct{}
	err  error
}

// startReaped starts command and kills it when the test ends.
func startReaped(t *testing.T, command *exec.Cmd) *reapedChild {
	t.Helper()
	if err := core.SpawnChild(command); err != nil {
		t.Fatalf("start %s: %v", command.Path, err)
	}
	child := &reapedChild{done: make(chan struct{})}
	go func() {
		child.err = command.Wait()
		close(child.done)
	}()
	t.Cleanup(func() {
		_ = command.Process.Kill()
		<-child.done
	})
	return child
}

// acceptWithin accepts one connection, failing after wait, or as soon as
// child has exited when it is not nil.
func acceptWithin(t *testing.T, listener *net.UnixListener, wait time.Duration, child *reapedChild, failure string) *net.UnixConn {
	t.Helper()
	deadline := time.Now().Add(wait)
	for {
		if err := listener.SetDeadline(time.Now().Add(25 * time.Millisecond)); err != nil {
			t.Fatal(err)
		}
		connection, err := listener.AcceptUnix()
		if err == nil {
			return connection
		}
		if !errors.Is(err, os.ErrDeadlineExceeded) {
			t.Fatalf("accept the relay proxy connection: %v", err)
		}
		if child != nil {
			select {
			case <-child.done:
				t.Fatalf("the relay proxy exited before connecting: %v", child.err)
			default:
			}
		}
		if !time.Now().Before(deadline) {
			t.Fatal(failure)
		}
	}
}

// A relay proxy whose worker dies must exit, even while its client still
// holds the proxy's stdin open.
//
// Before the fix the proxy finished forwarding and then hung in runtime
// shutdown: the global stdin reader owns a blocking thread whose read cannot
// be cancelled, and the controller does not close the proxy's stdin while it
// waits for a reply. The proxy stayed alive with no worker behind it, its
// stdout never closed, and the controller waited out its full relay timeout
// before noticing.
// Hard-won: 5dc31b4: a dead worker left its proxy waiting for the 900-second relay timeout.
func TestAProxyExitsWhenItsWorkerDiesWhileItsClientHoldsStdinOpen(t *testing.T) {
	const acceptWait = 10 * time.Second
	const exitWait = 10 * time.Second
	root := t.TempDir()
	listener, err := net.ListenUnix("unix", &net.UnixAddr{Name: filepath.Join(root, "control.sock"), Net: "unix"})
	if err != nil {
		t.Fatalf("bind a fake control socket: %v", err)
	}
	command := exec.Command(workerExecutable, "worker", "proxy", "--root", root)
	command.Env = append(os.Environ(),
		"MJ_DATA_DIR="+filepath.Join(root, "data"),
		"MJ_CONFIG_DIR="+filepath.Join(root, "config"))
	clientStdin, err := command.StdinPipe()
	if err != nil {
		t.Fatal(err)
	}
	// Hold the read end open: a closed stdout would end the proxy for the
	// wrong reason.
	clientStdout, err := command.StdoutPipe()
	if err != nil {
		t.Fatal(err)
	}
	defer clientStdout.Close()
	proxy := startReaped(t, command)

	worker := acceptWithin(t, listener, acceptWait, nil, "relay proxy never connected to control.sock")
	if err := worker.SetReadDeadline(time.Now().Add(acceptWait)); err != nil {
		t.Fatal(err)
	}
	// The proxy forwards nothing until its client speaks first.
	if _, err := clientStdin.Write([]byte("hello\n")); err != nil {
		t.Fatalf("send the first request: %v", err)
	}
	request := make([]byte, 6)
	if _, err := io.ReadFull(worker, request); err != nil {
		t.Fatalf("the proxy forwards the first request: %v", err)
	}
	if string(request) != "hello\n" {
		t.Fatalf("forwarded %q", request)
	}

	// The worker dies. Its client does not: it still owns the proxy's
	// stdin, waiting for a response that will never arrive.
	_ = worker.Close()
	_ = listener.Close()

	select {
	case <-proxy.done:
		if proxy.err != nil {
			t.Fatalf("a proxy that outlives its worker exits cleanly, got %v", proxy.err)
		}
	case <-time.After(exitWait):
		t.Fatal("relay proxy stayed alive after its worker closed the control socket")
	}
	_ = clientStdin.Close()
}

// A relay proxy must reach a worker whose root is longer than `sun_path`.
//
// macOS caps a Unix socket address at 104 bytes. Worker roots under
// `<data_dir>/workers/<32-hex session id>/` pass that routinely, and the
// proxy used to exit with "path must be shorter than SUN_LEN" instead of
// connecting.
// Hard-won: d1744be: a worker socket could not connect when its root exceeded the platform socket path limit.
func TestAProxyConnectsToAWorkerRootLongerThanSunPath(t *testing.T) {
	const acceptWait = 10 * time.Second
	temp := t.TempDir()
	root := temp
	for len(filepath.Join(root, "control.sock")) <= 120 {
		root = filepath.Join(root, "nested-worker-root-component")
	}
	if err := os.MkdirAll(root, 0o777); err != nil {
		t.Fatalf("create the nested worker root: %v", err)
	}
	socket := filepath.Join(root, "control.sock")
	if len(socket) <= core.UnixSocketPathLimit() {
		t.Fatal("the test root must be long enough to need the relative-name connect")
	}
	listener, err := core.BindUnixListener(socket)
	if err != nil {
		t.Fatalf("bind a fake control socket: %v", err)
	}
	defer listener.Close()

	command := exec.Command(workerExecutable, "worker", "proxy", "--root", root)
	command.Env = append(os.Environ(),
		"MJ_DATA_DIR="+filepath.Join(temp, "data"),
		"MJ_CONFIG_DIR="+filepath.Join(temp, "config"))
	if _, err := command.StdinPipe(); err != nil {
		t.Fatal(err)
	}
	stdout, err := command.StdoutPipe()
	if err != nil {
		t.Fatal(err)
	}
	defer stdout.Close()
	proxy := startReaped(t, command)

	worker := acceptWithin(t, listener, acceptWait, proxy, "the relay proxy never connected to a long control.sock path")
	_ = worker.Close()
}
