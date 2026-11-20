//go:build linux

// Port of mj-worker/src/microvm/protocol.rs.
//
// The vsock protocol between the host engine and the guest agent.
//
// One connection carries one request. Both sides open with a hello that
// names their protocol version, so an engine can talk to the agent of a VM
// that was launched by an older engine build, and the reverse. After the
// hello the client sends a request frame, then data frames flow in both
// directions until the agent sends an exit frame (or an error frame).
//
// A frame is one kind byte, a four-byte big-endian payload length, and the
// payload. Control payloads are JSON; data payloads are raw bytes.

package worker

import (
	"bufio"
	"bytes"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"net"
	"sync"
	"syscall"
)

// MicrovmProtocolVersion is the version this build speaks.
const MicrovmProtocolVersion uint32 = 1

// MicrovmMinProtocolVersion is the oldest version this build can still
// speak. Raising it strands VMs that were launched by older engines, so it
// only rises with a migration for them.
const MicrovmMinProtocolVersion uint32 = 1

// MicrovmAgentPort is the guest vsock port the agent listens on.
const MicrovmAgentPort uint32 = 1024

// MicrovmChunkSize is the largest data frame writers send.
const MicrovmChunkSize = 64 * 1024

// MicrovmMaxFrameSize is the largest frame readers accept. It is the
// protocol's own limit, not a guard against load: a corrupt length must not
// allocate gigabytes.
const MicrovmMaxFrameSize = 16 * 1024 * 1024

// MicrovmFrameKind is what a frame carries.
type MicrovmFrameKind uint8

const (
	MicrovmFrameKindHello   MicrovmFrameKind = 1
	MicrovmFrameKindRequest MicrovmFrameKind = 2
	// MicrovmFrameKindAccepted: a detached or started process was created;
	// the payload names its pid.
	MicrovmFrameKindAccepted MicrovmFrameKind = 3
	MicrovmFrameKindStdin    MicrovmFrameKind = 0x10
	MicrovmFrameKindStdinEOF MicrovmFrameKind = 0x11
	MicrovmFrameKindStdout   MicrovmFrameKind = 0x12
	MicrovmFrameKindStderr   MicrovmFrameKind = 0x13
	MicrovmFrameKindTar      MicrovmFrameKind = 0x14
	MicrovmFrameKindTarEnd   MicrovmFrameKind = 0x15
	MicrovmFrameKindExit     MicrovmFrameKind = 0x20
	MicrovmFrameKindError    MicrovmFrameKind = 0x21
)

// microvmFrameKindFromByte is FrameKind::from_byte.
func microvmFrameKindFromByte(b byte) (MicrovmFrameKind, error) {
	kind := MicrovmFrameKind(b)
	switch kind {
	case MicrovmFrameKindHello, MicrovmFrameKindRequest, MicrovmFrameKindAccepted,
		MicrovmFrameKindStdin, MicrovmFrameKindStdinEOF, MicrovmFrameKindStdout,
		MicrovmFrameKindStderr, MicrovmFrameKindTar, MicrovmFrameKindTarEnd,
		MicrovmFrameKindExit, MicrovmFrameKindError:
		return kind, nil
	}
	return 0, fmt.Errorf("unknown microvm protocol frame kind %#x", b)
}

// String is the Rust Debug name of the kind, as error messages print it.
func (k MicrovmFrameKind) String() string {
	names := map[MicrovmFrameKind]string{
		MicrovmFrameKindHello: "Hello", MicrovmFrameKindRequest: "Request",
		MicrovmFrameKindAccepted: "Accepted", MicrovmFrameKindStdin: "Stdin",
		MicrovmFrameKindStdinEOF: "StdinEof", MicrovmFrameKindStdout: "Stdout",
		MicrovmFrameKindStderr: "Stderr", MicrovmFrameKindTar: "Tar",
		MicrovmFrameKindTarEnd: "TarEnd", MicrovmFrameKindExit: "Exit",
		MicrovmFrameKindError: "Error",
	}
	if name, ok := names[k]; ok {
		return name
	}
	return fmt.Sprintf("MicrovmFrameKind(%#x)", uint8(k))
}

// MicrovmFrame is one decoded frame.
type MicrovmFrame struct {
	Kind    MicrovmFrameKind
	Payload []byte
}

// MicrovmHello is what both peers announce they speak.
type MicrovmHello struct {
	Version    uint32 `json:"version"`
	MinVersion uint32 `json:"min_version"`
}

// UnmarshalJSON requires both members, as serde does.
func (h *MicrovmHello) UnmarshalJSON(data []byte) error {
	if err := microvmRequireFields(data, "version", "min_version"); err != nil {
		return err
	}
	type plain MicrovmHello
	return json.Unmarshal(data, (*plain)(h))
}

// MicrovmCurrentHello is Hello::current: the hello this build sends.
func MicrovmCurrentHello() MicrovmHello {
	return MicrovmHello{Version: MicrovmProtocolVersion, MinVersion: MicrovmMinProtocolVersion}
}

// MicrovmNegotiate returns the version two hellos agree on, or why they
// cannot.
func MicrovmNegotiate(ours, theirs MicrovmHello) (uint32, error) {
	version := min(ours.Version, theirs.Version)
	floor := max(ours.MinVersion, theirs.MinVersion)
	if version < floor {
		return 0, fmt.Errorf("microvm protocol versions do not overlap: this side speaks %d..=%d, the peer speaks %d..=%d",
			ours.MinVersion, ours.Version, theirs.MinVersion, theirs.Version)
	}
	return version, nil
}

// MicrovmExitInfo is how a process ended.
type MicrovmExitInfo struct {
	Code   *int32 `json:"code"`
	Signal *int32 `json:"signal"`
}

// ShellStatus is the exit status a shell would report: the code, or 128
// plus the signal.
func (e MicrovmExitInfo) ShellStatus() int32 {
	switch {
	case e.Code != nil:
		return *e.Code
	case e.Signal != nil:
		return 128 + *e.Signal
	}
	return 1
}

// MicrovmVolumeSpec is one volume the agent finds mounted by the initrd.
type MicrovmVolumeSpec struct {
	Destination string `json:"destination"`
	// Chown is Podman's `U` volume option: the volume root belongs to the
	// container user.
	Chown bool `json:"chown"`
}

// UnmarshalJSON requires the destination, as serde does.
func (v *MicrovmVolumeSpec) UnmarshalJSON(data []byte) error {
	if err := microvmRequireFields(data, "destination"); err != nil {
		return err
	}
	type plain MicrovmVolumeSpec
	return json.Unmarshal(data, (*plain)(v))
}

// MicrovmStartRequest is the container's main process and the defaults its
// exec sessions inherit.
type MicrovmStartRequest struct {
	Argv []string `json:"argv"`
	// Env holds `KEY=VALUE` entries.
	Env     []string            `json:"env"`
	Cwd     string              `json:"cwd"`
	User    string              `json:"user"`
	Volumes []MicrovmVolumeSpec `json:"volumes"`
}

// MarshalJSON writes empty lists as `[]`, as serde does.
func (s MicrovmStartRequest) MarshalJSON() ([]byte, error) {
	type plain MicrovmStartRequest
	p := plain(s)
	p.Argv, p.Env, p.Volumes = microvmNonNil(p.Argv), microvmNonNil(p.Env), microvmNonNil(p.Volumes)
	return json.Marshal(p)
}

// UnmarshalJSON requires the members serde requires.
func (s *MicrovmStartRequest) UnmarshalJSON(data []byte) error {
	if err := microvmRequireFields(data, "argv", "env", "cwd", "user"); err != nil {
		return err
	}
	type plain MicrovmStartRequest
	return json.Unmarshal(data, (*plain)(s))
}

// MicrovmExecRequest is one command run in the VM.
type MicrovmExecRequest struct {
	Argv []string `json:"argv"`
	// Env holds `KEY=VALUE` entries added to the container environment.
	Env  []string `json:"env"`
	Cwd  *string  `json:"cwd"`
	User *string  `json:"user"`
	// Stdin: the client will send stdin frames; otherwise stdin is `/dev/null`.
	Stdin bool `json:"stdin"`
	// Detach: start in a new session, answer at once, and leave the process
	// running.
	Detach bool `json:"detach"`
}

// MarshalJSON writes empty lists as `[]`, as serde does.
func (r MicrovmExecRequest) MarshalJSON() ([]byte, error) {
	type plain MicrovmExecRequest
	p := plain(r)
	p.Argv, p.Env = microvmNonNil(p.Argv), microvmNonNil(p.Env)
	return json.Marshal(p)
}

// UnmarshalJSON requires the argv, as serde does.
func (r *MicrovmExecRequest) UnmarshalJSON(data []byte) error {
	if err := microvmRequireFields(data, "argv"); err != nil {
		return err
	}
	type plain MicrovmExecRequest
	return json.Unmarshal(data, (*plain)(r))
}

// MicrovmRequest is the first frame after the hello: serde's internally
// tagged enum (`tag = "op"`, snake_case variant names). Encode a variant
// with json.Marshal; decode with MicrovmDecodeRequest.
type MicrovmRequest interface {
	isMicrovmRequest()
}

// MicrovmRequestStart starts the container's main process.
type MicrovmRequestStart struct {
	Request MicrovmStartRequest
}

// MicrovmRequestExec runs a command.
type MicrovmRequestExec struct {
	Request MicrovmExecRequest
}

// MicrovmRequestCopyIn receives a tar stream and unpacks it at Destination.
type MicrovmRequestCopyIn struct {
	Destination string `json:"destination"`
	// Contents: the tar holds a directory's contents, not the directory
	// itself.
	Contents bool `json:"contents"`
}

// MicrovmRequestCopyOut sends Source as a tar stream.
type MicrovmRequestCopyOut struct {
	Source string `json:"source"`
	// Contents: send the directory's contents, not the directory itself.
	Contents bool `json:"contents"`
}

// MicrovmRequestStop flushes every filesystem, answers, and powers the VM
// off.
type MicrovmRequestStop struct{}

func (MicrovmRequestStart) isMicrovmRequest()   {}
func (MicrovmRequestExec) isMicrovmRequest()    {}
func (MicrovmRequestCopyIn) isMicrovmRequest()  {}
func (MicrovmRequestCopyOut) isMicrovmRequest() {}
func (MicrovmRequestStop) isMicrovmRequest()    {}

func (r MicrovmRequestStart) MarshalJSON() ([]byte, error) { return microvmTagged("start", r.Request) }
func (r MicrovmRequestExec) MarshalJSON() ([]byte, error)  { return microvmTagged("exec", r.Request) }

func (r MicrovmRequestCopyIn) MarshalJSON() ([]byte, error) {
	type plain MicrovmRequestCopyIn
	return microvmTagged("copy_in", plain(r))
}

func (r MicrovmRequestCopyOut) MarshalJSON() ([]byte, error) {
	type plain MicrovmRequestCopyOut
	return microvmTagged("copy_out", plain(r))
}

func (MicrovmRequestStop) MarshalJSON() ([]byte, error) { return microvmTagged("stop", struct{}{}) }

// MicrovmDecodeRequest decodes a request as serde decodes the internally
// tagged enum: the `op` member names the variant, the others its fields.
func MicrovmDecodeRequest(data []byte) (MicrovmRequest, error) {
	var tagged struct {
		Op *string `json:"op"`
	}
	if err := json.Unmarshal(data, &tagged); err != nil {
		return nil, err
	}
	if tagged.Op == nil {
		return nil, errors.New("missing field `op`")
	}
	switch *tagged.Op {
	case "start":
		var request MicrovmStartRequest
		err := json.Unmarshal(data, &request)
		return MicrovmRequestStart{Request: request}, err
	case "exec":
		var request MicrovmExecRequest
		err := json.Unmarshal(data, &request)
		return MicrovmRequestExec{Request: request}, err
	case "copy_in":
		if err := microvmRequireFields(data, "destination"); err != nil {
			return nil, err
		}
		type plain MicrovmRequestCopyIn
		var request plain
		err := json.Unmarshal(data, &request)
		return MicrovmRequestCopyIn(request), err
	case "copy_out":
		if err := microvmRequireFields(data, "source"); err != nil {
			return nil, err
		}
		type plain MicrovmRequestCopyOut
		var request plain
		err := json.Unmarshal(data, &request)
		return MicrovmRequestCopyOut(request), err
	case "stop":
		return MicrovmRequestStop{}, nil
	}
	return nil, fmt.Errorf("unknown variant `%s`, expected one of `start`, `exec`, `copy_in`, `copy_out`, `stop`", *tagged.Op)
}

// microvmTagged writes body (a JSON object) with the serde tag member
// `"op":op` first, as an internally tagged enum is written.
func microvmTagged(op string, body any) ([]byte, error) {
	encoded, err := json.Marshal(body)
	if err != nil {
		return nil, err
	}
	tag := fmt.Sprintf(`{"op":%q`, op)
	if bytes.Equal(encoded, []byte("{}")) {
		return []byte(tag + "}"), nil
	}
	return append([]byte(tag+","), encoded[1:]...), nil
}

// microvmRequireFields is serde's missing-field check for required members.
func microvmRequireFields(data []byte, names ...string) error {
	var object map[string]json.RawMessage
	if err := json.Unmarshal(data, &object); err != nil {
		return err
	}
	for _, name := range names {
		if _, ok := object[name]; !ok {
			return fmt.Errorf("missing field `%s`", name)
		}
	}
	return nil
}

// microvmNonNil is items, or an empty list for nil, so a present empty Vec
// is written as `[]`.
func microvmNonNil[T any](items []T) []T {
	if items == nil {
		return []T{}
	}
	return items
}

// microvmNonNilMap is entries, or an empty map for nil, so a present empty
// BTreeMap is written as `{}`.
func microvmNonNilMap[V any](entries map[string]V) map[string]V {
	if entries == nil {
		return map[string]V{}
	}
	return entries
}

// MicrovmAccepted is what the agent answers to a detached exec or a start.
type MicrovmAccepted struct {
	Pid uint32 `json:"pid"`
}

// UnmarshalJSON requires the pid, as serde does.
func (a *MicrovmAccepted) UnmarshalJSON(data []byte) error {
	if err := microvmRequireFields(data, "pid"); err != nil {
		return err
	}
	type plain MicrovmAccepted
	return json.Unmarshal(data, (*plain)(a))
}

// MicrovmTransport is a byte stream the protocol can run over: a vsock
// socket in the guest, a Unix socket on the host.
//
// Rust's Transport also has try_clone, because its reader and writer each
// own a handle; a Go connection is safe for one reader and one writer at
// once, so both share this one. Close stands in for Rust's Drop.
type MicrovmTransport interface {
	io.Reader
	io.Writer
	io.Closer
	// Shutdown wakes any goroutine blocked reading this connection.
	Shutdown() error
}

// MicrovmUnixTransport is the host side's transport: a Unix socket.
type MicrovmUnixTransport struct {
	*net.UnixConn
}

// Shutdown shuts the socket down in both directions.
func (t MicrovmUnixTransport) Shutdown() error {
	raw, err := t.UnixConn.SyscallConn()
	if err != nil {
		return err
	}
	var shutdownErr error
	if err := raw.Control(func(fd uintptr) {
		shutdownErr = syscall.Shutdown(int(fd), syscall.SHUT_RDWR)
	}); err != nil {
		return err
	}
	return shutdownErr
}

// MicrovmFrameSender serializes writers: several goroutines (stdout,
// stderr, exit) share one connection. Share the pointer to share the
// sender, as Rust clones its Arc.
type MicrovmFrameSender struct {
	mu        sync.Mutex
	transport MicrovmTransport
}

// NewMicrovmFrameSender is FrameSender::new.
func NewMicrovmFrameSender(transport MicrovmTransport) *MicrovmFrameSender {
	return &MicrovmFrameSender{transport: transport}
}

// Send writes one frame.
func (s *MicrovmFrameSender) Send(kind MicrovmFrameKind, payload []byte) error {
	panic("unported: mj-worker/src/microvm/protocol.rs FrameSender::send")
}

// SendJSON writes one frame whose payload is value encoded as JSON.
func (s *MicrovmFrameSender) SendJSON(kind MicrovmFrameKind, value any) error {
	payload, err := json.Marshal(value)
	if err != nil {
		return fmt.Errorf("encode microvm protocol message: %w", err)
	}
	if err := s.Send(kind, payload); err != nil {
		return fmt.Errorf("write microvm protocol frame: %w", err)
	}
	return nil
}

// SendData sends data as data frames of at most MicrovmChunkSize bytes.
func (s *MicrovmFrameSender) SendData(kind MicrovmFrameKind, data []byte) error {
	for start := 0; start < len(data); start += MicrovmChunkSize {
		if err := s.Send(kind, data[start:min(start+MicrovmChunkSize, len(data))]); err != nil {
			return err
		}
	}
	return nil
}

// Shutdown shuts the connection down.
func (s *MicrovmFrameSender) Shutdown() error {
	s.mu.Lock()
	defer s.mu.Unlock()
	return s.transport.Shutdown()
}

// MicrovmFrameReader reads frames from one connection.
type MicrovmFrameReader struct {
	inner *bufio.Reader
}

// NewMicrovmFrameReader is FrameReader::new.
func NewMicrovmFrameReader(transport io.Reader) *MicrovmFrameReader {
	return &MicrovmFrameReader{inner: bufio.NewReaderSize(transport, MicrovmChunkSize)}
}

// NextFrame returns the next frame, or nil on a clean end of stream between
// frames.
func (r *MicrovmFrameReader) NextFrame() (*MicrovmFrame, error) {
	panic("unported: mj-worker/src/microvm/protocol.rs FrameReader::next_frame")
}

// Expect returns the next frame; end of stream is an error because the
// conversation is not over.
func (r *MicrovmFrameReader) Expect(what string) (MicrovmFrame, error) {
	frame, err := r.NextFrame()
	if err != nil {
		return MicrovmFrame{}, err
	}
	if frame == nil {
		return MicrovmFrame{}, fmt.Errorf("connection closed while waiting for %s", what)
	}
	return *frame, nil
}

// MicrovmTarFrameReader reads the tar stream of a copy: tar frames until
// TarEnd. Read returns io.EOF at TarEnd.
type MicrovmTarFrameReader struct {
	frames   *MicrovmFrameReader
	current  []byte
	offset   int
	finished bool
}

// NewMicrovmTarFrameReader is TarFrameReader::new.
func NewMicrovmTarFrameReader(frames *MicrovmFrameReader) *MicrovmTarFrameReader {
	return &MicrovmTarFrameReader{frames: frames}
}

// Finish consumes whatever remains of the stream, so the connection is
// left at the frame after TarEnd.
func (r *MicrovmTarFrameReader) Finish() error {
	panic("unported: mj-worker/src/microvm/protocol.rs TarFrameReader::finish")
}

// Read is the io.Reader over the tar frames.
func (r *MicrovmTarFrameReader) Read(buffer []byte) (int, error) {
	panic("unported: mj-worker/src/microvm/protocol.rs TarFrameReader::read")
}

// MicrovmTarFrameWriter writes a tar stream as tar frames.
type MicrovmTarFrameWriter struct {
	sender  *MicrovmFrameSender
	pending []byte
}

// NewMicrovmTarFrameWriter is TarFrameWriter::new.
func NewMicrovmTarFrameWriter(sender *MicrovmFrameSender) *MicrovmTarFrameWriter {
	return &MicrovmTarFrameWriter{sender: sender, pending: make([]byte, 0, MicrovmChunkSize)}
}

// Finish flushes the last partial frame and ends the stream.
func (w *MicrovmTarFrameWriter) Finish() error {
	if err := w.flushPending(); err != nil {
		return err
	}
	return w.sender.Send(MicrovmFrameKindTarEnd, nil)
}

func (w *MicrovmTarFrameWriter) flushPending() error {
	if len(w.pending) > 0 {
		if err := w.sender.Send(MicrovmFrameKindTar, w.pending); err != nil {
			return err
		}
		w.pending = w.pending[:0]
	}
	return nil
}

// Write is the io.Writer of the tar stream. Unlike Rust's partial write,
// it takes all of data, as io.Writer requires.
func (w *MicrovmTarFrameWriter) Write(data []byte) (int, error) {
	panic("unported: mj-worker/src/microvm/protocol.rs TarFrameWriter::write")
}

// Flush does nothing: frame boundaries are not stream boundaries; Finish
// ends the stream.
func (w *MicrovmTarFrameWriter) Flush() error {
	return nil
}
