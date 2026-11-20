//go:build linux

// Port of mj-worker/src/microvm/client.rs.
//
// The host side of the vsock protocol.
//
// Firecracker exposes the guest's vsock as a Unix socket on the host. A
// client connects to it, writes `CONNECT <port>\n`, and reads `OK <n>\n`
// once the guest accepts; the stream is then the guest connection.

package worker

import (
	"errors"
	"io"
	"net"
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
	panic("unported: mj-worker/src/microvm/client.rs connect_unix")
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
	panic("unported: mj-worker/src/microvm/client.rs handshake")
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
	panic("unported: mj-worker/src/microvm/client.rs Client::accepted")
}

// Start starts the container's main process.
func (c *MicrovmClient) Start(start MicrovmStartRequest) (uint32, error) {
	panic("unported: mj-worker/src/microvm/client.rs Client::start")
}

// ExecDetached starts a detached process and returns its pid in the guest.
func (c *MicrovmClient) ExecDetached(request MicrovmExecRequest) (uint32, error) {
	panic("unported: mj-worker/src/microvm/client.rs Client::exec_detached")
}

// Exec runs a command, relaying stdin (nil for none) to it and its output to
// stdout and stderr, and returns how it ended.
//
// Input is sent from its own goroutine while this one reads output, so a
// command that writes before it reads cannot deadlock the pair.
func (c *MicrovmClient) Exec(request MicrovmExecRequest, stdin io.Reader, stdout, stderr io.Writer) (MicrovmExitInfo, error) {
	panic("unported: mj-worker/src/microvm/client.rs Client::exec")
}

func (c *MicrovmClient) expectExit() error {
	panic("unported: mj-worker/src/microvm/client.rs Client::expect_exit")
}

// CopyIn copies a host file or directory into the guest at destination.
func (c *MicrovmClient) CopyIn(source string, destination string, mode MicrovmCopyMode) error {
	panic("unported: mj-worker/src/microvm/client.rs Client::copy_in")
}

// CopyOut copies source out of the guest to destination on the host.
func (c *MicrovmClient) CopyOut(source string, destination string, owner *MicrovmOwner, mode MicrovmCopyMode) error {
	panic("unported: mj-worker/src/microvm/client.rs Client::copy_out")
}

// Stop asks the guest to flush its filesystems and power off. Returns once
// the flush has finished.
func (c *MicrovmClient) Stop() error {
	panic("unported: mj-worker/src/microvm/client.rs Client::stop")
}

// microvmConnectionBroke reports whether a packing failure came from
// writing to the connection.
func microvmConnectionBroke(err error) bool {
	panic("unported: mj-worker/src/microvm/client.rs connection_broke")
}
