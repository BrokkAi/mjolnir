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
	"os/exec"
	"path/filepath"
	"syscall"
	"time"

	"github.com/BrokkAi/mjolnir/go/internal/core"
	"github.com/BrokkAi/mjolnir/go/internal/harnessinstall"
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
	frame, err := core.ReadBoundedFrame(reader, maximumBytes)
	if err != nil {
		var tooLarge core.BoundedFrameErrorTooLarge
		if errors.As(err, &tooLarge) {
			return "", false, errors.New("relay request frame is too large")
		}
		return "", false, fmt.Errorf("read relay request: %w", err)
	}
	var line []byte
	switch frame := frame.(type) {
	case core.BoundedFrameLine:
		line = frame.Bytes
	case core.BoundedFrameTruncated:
		line = frame.Bytes
	default:
		return "", false, nil
	}
	text, err := captureFromUTF8(line)
	if err != nil {
		return "", false, fmt.Errorf("relay request is not UTF-8: %w", err)
	}
	return text, true, nil
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
	// Each direction is read on its own goroutine, as tokio reads both in one
	// select. done stops a reader that has nobody left to hand a chunk to.
	done := make(chan struct{})
	defer close(done)
	clientChunks := proxyReadChunks(clientRead, done)
	initial := time.NewTimer(proxyInitialInputTimeout)
	var first proxyChunk
	select {
	case first = <-clientChunks:
		initial.Stop()
	case <-initial.C:
		slog.Debug("relay proxy client sent no initial frame before the idle deadline",
			"operation", "proxy_initial_input")
		return nil
	case <-ctx.Done():
		initial.Stop()
		return ctx.Err()
	}
	if first.err != nil {
		return fmt.Errorf("read initial proxy stdin: %w", first.err)
	}
	if len(first.data) == 0 {
		if err := relayWrite.CloseWrite(); err != nil {
			slog.Debug("could not close relay socket after proxy client EOF",
				"operation", "proxy_shutdown", "error", err)
		}
		return nil
	}
	if _, err := relayWrite.Write(first.data); err != nil {
		return fmt.Errorf("forward initial request to worker: %w", err)
	}

	relayChunks := proxyReadChunks(relayRead, done)
	for {
		select {
		case <-ctx.Done():
			return ctx.Err()
		case chunk := <-clientChunks:
			if chunk.err != nil {
				return fmt.Errorf("read proxy stdin: %w", chunk.err)
			}
			if len(chunk.data) == 0 {
				// Client is gone; flush any final in-flight response briefly,
				// then exit.
				if err := relayWrite.CloseWrite(); err != nil {
					slog.Debug("could not close relay socket after proxy client EOF",
						"operation", "proxy_shutdown", "error", err)
				}
				proxyForwardFinalResponse(relayChunks, clientWrite)
				return nil
			}
			if _, err := relayWrite.Write(chunk.data); err != nil {
				return fmt.Errorf("forward request to worker: %w", err)
			}
		case chunk := <-relayChunks:
			if chunk.err != nil {
				return fmt.Errorf("read worker socket: %w", chunk.err)
			}
			if len(chunk.data) == 0 {
				return nil
			}
			if _, err := clientWrite.Write(chunk.data); err != nil {
				return fmt.Errorf("forward response to client: %w", err)
			}
			if flusher, ok := clientWrite.(interface{ Flush() error }); ok {
				if err := flusher.Flush(); err != nil {
					return fmt.Errorf("flush proxy stdout: %w", err)
				}
			}
		}
	}
}

// proxyForwardFinalResponse copies what the worker still sends to the client
// for at most 500ms after the client closed its side.
func proxyForwardFinalResponse(relayChunks <-chan proxyChunk, clientWrite io.Writer) {
	deadline := time.NewTimer(500 * time.Millisecond)
	defer deadline.Stop()
	for {
		select {
		case chunk := <-relayChunks:
			if chunk.err != nil || len(chunk.data) == 0 {
				return
			}
			if _, err := clientWrite.Write(chunk.data); err != nil {
				return
			}
		case <-deadline.C:
			slog.Debug("could not forward the final relay response before proxy shutdown",
				"operation", "proxy_final_response", "error", "deadline has elapsed")
			return
		}
	}
}

// proxyChunk is one read of a proxied stream: data, or an empty chunk at the
// end of the stream, or the error that ended it.
type proxyChunk struct {
	data []byte
	err  error
}

// proxyReadChunks reads reader in 64 KiB chunks on its own goroutine until
// the stream ends or done closes.
func proxyReadChunks(reader io.Reader, done <-chan struct{}) <-chan proxyChunk {
	chunks := make(chan proxyChunk)
	go func() {
		deliver := func(chunk proxyChunk) bool {
			select {
			case chunks <- chunk:
				return true
			case <-done:
				return false
			}
		}
		for {
			buffer := make([]byte, 64*1024)
			count, err := reader.Read(buffer)
			if count > 0 && !deliver(proxyChunk{data: buffer[:count]}) {
				return
			}
			if errors.Is(err, io.EOF) {
				deliver(proxyChunk{})
				return
			}
			if err != nil {
				deliver(proxyChunk{err: err})
				return
			}
		}
	}()
	return chunks
}

// Proxy carries this process's stdin and stdout to the worker socket under
// root, then exits the process.
func Proxy(ctx context.Context, root string) error {
	socket := filepath.Join(root, "control.sock")
	stream, err := core.ConnectUnixStream(socket)
	if err != nil {
		return fmt.Errorf("connect worker socket %s: %w", socket, err)
	}
	outcome := forwardProxyStreams(ctx, os.Stdin, os.Stdout, stream, stream)

	// Leave now instead of returning into shutdown. The stdin reader can be
	// blocked in a read that cannot be cancelled, and a client that is
	// waiting for a response keeps the proxy's stdin open. A proxy whose
	// worker socket is gone has nothing left to carry: the client must see
	// this process exit and its stdout close, not a live proxy with no worker
	// behind it. os.Stdout is unbuffered, so there is nothing to flush.
	if outcome == nil {
		os.Exit(0)
	}
	// The client reads this proxy's stderr into its own log.
	slog.Error("relay proxy stopped with an error", "operation", "proxy", "error", outcome.Error())
	os.Exit(1)
	return nil
}

// RunACPSupervisor owns the ACP bridge's process group. The daemon
// communicates only with this supervisor; if the daemon is killed, stdin
// reaches EOF and the complete bridge process tree is terminated and reaped.
func RunACPSupervisor(ctx context.Context, spec AcpSupervisorSpec) error {
	// This hidden subcommand exclusively owns its stdio.
	return runACPSupervisorWithStreams(ctx, spec, os.Stdin, os.Stdout)
}

// supervisorCompletion is what ended the supervisor's forwarding.
type supervisorCompletion int

const (
	supervisorInputEnded supervisorCompletion = iota
	supervisorOutputEnded
	supervisorChildExited
)

func runACPSupervisorWithStreams(ctx context.Context, spec AcpSupervisorSpec, parentStdin io.Reader, parentStdout io.Writer) error {
	if spec.HarnessLease != nil {
		lease, err := harnessinstall.AcquireSupervisorLease(*spec.HarnessLease)
		if err != nil {
			return err
		}
		defer lease.Drop()
	}
	if spec.NodeLease != nil {
		lease, err := harnessinstall.AcquireSupervisorLease(*spec.NodeLease)
		if err != nil {
			return err
		}
		defer lease.Drop()
	}
	environment, err := harnessinstall.WithOverrides(ctx, spec.Environment)
	if err != nil {
		return err
	}
	// Removed after the merge, so a value the target's login environment
	// sets is removed as well (#1160).
	for _, name := range spec.ExcludedEnvironment {
		delete(environment, name)
	}
	command := exec.Command(spec.Command, spec.Args...)
	command.Env = make([]string, 0, len(environment))
	for name, value := range environment {
		if name == "GH_TOKEN" || name == "GITHUB_TOKEN" {
			continue
		}
		command.Env = append(command.Env, name+"="+value)
	}
	command.Dir = spec.Cwd
	command.Stderr = os.Stderr
	command.SysProcAttr = &syscall.SysProcAttr{Setpgid: true}
	stdinRead, childStdin, err := os.Pipe()
	if err != nil {
		return fmt.Errorf("launch supervised ACP bridge %s: %w", spec.Command, err)
	}
	childStdout, stdoutWrite, err := os.Pipe()
	if err != nil {
		_ = stdinRead.Close()
		_ = childStdin.Close()
		return fmt.Errorf("launch supervised ACP bridge %s: %w", spec.Command, err)
	}
	command.Stdin, command.Stdout = stdinRead, stdoutWrite
	err = command.Start()
	_ = stdinRead.Close()
	_ = stdoutWrite.Close()
	if err != nil {
		_ = childStdin.Close()
		_ = childStdout.Close()
		return fmt.Errorf("launch supervised ACP bridge %s: %w", spec.Command, err)
	}
	pid := command.Process.Pid
	group := core.NewProcessGroupGuard(pid)
	defer group.Close()
	defer childStdout.Close()
	exited := make(chan supervisorWaitOutcome, 1)
	go func() {
		err := command.Wait()
		exited <- supervisorWaitOutcome{state: command.ProcessState, err: err}
	}()
	var reaped *supervisorWaitOutcome
	// Rust's kill_on_drop: a supervisor that returns early still reaps its
	// child.
	defer func() {
		if reaped == nil {
			_ = command.Process.Kill()
			<-exited
		}
	}()

	inputEnded := make(chan error, 1)
	go func() {
		_, err := io.Copy(childStdin, parentStdin)
		inputEnded <- err
	}()
	outputEnded := make(chan error, 1)
	go func() {
		_, err := io.Copy(parentStdout, childStdout)
		outputEnded <- err
	}()
	var completion supervisorCompletion
	var forwardingErr error
	var exitStatus *os.ProcessState
	select {
	case err := <-inputEnded:
		completion = supervisorInputEnded
		if err != nil {
			forwardingErr = fmt.Errorf("forward ACP supervisor input: %w", err)
		}
	case err := <-outputEnded:
		completion = supervisorOutputEnded
		if err != nil {
			// A broken parent pipe must still terminate the owned harness
			// group.
			completion = supervisorInputEnded
			forwardingErr = fmt.Errorf("forward ACP supervisor output: %w", err)
		}
	case outcome := <-exited:
		reaped = &outcome
		completion = supervisorChildExited
		if outcome.state == nil {
			completion = supervisorInputEnded
			forwardingErr = fmt.Errorf("wait for supervised ACP bridge: %w", outcome.err)
		} else {
			exitStatus = outcome.state
		}
	}
	if completion != supervisorChildExited {
		closed := make(chan error, 1)
		go func() { closed <- childStdin.Close() }()
		timer := time.NewTimer(time.Second)
		select {
		case err := <-closed:
			timer.Stop()
			if err != nil {
				slog.Debug("could not close ACP bridge stdin before termination",
					"operation", "acp_supervisor_shutdown", "error", err)
			}
		case <-timer.C:
			slog.Warn("timed out closing ACP bridge stdin before termination",
				"operation", "acp_supervisor_shutdown")
		}
	} else {
		_ = childStdin.Close()
	}
	core.TerminateProcessGroup(pid, syscall.SIGTERM)
	bridgeEnded := completion != supervisorInputEnded
	// A wait that already failed has nothing left to reap.
	if completion != supervisorChildExited && reaped == nil {
		timer := time.NewTimer(2 * time.Second)
		select {
		case outcome := <-exited:
			timer.Stop()
			reaped = &outcome
			if outcome.state == nil {
				return fmt.Errorf("wait for supervised ACP bridge: %w", outcome.err)
			}
			exitStatus = outcome.state
		case <-timer.C:
			core.TerminateProcessGroup(pid, syscall.SIGKILL)
			outcome := <-exited
			reaped = &outcome
			if outcome.state == nil {
				return fmt.Errorf("reap supervised ACP bridge: %w", outcome.err)
			}
		}
	}
	// The leader can exit on TERM while a descendant ignores it.
	core.TerminateProcessGroup(pid, syscall.SIGKILL)
	if forwardingErr != nil {
		return forwardingErr
	}
	if bridgeEnded && exitStatus != nil && !exitStatus.Success() {
		return fmt.Errorf("supervised ACP bridge exited with %s", workFactsExitStatus(exitStatus))
	}
	return nil
}

// supervisorWaitOutcome is a reaped child's state, or why waiting for it failed.
type supervisorWaitOutcome struct {
	state *os.ProcessState
	err   error
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
