// Port of mj-accounts/src/login_terminal.rs.

// Daemon-owned disposable login terminals. No worker state or handoff
// admission.

package accounts

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"log/slog"
	"net"
	"os/exec"
	"sync"
	"time"

	"github.com/BrokkAi/mjolnir/go/internal/client"
	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// loginProcess is the login root and whether this owner still has to stop
// it.
type loginProcess struct {
	child *exec.Cmd
	armed bool
}

// terminate kills the root's process group, once. The root is not reaped
// until cleanup, so its process-group id cannot be recycled while this owner
// still has terminal reader/writer tasks.
func (p *loginProcess) terminate() {
	if !p.armed {
		return
	}
	p.armed = false
	loginKillGroup(p.child.Process.Pid)
	if err := p.child.Process.Kill(); err != nil {
		slog.Debug("terminate login terminal child", "error", err)
	}
}

// hangup wakes the root's cleanup owner before waiting for its exit:
// reader/writer duplicates keep the master open. Unix only; a no-op
// elsewhere.
func (p *loginProcess) hangup() {
	if p.child != nil && p.child.Process != nil {
		loginHangup(p.child.Process.Pid)
	}
}

// release is Rust's Drop: an owner abandoned while armed hangs up the root
// and transfers its cleanup to an independent goroutine, never blocking the
// daemon or killing the root first.
func (p *loginProcess) release() {
	if !p.armed {
		return
	}
	p.hangup()
	child := p.child
	p.child = nil
	if child == nil {
		return
	}
	// Connection tasks are disposable during handoff. Transfer cleanup to an
	// independent owner; never block the daemon or kill the root first.
	go func() {
		loginAbandonedCleanup(child.Process.Pid)
		if _, err := loginWaitChild(child); err != nil {
			slog.Warn("reap abandoned login child", "error", err)
		}
	}()
}

// loginPty is the master side of the login terminal: its output, its input
// and its size.
type loginPty interface {
	io.ReadWriteCloser
	Resize(width, height int) error
}

// loginMaster holds the PTY master until it is closed. On Windows closing
// it is moved off the caller (Rust's Drop): ConPTY close can wait for the
// login close handler.
type loginMaster struct {
	mu     sync.Mutex
	master loginPty
}

// take removes the master, leaving nothing for a later close.
func (m *loginMaster) take() loginPty {
	m.mu.Lock()
	defer m.mu.Unlock()
	master := m.master
	m.master = nil
	return master
}

// release is Rust's Drop for Master.
func (m *loginMaster) release() {
	master := m.take()
	if master == nil {
		return
	}
	if loginConsoleClosesOnExit {
		// A connection task may be aborted during handoff. ConPTY close can
		// wait for the login close handler, so keep it off this goroutine.
		go func() { _ = master.Close() }()
		return
	}
	_ = master.Close()
}

// closeConsoleOnExit closes the console when the root exits, on Windows:
// closing ConPTY sends CTRL_CLOSE_EVENT. The login handler retains cleanup
// ownership until preparation groups stop. Output is drained concurrently:
// ClosePseudoConsole can emit a final frame and block. Nil elsewhere.
// https://learn.microsoft.com/en-us/windows/console/closepseudoconsole
func closeConsoleOnExit(master *loginMaster) *core.JoinHandle[struct{}] {
	if !loginConsoleClosesOnExit {
		return nil
	}
	console := master.take()
	return core.SpawnBlocking(func() struct{} {
		if console != nil {
			_ = console.Close()
		}
		return struct{}{}
	})
}

// loginTerminalSize is the PTY size for cols and rows.
func loginTerminalSize(cols, rows uint16) (width, height int) {
	return int(cols), int(rows)
}

// loginQueue is an unbounded channel: pushes never wait for the receiver.
// The receiver waits on ready and takes everything queued at once.
type loginQueue[T any] struct {
	mu    sync.Mutex
	items []T
	// closed: the sender is gone (Rust: every Sender dropped).
	closed bool
	// abandoned: the receiver is gone, so pushes fail (Rust: the Receiver
	// dropped).
	abandoned bool
	ready     chan struct{}
}

func newLoginQueue[T any]() *loginQueue[T] {
	return &loginQueue[T]{ready: make(chan struct{}, 1)}
}

func (q *loginQueue[T]) signal() {
	select {
	case q.ready <- struct{}{}:
	default:
	}
}

// push queues item; false when the receiver is gone.
func (q *loginQueue[T]) push(item T) bool {
	q.mu.Lock()
	defer q.mu.Unlock()
	if q.abandoned {
		return false
	}
	q.items = append(q.items, item)
	q.signal()
	return true
}

// close ends the stream once the queued items are taken.
func (q *loginQueue[T]) close() {
	q.mu.Lock()
	defer q.mu.Unlock()
	q.closed = true
	q.signal()
}

// abandon drops the receiver: later pushes fail.
func (q *loginQueue[T]) abandon() {
	q.mu.Lock()
	defer q.mu.Unlock()
	q.abandoned = true
	q.items = nil
}

// take is everything queued, and whether the stream has ended after it.
func (q *loginQueue[T]) take() ([]T, bool) {
	q.mu.Lock()
	defer q.mu.Unlock()
	items := q.items
	q.items = nil
	return items, q.closed
}

// LoginTerminalServe serves a prepared command on stream until the login
// exits or the connection ends. Tests pass a real fixture command through
// this same connection executor; production passes this daemon's login
// entrypoint. Cancelling ctx is the daemon stopping or handing off (Rust's
// `cancellation` token).
func LoginTerminalServe(ctx context.Context, stream net.Conn, command *exec.Cmd, cols, rows uint16) error {
	pty, process, err := startLoginTerminal(command, cols, rows)
	if err != nil {
		return client.WriteFrame(stream, client.TerminalOutputFailed{
			Message: fmt.Sprintf("Start login terminal: %v", err),
		})
	}
	// An early return hands an armed root to its independent cleanup owner,
	// as dropping the Rust Process does.
	defer process.release()
	master := &loginMaster{master: pty}
	defer master.release()

	// A one-chunk rendezvous keeps output backpressure at the PTY instead of
	// accumulating the entire login transcript in daemon memory.
	outputCh := make(chan []byte, 1)
	outputDropped := make(chan struct{})
	var dropOutput sync.Once
	releaseOutput := func() { dropOutput.Do(func() { close(outputDropped) }) }
	defer releaseOutput()
	output := core.SpawnBlocking(func() error {
		defer close(outputCh)
		bytes := make([]byte, 8192)
		forwarding := true
		for {
			count, err := pty.Read(bytes)
			if count > 0 && forwarding {
				chunk := append([]byte(nil), bytes[:count]...)
				select {
				case outputCh <- chunk:
				case <-outputDropped:
					// ConPTY close may emit output until its close handlers
					// finish. Keep draining after connection cancellation so
					// closing the console never depends on a client that has
					// disconnected.
					forwarding = false
				}
			}
			if err != nil {
				if errors.Is(err, io.EOF) || loginReadEnded(err) {
					return nil
				}
				return err
			}
			if count == 0 {
				return nil
			}
		}
	})

	// Keep reading TCP while the harness is not reading its terminal.
	// Otherwise input backpressure hides EOF and a disconnected login cannot
	// be killed.
	inputs := newLoginQueue[client.TerminalInput]()
	disconnected, disconnect := context.WithCancel(ctx)
	defer disconnect()
	input := core.Spawn(func() error {
		defer disconnect()
		defer inputs.close()
		for {
			body, err := client.ReadFrame[json.RawMessage](stream)
			if err != nil {
				return err
			}
			frame, err := client.UnmarshalTerminalInputJSON(body)
			if err != nil {
				return fmt.Errorf("decode daemon frame: %w", err)
			}
			if !inputs.push(frame) {
				return nil
			}
		}
	})
	abortInput := func() {
		// Rust aborts the task; a blocked read is woken by its deadline.
		_ = stream.SetReadDeadline(time.Now())
	}
	defer abortInput()

	writes := newLoginQueue[[]byte]()
	writing := core.SpawnBlocking(func() error {
		defer writes.abandon()
		for {
			<-writes.ready
			chunks, closed := writes.take()
			for _, chunk := range chunks {
				if _, err := pty.Write(chunk); err != nil {
					return err
				}
			}
			if closed {
				return nil
			}
		}
	})
	defer writes.close()

	exited, err := observeExit(process)
	if err != nil {
		return err
	}
	observed := false
	var closing *core.JoinHandle[struct{}]
	interruptedWrite := false
	finished, result := func() (bool, error) {
		for {
			var exitedDone <-chan struct{}
			if !observed {
				exitedDone = exited.Done()
			}
			select {
			case <-exitedDone:
				observed = true
				exitErr, joinErr := exited.Join(context.Background())
				if joinErr != nil {
					return false, fmt.Errorf("observe login exit: %w", joinErr)
				}
				if exitErr != nil {
					return false, exitErr
				}
				process.terminate()
				closing = closeConsoleOnExit(master)
			case <-disconnected.Done():
				// Connection cancellation is the retry boundary, not login
				// success.
				return false, nil
			case <-inputs.ready:
				frames, closed := inputs.take()
				for _, frame := range frames {
					switch frame := frame.(type) {
					case client.TerminalInputBytes:
						if !writes.push(frame.Bytes) {
							return false, errors.New("login input writer stopped: channel closed")
						}
					case client.TerminalInputResize:
						master.mu.Lock()
						var resized error
						if master.master != nil {
							resized = master.master.Resize(loginTerminalSize(frame.Cols, frame.Rows))
						}
						master.mu.Unlock()
						if resized != nil {
							return false, resized
						}
					}
				}
				if closed {
					return false, nil
				}
			case chunk, ok := <-outputCh:
				if !ok {
					return true, nil
				}
				written := make(chan error, 1)
				go func() {
					written <- client.WriteFrame(stream, client.TerminalOutputBytes{Bytes: chunk})
				}()
				select {
				case <-disconnected.Done():
					interruptedWrite = true
					// Rust drops the write; wake the blocked one.
					_ = stream.SetWriteDeadline(time.Now())
					<-written
					return false, nil
				case err := <-written:
					if err != nil {
						return false, err
					}
				}
			}
		}
	}()
	// The connection has stopped forwarding output. Release the receiver now
	// so the reader discards PTY output from here on: the root writes to the
	// terminal while it exits (its cancellation error goes to stderr), and a
	// reader parked on a full channel would leave it blocked on a full PTY
	// while cleanup below waits for that exit.
	releaseOutput()
	if !observed {
		process.hangup()
		closing = closeConsoleOnExit(master)
		exitErr, joinErr := exited.Join(context.Background())
		if joinErr != nil {
			return fmt.Errorf("join login exit observer: %w", joinErr)
		}
		if exitErr != nil {
			return exitErr
		}
	}
	process.terminate()
	abortInput()
	if inputErr, joinErr := input.Join(context.Background()); joinErr != nil {
		slog.Warn("login terminal input task failed", "error", joinErr)
	} else if inputErr != nil {
		slog.Debug("login terminal input ended", "error", inputErr)
	}
	writes.close()
	if closing != nil {
		if _, err := closing.Join(context.Background()); err != nil {
			return fmt.Errorf("close login console: %w", err)
		}
	}
	code, err := loginWaitChild(process.child)
	if err != nil {
		return err
	}
	outputErr, joinErr := output.Join(context.Background())
	if joinErr != nil {
		return fmt.Errorf("join login output reader: %w", joinErr)
	}
	if outputErr != nil {
		return outputErr
	}
	// The master closes once its reader has drained it (Rust drops it before
	// the wait; its reader holds a duplicate of the descriptor).
	master.release()
	writeErr, joinErr := writing.Join(context.Background())
	if joinErr != nil {
		return fmt.Errorf("join login input writer: %w", joinErr)
	}
	if writeErr != nil {
		return writeErr
	}
	if result != nil {
		return result
	}
	if finished {
		return client.WriteFrame(stream, client.TerminalOutputExited{Code: code})
	}
	if ctx.Err() != nil && !interruptedWrite {
		return client.WriteFrame(stream, client.TerminalOutputFailed{
			Message: "The daemon stopped or handed off; retry Login.",
		})
	}
	return nil
}
