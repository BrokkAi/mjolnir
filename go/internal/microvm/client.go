//go:build linux

// Port of mj-microvm/src/client.rs.
//
// The host side of the vsock protocol.
//
// Firecracker exposes the guest's vsock as a Unix socket on the host. A
// client connects to it, writes `CONNECT <port>\n`, and reads `OK <n>\n`
// once the guest accepts; the stream is then the guest connection.

package microvm

import (
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"log/slog"
	"net"
	"strings"
	"syscall"
	"time"
)

// microvmAttemptTimeout is how long one connection attempt may wait for the
// guest's answers. The readiness loop that calls this keeps trying while
// the VM is running.
const microvmAttemptTimeout = 5 * time.Second

// MicrovmConnectUnix connects to the guest agent through Firecracker's
// vsock socket. The returned connection carries a deadline of
// microvmAttemptTimeout, as Rust's read and write timeouts.
func MicrovmConnectUnix(socket string, port uint32) (*net.UnixConn, error) {
	stream, err := net.DialUnix("unix", nil, &net.UnixAddr{Name: socket, Net: "unix"})
	if err != nil {
		return nil, fmt.Errorf("connect to %s: %w", socket, microvmIOError(err))
	}
	// Each exchange gets the attempt's time, as Rust's per-operation read
	// and write timeouts give it.
	extend := func() error {
		return stream.SetDeadline(time.Now().Add(microvmAttemptTimeout))
	}
	fail := func(err error) (*net.UnixConn, error) {
		_ = stream.Close()
		return nil, err
	}
	if err := extend(); err != nil {
		return fail(microvmIOError(err))
	}
	if _, err := stream.Write([]byte(fmt.Sprintf("CONNECT %d\n", port))); err != nil {
		return fail(fmt.Errorf("ask Firecracker for the guest connection: %w", microvmIOError(err)))
	}
	// Read one byte at a time: bytes after the newline belong to the protocol.
	var line []byte
	for {
		if err := extend(); err != nil {
			return fail(microvmIOError(err))
		}
		var b [1]byte
		count, err := stream.Read(b[:])
		if err != nil && err != io.EOF {
			return fail(fmt.Errorf("read Firecracker's answer: %w", microvmIOError(err)))
		}
		if count == 0 {
			return fail(fmt.Errorf("the guest agent is not listening on vsock port %d", port))
		}
		if b[0] == '\n' {
			break
		}
		line = append(line, b[0])
		if len(line) > 64 {
			return fail(errors.New("Firecracker's vsock answer is too long"))
		}
	}
	text := microvmLossy(line)
	if !strings.HasPrefix(text, "OK ") {
		return fail(fmt.Errorf("Firecracker refused the vsock connection: %s", text))
	}
	if err := extend(); err != nil {
		return fail(microvmIOError(err))
	}
	return stream, nil
}

// MicrovmClient is a connection past its hello, ready for one request.
// Each request method consumes the client: it closes the connection when
// it returns.
type MicrovmClient struct {
	transport MicrovmTransport
	sender    *MicrovmFrameSender
	frames    *MicrovmFrameReader
	Version   uint32
}

// MicrovmHandshake exchanges hellos on a fresh connection.
func MicrovmHandshake(transport MicrovmTransport) (*MicrovmClient, error) {
	sender := NewMicrovmFrameSender(transport)
	frames := NewMicrovmFrameReader(transport)
	fail := func(err error) (*MicrovmClient, error) {
		_ = transport.Close()
		return nil, err
	}
	ours := MicrovmCurrentHello()
	if err := sender.SendJSON(MicrovmFrameKindHello, ours); err != nil {
		return fail(err)
	}
	reply, err := frames.Expect("the agent's hello")
	if err != nil {
		return fail(err)
	}
	var theirs MicrovmHello
	switch reply.Kind {
	case MicrovmFrameKindHello:
		if err := json.Unmarshal(reply.Payload, &theirs); err != nil {
			return fail(fmt.Errorf("decode the agent's hello: %w", err))
		}
	case MicrovmFrameKindError:
		return fail(microvmErrorText(reply.Payload))
	default:
		return fail(fmt.Errorf("expected a hello frame, received %s", reply.Kind))
	}
	version, err := MicrovmNegotiate(ours, theirs)
	if err != nil {
		return fail(err)
	}
	return &MicrovmClient{transport: transport, sender: sender, frames: frames, Version: version}, nil
}

// microvmErrorText is the error an error frame's payload carries.
func microvmErrorText(payload []byte) error {
	return errors.New(microvmLossy(payload))
}

func (c *MicrovmClient) request(request MicrovmRequest) error {
	return c.sender.SendJSON(MicrovmFrameKindRequest, request)
}

// accepted waits for the agent to accept a started or detached process.
func (c *MicrovmClient) accepted() (uint32, error) {
	frame, err := c.frames.Expect("the agent's answer")
	if err != nil {
		return 0, err
	}
	switch frame.Kind {
	case MicrovmFrameKindAccepted:
		var accepted MicrovmAccepted
		if err := json.Unmarshal(frame.Payload, &accepted); err != nil {
			return 0, fmt.Errorf("decode the agent's answer: %w", err)
		}
		return accepted.Pid, nil
	case MicrovmFrameKindError:
		return 0, microvmErrorText(frame.Payload)
	}
	return 0, fmt.Errorf("expected an accepted frame, received %s", frame.Kind)
}

// Start starts the container's main process.
func (c *MicrovmClient) Start(start MicrovmStartRequest) (uint32, error) {
	defer c.transport.Close()
	if err := c.request(MicrovmRequestStart{Request: start}); err != nil {
		return 0, err
	}
	return c.accepted()
}

// ExecDetached starts a detached process and returns its pid in the guest.
func (c *MicrovmClient) ExecDetached(request MicrovmExecRequest) (uint32, error) {
	defer c.transport.Close()
	request.Detach = true
	request.Stdin = false
	if err := c.request(MicrovmRequestExec{Request: request}); err != nil {
		return 0, err
	}
	return c.accepted()
}

// Exec runs a command, relaying stdin (nil for none) to it and its output to
// stdout and stderr, and returns how it ended.
//
// Input is sent from its own goroutine while this one reads output, so a
// command that writes before it reads cannot deadlock the pair.
func (c *MicrovmClient) Exec(request MicrovmExecRequest, stdin io.Reader, stdout, stderr io.Writer) (MicrovmExitInfo, error) {
	defer c.transport.Close()
	request.Detach = false
	request.Stdin = stdin != nil
	if err := c.request(MicrovmRequestExec{Request: request}); err != nil {
		return MicrovmExitInfo{}, err
	}
	inputFailures := make(chan error, 1)
	if stdin != nil {
		sender := c.sender
		go func() {
			buffer := make([]byte, MicrovmChunkSize)
			for {
				count, err := stdin.Read(buffer)
				if count > 0 {
					if sender.Send(MicrovmFrameKindStdin, buffer[:count]) != nil {
						// The agent closed the connection: the process has
						// finished, so unsent input is moot and the exit
						// frame has the rest.
						return
					}
				}
				if err == io.EOF {
					break
				}
				if err != nil {
					inputFailures <- fmt.Errorf("read the command's standard input: %w", microvmIOError(err))
					break
				}
			}
			// Same reasoning for a failed end-of-input frame.
			_ = sender.Send(MicrovmFrameKindStdinEOF, nil)
		}()
	}
	for {
		frame, err := c.frames.Expect("the command's output or exit")
		if err != nil {
			return MicrovmExitInfo{}, err
		}
		switch frame.Kind {
		case MicrovmFrameKindStdout:
			if _, err := stdout.Write(frame.Payload); err != nil {
				return MicrovmExitInfo{}, fmt.Errorf("write command output: %w", microvmIOError(err))
			}
		case MicrovmFrameKindStderr:
			if _, err := stderr.Write(frame.Payload); err != nil {
				return MicrovmExitInfo{}, fmt.Errorf("write command error output: %w", microvmIOError(err))
			}
		case MicrovmFrameKindExit:
			if flusher, ok := stdout.(interface{ Flush() error }); ok {
				if err := flusher.Flush(); err != nil {
					return MicrovmExitInfo{}, fmt.Errorf("flush command output: %w", microvmIOError(err))
				}
			}
			select {
			case err := <-inputFailures:
				return MicrovmExitInfo{}, err
			default:
			}
			var info MicrovmExitInfo
			if err := json.Unmarshal(frame.Payload, &info); err != nil {
				return MicrovmExitInfo{}, fmt.Errorf("decode exit status: %w", err)
			}
			return info, nil
		case MicrovmFrameKindError:
			return MicrovmExitInfo{}, microvmErrorText(frame.Payload)
		default:
			return MicrovmExitInfo{}, fmt.Errorf("unexpected %s frame during exec", frame.Kind)
		}
	}
}

func (c *MicrovmClient) expectExit() error {
	frame, err := c.frames.Expect("the copy result")
	if err != nil {
		return err
	}
	switch frame.Kind {
	case MicrovmFrameKindExit:
		return nil
	case MicrovmFrameKindError:
		return microvmErrorText(frame.Payload)
	}
	return fmt.Errorf("unexpected %s frame after a copy", frame.Kind)
}

// CopyIn copies a host file or directory into the guest at destination.
func (c *MicrovmClient) CopyIn(source string, destination string, mode MicrovmCopyMode) error {
	defer c.transport.Close()
	if err := c.request(MicrovmRequestCopyIn{Destination: destination, Contents: mode == MicrovmCopyModeContents}); err != nil {
		return err
	}
	writer := NewMicrovmTarFrameWriter(c.sender)
	packing := MicrovmTarcopyPack(source, writer, mode)
	if packing == nil {
		if err := writer.Finish(); err != nil {
			packing = fmt.Errorf("finish tar stream: %w", err)
		}
	}
	switch {
	case packing == nil:
		return c.expectExit()
	case microvmConnectionBroke(packing):
		// The write failed because the agent refused the copy and closed
		// the connection; its reason is waiting in it.
		if reason := c.expectExit(); reason != nil && !strings.HasPrefix(reason.Error(), "connection closed") {
			return reason
		}
		return packing
	default:
		// The failure is on this side (a missing or unreadable source). The
		// agent is still waiting for tar frames, so end the connection
		// rather than leave it waiting.
		if err := c.sender.Shutdown(); err != nil {
			slog.Warn("could not close the connection after a failed copy", "error", err)
		}
		return packing
	}
}

// CopyOut copies source out of the guest to destination on the host.
func (c *MicrovmClient) CopyOut(source string, destination string, owner *MicrovmOwner, mode MicrovmCopyMode) error {
	defer c.transport.Close()
	if err := c.request(MicrovmRequestCopyOut{Source: source, Contents: mode == MicrovmCopyModeContents}); err != nil {
		return err
	}
	reader := NewMicrovmTarFrameReader(c.frames)
	if err := MicrovmTarcopyUnpack(reader, destination, owner, mode); err != nil {
		return err
	}
	if err := reader.Finish(); err != nil {
		return err
	}
	return c.expectExit()
}

// Stop asks the guest to flush its filesystems and power off. Returns once
// the flush has finished.
func (c *MicrovmClient) Stop() error {
	defer c.transport.Close()
	if err := c.request(MicrovmRequestStop{}); err != nil {
		return err
	}
	return c.expectExit()
}

// microvmConnectionBroke reports whether a packing failure came from
// writing to the connection.
func microvmConnectionBroke(err error) bool {
	return errors.Is(err, syscall.EPIPE) || errors.Is(err, syscall.ECONNRESET) || errors.Is(err, syscall.ECONNABORTED)
}
