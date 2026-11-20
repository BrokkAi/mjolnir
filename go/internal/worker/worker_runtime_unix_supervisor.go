// Port of mj-worker/src/worker_runtime/unix/supervisor.rs.
//
// The relay proxy, the ACP supervisor that owns the bridge's process group,
// and the helpers the daemon uses to stop its peers.

//go:build unix

package worker

import (
	"bufio"
	"context"
	"errors"
	"fmt"
	"io"
	"log/slog"
	"os"
	"syscall"
	"time"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// proxyInitialInputTimeout is how long the relay proxy waits for its
// client's first request.
const proxyInitialInputTimeout = 30 * time.Second

// stopPeerAndReturn stops the ACP peer after a failure and returns that
// failure, with any cleanup failure, under contextMessage.
func stopPeerAndReturn(peer *core.JoinHandle[error], shutdown context.CancelFunc, err error, contextMessage string) error {
	shutdown()
	// The peer owns a supervisor process, which in turn owns the harness
	// group. Aborting the peer would kill that owner before it can clean up.
	cleanup, joinErr := peer.Join(context.Background())
	switch {
	case joinErr != nil:
		err = fmt.Errorf("ACP cleanup task failed: %v: %w", joinErr, err)
	case cleanup != nil:
		err = fmt.Errorf("ACP cleanup also failed: %v: %w", cleanup, err)
	}
	return fmt.Errorf("%s: %w", contextMessage, err)
}

// stopFailedCoordinator aborts the relay coordinator (through abort, the
// cancel function of its context) and waits for it.
func stopFailedCoordinator(abort context.CancelFunc, peer *core.JoinHandle[error]) {
	abort()
	result, joinErr := peer.Join(context.Background())
	switch {
	case joinErr != nil:
		slog.Warn("relay coordinator task failed during shutdown", "error", joinErr)
	case result != nil && !errors.Is(result, context.Canceled):
		slog.Warn("relay coordinator failed during shutdown", "error", result)
	}
}

// readBoundedLine reads one relay request line of at most maximumBytes;
// false at the end of the stream.
func readBoundedLine(reader *bufio.Reader, maximumBytes int) (string, bool, error) {
	panic("unported: mj-worker/src/worker_runtime/unix/supervisor.rs read_bounded_line")
}

// workerRootWasRemoved is whether a durable write failed because the worker
// root is gone. A durable write can only fail permanently that way: session
// teardown removed the root under this daemon. Nothing served afterwards
// could ever be persisted, so the daemon has to stop.
func workerRootWasRemoved(body core.RelayResponseBody, root string) bool {
	failure, ok := body.(core.RelayResponseBodyError)
	if !ok || failure.Error.Code != core.RelayErrorCodeInternal {
		return false
	}
	info, err := os.Stat(root)
	return err != nil || !info.IsDir()
}

// proxyWriteHalf is the write half of the relay socket the proxy forwards
// to; CloseWrite is Rust's AsyncWriteExt::shutdown, which ends the write
// direction only.
type proxyWriteHalf interface {
	io.Writer
	CloseWrite() error
}

// forwardProxyStreams carries frames between the proxy's client and the
// worker socket until either side closes.
//
// The proxy must die with its client. Joining both copy directions left the
// process alive forever after stdin EOF (a killed `podman exec` client),
// leaking one thread-heavy process per poll inside the container. Exit as
// soon as either side closes. An idle connection is intentional: it may own
// a checkpoint barrier while the controller transfers a large archive.
// Before the first request, however, a bounded deadline
// (proxyInitialInputTimeout) prevents a detached Podman conmon from holding
// the target-side proxy forever after the controller kills a launcher whose
// handshake timed out.
func forwardProxyStreams(ctx context.Context, clientRead io.Reader, clientWrite io.Writer, relayRead io.Reader, relayWrite proxyWriteHalf) error {
	panic("unported: mj-worker/src/worker_runtime/unix/supervisor.rs forward_proxy_streams")
}

// Proxy carries this process's stdin and stdout to the worker socket under
// root, then exits the process.
func Proxy(ctx context.Context, root string) error {
	panic("unported: mj-worker/src/worker_runtime/unix/supervisor.rs proxy")
}

// RunACPSupervisor owns the ACP bridge's process group. The daemon
// communicates only with this supervisor; if the daemon is killed, stdin
// reaches EOF and the complete bridge process tree is terminated and reaped.
func RunACPSupervisor(ctx context.Context, spec AcpSupervisorSpec) error {
	panic("unported: mj-worker/src/worker_runtime/unix/supervisor.rs run_acp_supervisor")
}

func runACPSupervisorWithStreams(ctx context.Context, spec AcpSupervisorSpec, parentStdin io.Reader, parentStdout io.Writer) error {
	panic("unported: mj-worker/src/worker_runtime/unix/supervisor.rs run_acp_supervisor_with_streams")
}

// LeadProcessGroup makes this process lead its own session, so session
// teardown can stop the whole worker tree with a single process-group
// signal. Failing means the process already leads its group, which is the
// state we wanted.
//
// Only the real daemon entry point may call this: it detaches the caller
// from its controlling terminal.
func LeadProcessGroup() {
	_, _ = syscall.Setsid()
}
