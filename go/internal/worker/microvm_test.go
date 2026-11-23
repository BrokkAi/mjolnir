//go:build linux

// Port of mj-worker/src/microvm/tests.rs.
//
// The vsock protocol between an engine client and the guest agent, over a
// Unix socket pair. The pair is the same byte stream a vsock connection
// gives; only the transport differs.

package worker

import (
	"archive/tar"
	"bytes"
	"encoding/json"
	"fmt"
	"io"
	"net"
	"os"
	"path/filepath"
	"strings"
	"sync/atomic"
	"testing"
	"time"

	"golang.org/x/sys/unix"
)

// microvmRecordingSystem records stop calls instead of powering anything
// off (guest_agent.rs's test_support::RecordingSystem).
type microvmRecordingSystem struct {
	quiesced   atomic.Bool
	poweredOff atomic.Bool
}

func (s *microvmRecordingSystem) Quiesce() { s.quiesced.Store(true) }

func (s *microvmRecordingSystem) PowerOff() { s.poweredOff.Store(true) }

type microvmHarness struct {
	agent  *MicrovmAgent
	system *microvmRecordingSystem
}

func newMicrovmHarness() *microvmHarness {
	system := &microvmRecordingSystem{}
	return &microvmHarness{agent: NewMicrovmAgent(MicrovmDirectSpawner{}, system), system: system}
}

// microvmSocketPair is Rust's UnixStream::pair.
func microvmSocketPair(t *testing.T) (*net.UnixConn, *net.UnixConn) {
	t.Helper()
	fds, err := unix.Socketpair(unix.AF_UNIX, unix.SOCK_STREAM|unix.SOCK_CLOEXEC, 0)
	if err != nil {
		t.Fatal(err)
	}
	connection := func(fd int) *net.UnixConn {
		file := os.NewFile(uintptr(fd), "socketpair")
		defer file.Close()
		conn, err := net.FileConn(file)
		if err != nil {
			t.Fatal(err)
		}
		return conn.(*net.UnixConn)
	}
	return connection(fds[0]), connection(fds[1])
}

// connect is a client connected to a fresh agent connection, and the
// agent's result.
func (h *microvmHarness) connect(t *testing.T) (*net.UnixConn, <-chan error) {
	clientSide, agentSide := microvmSocketPair(t)
	done := make(chan error, 1)
	go func() { done <- h.agent.Serve(MicrovmUnixTransport{UnixConn: agentSide}) }()
	return clientSide, done
}

func (h *microvmHarness) client(t *testing.T) *MicrovmClient {
	stream, _ := h.connect(t)
	client, err := MicrovmHandshake(MicrovmUnixTransport{UnixConn: stream})
	if err != nil {
		t.Fatal(err)
	}
	return client
}

func microvmExecRequest(argv ...string) MicrovmExecRequest {
	return MicrovmExecRequest{Argv: argv}
}

func microvmPattern(length int) []byte {
	data := make([]byte, length)
	for i := range data {
		data[i] = byte(i % 251)
	}
	return data
}

func microvmCode(info MicrovmExitInfo) string {
	describe := func(value *int32) string {
		if value == nil {
			return "None"
		}
		return fmt.Sprintf("Some(%d)", *value)
	}
	return fmt.Sprintf("code %s, signal %s", describe(info.Code), describe(info.Signal))
}

func TestExecMovesMoreThanAPipeBufferInBothDirectionsAtOnce(t *testing.T) {
	// `cat` echoes while it reads, so a client that sent all its input before
	// reading output would deadlock once either 64 KiB pipe filled.
	harness := newMicrovmHarness()
	input := microvmPattern(1 << 20)
	var stdout, stderr bytes.Buffer
	exit, err := harness.client(t).Exec(
		microvmExecRequest("sh", "-c", "cat; head -c 200000 /dev/zero | tr '\\0' x >&2"),
		bytes.NewReader(input),
		&stdout,
		&stderr,
	)
	if err != nil {
		t.Fatal(err)
	}
	if exit.Code == nil || *exit.Code != 0 {
		t.Fatalf("exit: %s", microvmCode(exit))
	}
	if !bytes.Equal(stdout.Bytes(), input) {
		t.Fatalf("stdout differs from the %d byte input", len(input))
	}
	if !bytes.Equal(stderr.Bytes(), bytes.Repeat([]byte{'x'}, 200_000)) {
		t.Fatalf("stderr holds %d bytes, not 200000 x's", stderr.Len())
	}
}

func TestExecSurvivesAProcessThatStopsReadingStdinEarly(t *testing.T) {
	harness := newMicrovmHarness()
	var stdout bytes.Buffer
	exit, err := harness.client(t).Exec(
		microvmExecRequest("head", "-c", "10"),
		bytes.NewReader(microvmPattern(4<<20)),
		&stdout,
		io.Discard,
	)
	if err != nil {
		t.Fatal(err)
	}
	if exit.Code == nil || *exit.Code != 0 {
		t.Fatalf("exit: %s", microvmCode(exit))
	}
	if !bytes.Equal(stdout.Bytes(), microvmPattern(10)) {
		t.Fatalf("stdout = %v", stdout.Bytes())
	}
}

// Hard-won: review: a background descendant that holds the exec's stdin open
// without reading it left the connection thread blocked in a pipe write after
// the exec had finished.
func TestExecEndsWhenADescendantHoldsUnreadStdinOpen(t *testing.T) {
	harness := newMicrovmHarness()
	stream, done := harness.connect(t)
	client, err := MicrovmHandshake(MicrovmUnixTransport{UnixConn: stream})
	if err != nil {
		t.Fatal(err)
	}
	exit, err := client.Exec(
		microvmExecRequest("sh", "-c", "sleep 5 & exit 0"),
		bytes.NewReader(microvmPattern(2<<20)),
		io.Discard,
		io.Discard,
	)
	if err != nil {
		t.Fatal(err)
	}
	if exit.Code == nil || *exit.Code != 0 {
		t.Fatalf("exit: %s", microvmCode(exit))
	}
	if err := <-done; err != nil {
		t.Fatal(err)
	}
}

// microvmHeldInput hands out one line, then waits to be released before
// reporting end of input.
type microvmHeldInput struct {
	sentFirst bool
	release   <-chan struct{}
}

func (h *microvmHeldInput) Read(buffer []byte) (int, error) {
	if !h.sentFirst {
		h.sentFirst = true
		return copy(buffer, "hello\n"), nil
	}
	<-h.release
	return 0, io.EOF
}

// microvmReleaseOnOutput releases the input the first time output arrives.
type microvmReleaseOnOutput struct {
	release chan<- struct{}
	seen    []byte
}

func (w *microvmReleaseOnOutput) Write(data []byte) (int, error) {
	w.seen = append(w.seen, data...)
	if w.release != nil {
		w.release <- struct{}{}
		w.release = nil
	}
	return len(data), nil
}

func TestExecInputClosedByTheClientAfterOutputHasStarted(t *testing.T) {
	harness := newMicrovmHarness()
	held := make(chan struct{}, 1)
	stdout := &microvmReleaseOnOutput{release: held}
	exit, err := harness.client(t).Exec(
		microvmExecRequest("sh", "-c", "read line; echo got $line; cat; echo done"),
		&microvmHeldInput{release: held},
		stdout,
		io.Discard,
	)
	if err != nil {
		t.Fatal(err)
	}
	if exit.Code == nil || *exit.Code != 0 {
		t.Fatalf("exit: %s", microvmCode(exit))
	}
	if got := microvmLossy(stdout.seen); got != "got hello\ndone\n" {
		t.Fatalf("stdout = %q", got)
	}
}

func TestExecReportsExitCodesSignalsAndMissingPrograms(t *testing.T) {
	harness := newMicrovmHarness()
	run := func(request MicrovmExecRequest) (MicrovmExitInfo, string) {
		var stderr bytes.Buffer
		exit, err := harness.client(t).Exec(request, nil, io.Discard, &stderr)
		if err != nil {
			t.Fatal(err)
		}
		return exit, microvmLossy(stderr.Bytes())
	}
	exit, _ := run(microvmExecRequest("sh", "-c", "exit 7"))
	if got := fmt.Sprintf("%s, shell %d", microvmCode(exit), exit.ShellStatus()); got != "code Some(7), signal None, shell 7" {
		t.Fatal(got)
	}
	exit, _ = run(microvmExecRequest("sh", "-c", "kill -KILL $$"))
	if got := fmt.Sprintf("%s, shell %d", microvmCode(exit), exit.ShellStatus()); got != "code None, signal Some(9), shell 137" {
		t.Fatal(got)
	}
	exit, stderr := run(microvmExecRequest("definitely-not-a-program"))
	if exit.Code == nil || *exit.Code != 127 {
		t.Fatalf("exit: %s", microvmCode(exit))
	}
	if !strings.Contains(stderr, "not found") {
		t.Fatal(stderr)
	}
	request := microvmExecRequest("pwd")
	cwd := "/nonexistent-directory"
	request.Cwd = &cwd
	exit, stderr = run(request)
	if exit.Code == nil || *exit.Code != 127 {
		t.Fatalf("exit: %s: %s", microvmCode(exit), stderr)
	}
}

func TestDetachedExecAnswersAtOnceAndTheProcessKeepsRunning(t *testing.T) {
	harness := newMicrovmHarness()
	marker := filepath.Join(t.TempDir(), "marker")
	pid, err := harness.client(t).ExecDetached(microvmExecRequest("sh", "-c", fmt.Sprintf("sleep 0.3; touch %s", marker)))
	if err != nil {
		t.Fatal(err)
	}
	if pid == 0 {
		t.Fatal("pid 0")
	}
	if microvmExists(marker) {
		t.Fatal("the marker exists before the detached process could make it")
	}
	for range 100 {
		if microvmExists(marker) {
			return
		}
		time.Sleep(50 * time.Millisecond)
	}
	t.Fatal("the detached process never ran")
}

func microvmRawHello(t *testing.T, stream *net.UnixConn, hello MicrovmHello) *MicrovmFrameSender {
	t.Helper()
	sender := NewMicrovmFrameSender(MicrovmUnixTransport{UnixConn: stream})
	if err := sender.SendJSON(MicrovmFrameKindHello, hello); err != nil {
		t.Fatal(err)
	}
	return sender
}

func TestAPeerWithNoCommonProtocolVersionIsRefusedWithAReason(t *testing.T) {
	harness := newMicrovmHarness()
	stream, done := harness.connect(t)
	microvmRawHello(t, stream, MicrovmHello{Version: 0, MinVersion: 0})
	frames := NewMicrovmFrameReader(stream)
	reply, err := frames.Expect("the refusal")
	if err != nil {
		t.Fatal(err)
	}
	if reply.Kind != MicrovmFrameKindError {
		t.Fatalf("kind %s", reply.Kind)
	}
	if !strings.Contains(microvmLossy(reply.Payload), "do not overlap") {
		t.Fatal(microvmLossy(reply.Payload))
	}
	if err := <-done; err == nil {
		t.Fatal("the agent accepted a peer it cannot speak to")
	}
}

func TestANewerPeerIsServedAtTheVersionBothSpeak(t *testing.T) {
	harness := newMicrovmHarness()
	stream, done := harness.connect(t)
	sender := microvmRawHello(t, stream, MicrovmHello{Version: MicrovmProtocolVersion + 5, MinVersion: MicrovmMinProtocolVersion})
	frames := NewMicrovmFrameReader(stream)
	reply, err := frames.Expect("the agent hello")
	if err != nil {
		t.Fatal(err)
	}
	if reply.Kind != MicrovmFrameKindHello {
		t.Fatalf("kind %s", reply.Kind)
	}
	var hello MicrovmHello
	if err := json.Unmarshal(reply.Payload, &hello); err != nil {
		t.Fatal(err)
	}
	if hello.Version != MicrovmProtocolVersion {
		t.Fatalf("version %d", hello.Version)
	}
	if err := sender.SendJSON(MicrovmFrameKindRequest, MicrovmRequestExec{Request: microvmExecRequest("true")}); err != nil {
		t.Fatal(err)
	}
	exit, err := frames.Expect("the exit frame")
	if err != nil {
		t.Fatal(err)
	}
	if exit.Kind != MicrovmFrameKindExit {
		t.Fatalf("kind %s", exit.Kind)
	}
	if err := <-done; err != nil {
		t.Fatal(err)
	}
}

func microvmReadText(t *testing.T, path string) string {
	t.Helper()
	data, err := os.ReadFile(path)
	if err != nil {
		t.Fatal(err)
	}
	return string(data)
}

func microvmMustWrite(t *testing.T, path string, data []byte) {
	t.Helper()
	if err := os.WriteFile(path, data, 0o666); err != nil {
		t.Fatal(err)
	}
}

func microvmMustSymlink(t *testing.T, target, path string) {
	t.Helper()
	if err := os.Symlink(target, path); err != nil {
		t.Fatal(err)
	}
}

func microvmMustMkdirAll(t *testing.T, path string) {
	t.Helper()
	if err := os.MkdirAll(path, 0o777); err != nil {
		t.Fatal(err)
	}
}

func microvmMustChmod(t *testing.T, path string, mode os.FileMode) {
	t.Helper()
	if err := os.Chmod(path, mode); err != nil {
		t.Fatal(err)
	}
}

func microvmMode(t *testing.T, path string) os.FileMode {
	t.Helper()
	info, err := os.Stat(path)
	if err != nil {
		t.Fatal(err)
	}
	return info.Mode().Perm()
}

func microvmLink(t *testing.T, path string) string {
	t.Helper()
	target, err := os.Readlink(path)
	if err != nil {
		t.Fatal(err)
	}
	return target
}

func TestCopyMovesADirectoryInAndOutWithPodmanNaming(t *testing.T) {
	harness := newMicrovmHarness()
	work := t.TempDir()
	source := filepath.Join(work, "tree")
	microvmMustMkdirAll(t, filepath.Join(source, "sub"))
	microvmMustWrite(t, filepath.Join(source, "big.bin"), microvmPattern(300_000))
	microvmMustWrite(t, filepath.Join(source, "sub/a.txt"), []byte("alpha"))
	microvmMustSymlink(t, "sub/a.txt", filepath.Join(source, "link"))

	// Into an existing directory: the tree lands inside it under its own name.
	inbox := filepath.Join(work, "inbox")
	microvmMustMkdirAll(t, inbox)
	if err := harness.client(t).CopyIn(source, inbox, MicrovmCopyModeItem); err != nil {
		t.Fatal(err)
	}
	if got := microvmReadText(t, filepath.Join(inbox, "tree/big.bin")); got != string(microvmPattern(300_000)) {
		t.Fatal("big.bin differs")
	}
	if got := microvmReadText(t, filepath.Join(inbox, "tree/sub/a.txt")); got != "alpha" {
		t.Fatal(got)
	}
	if got := microvmLink(t, filepath.Join(inbox, "tree/link")); got != "sub/a.txt" {
		t.Fatal(got)
	}

	// To a path that does not exist: the destination becomes the tree's name.
	back := filepath.Join(work, "back")
	if err := harness.client(t).CopyOut(filepath.Join(inbox, "tree"), back, nil, MicrovmCopyModeItem); err != nil {
		t.Fatal(err)
	}
	if got := microvmReadText(t, filepath.Join(back, "big.bin")); got != string(microvmPattern(300_000)) {
		t.Fatal("big.bin differs after the copy out")
	}
	if got := microvmReadText(t, filepath.Join(back, "sub/a.txt")); got != "alpha" {
		t.Fatal(got)
	}

	// A missing destination parent is the agent's refusal, relayed to the client.
	err := harness.client(t).CopyIn(source, "/no/such/parent/tree", MicrovmCopyModeItem)
	if err == nil || !strings.Contains(err.Error(), "does not exist") {
		t.Fatalf("%v", err)
	}
}

func TestCopyContentsMergesIntoAnExistingDirectoryAndCreatesAMissingOne(t *testing.T) {
	harness := newMicrovmHarness()
	work := t.TempDir()
	source := filepath.Join(work, "profile")
	microvmMustMkdirAll(t, filepath.Join(source, "nested/deeper"))
	microvmMustWrite(t, filepath.Join(source, ".dotfile"), []byte("dot"))
	microvmMustWrite(t, filepath.Join(source, "nested/deeper/a.txt"), []byte("alpha"))
	microvmMustSymlink(t, "nested/deeper/a.txt", filepath.Join(source, "link"))
	microvmMustChmod(t, source, 0o750)

	// An existing destination keeps its own mode and what it already holds.
	existing := filepath.Join(work, "existing")
	microvmMustMkdirAll(t, existing)
	microvmMustWrite(t, filepath.Join(existing, "kept"), []byte("kept"))
	microvmMustChmod(t, existing, 0o711)
	if err := harness.client(t).CopyIn(source, existing, MicrovmCopyModeContents); err != nil {
		t.Fatal(err)
	}
	if got := microvmReadText(t, filepath.Join(existing, ".dotfile")); got != "dot" {
		t.Fatal(got)
	}
	if got := microvmReadText(t, filepath.Join(existing, "nested/deeper/a.txt")); got != "alpha" {
		t.Fatal(got)
	}
	if got := microvmLink(t, filepath.Join(existing, "link")); got != "nested/deeper/a.txt" {
		t.Fatal(got)
	}
	if got := microvmReadText(t, filepath.Join(existing, "kept")); got != "kept" {
		t.Fatal(got)
	}
	if microvmExists(filepath.Join(existing, "profile")) {
		t.Fatal("the directory itself must not be nested")
	}
	if got := microvmMode(t, existing); got != 0o711 {
		t.Fatalf("mode %o", got)
	}

	// A missing destination is created, with the source directory's mode.
	created := filepath.Join(work, "created")
	if err := harness.client(t).CopyIn(source, created, MicrovmCopyModeContents); err != nil {
		t.Fatal(err)
	}
	if got := microvmReadText(t, filepath.Join(created, ".dotfile")); got != "dot" {
		t.Fatal(got)
	}
	if got := microvmMode(t, created); got != 0o750 {
		t.Fatalf("mode %o", got)
	}

	// Its parent must exist, and a file in its place is refused.
	err := harness.client(t).CopyIn(source, "/no/such/parent/dest", MicrovmCopyModeContents)
	if err == nil || !strings.Contains(err.Error(), "does not exist") {
		t.Fatalf("%v", err)
	}
	file := filepath.Join(work, "file")
	microvmMustWrite(t, file, []byte("x"))
	err = harness.client(t).CopyIn(source, file, MicrovmCopyModeContents)
	if err == nil || !strings.Contains(err.Error(), "not a directory") {
		t.Fatalf("%v", err)
	}

	// Out of the guest: the same form, into a new host directory.
	back := filepath.Join(work, "back")
	if err := harness.client(t).CopyOut(source, back, nil, MicrovmCopyModeContents); err != nil {
		t.Fatal(err)
	}
	if got := microvmReadText(t, filepath.Join(back, "nested/deeper/a.txt")); got != "alpha" {
		t.Fatal(got)
	}

	// The contents form keeps the safety rules: no writing through a symlink.
	outside := filepath.Join(work, "outside")
	microvmMustMkdirAll(t, outside)
	target := filepath.Join(work, "target")
	microvmMustMkdirAll(t, target)
	microvmMustSymlink(t, outside, filepath.Join(target, "out"))
	// `target/out` is a symlink to a directory; the incoming `out/file` must
	// not go through it.
	incoming := filepath.Join(work, "incoming")
	microvmMustMkdirAll(t, filepath.Join(incoming, "out"))
	microvmMustWrite(t, filepath.Join(incoming, "out/file"), []byte("escape"))
	if err := harness.client(t).CopyIn(incoming, target, MicrovmCopyModeContents); err != nil {
		t.Fatal(err)
	}
	if microvmExists(filepath.Join(outside, "file")) {
		t.Fatal("wrote through a destination symlink")
	}
	if got := microvmReadText(t, filepath.Join(target, "out/file")); got != "escape" {
		t.Fatal(got)
	}
}

func TestUnpackingNeverLeavesTheDestinationOrWritesThroughASymlink(t *testing.T) {
	work := t.TempDir()
	destination := filepath.Join(work, "dest")
	microvmMustMkdirAll(t, destination)
	outside := filepath.Join(work, "outside")
	microvmMustMkdirAll(t, outside)

	type entry struct {
		path   string
		kind   byte
		target string
	}
	tarWith := func(entries ...entry) []byte {
		var buffer bytes.Buffer
		builder := tar.NewWriter(&buffer)
		for _, e := range entries {
			// The name goes into the header as it is: a well-behaved writer
			// would refuse "..".
			size := 0
			if e.kind == tar.TypeReg {
				size = 3
			}
			mode := int64(0o644)
			if e.kind == tar.TypeDir {
				mode = 0o755
			}
			header := &tar.Header{Name: e.path, Typeflag: e.kind, Size: int64(size), Mode: mode, Linkname: e.target, Format: tar.FormatGNU}
			if err := builder.WriteHeader(header); err != nil {
				t.Fatal(err)
			}
			if _, err := builder.Write([]byte("abc")[:size]); err != nil {
				t.Fatal(err)
			}
		}
		if err := builder.Close(); err != nil {
			t.Fatal(err)
		}
		return buffer.Bytes()
	}

	traversal := tarWith(entry{"top/../../escaped", tar.TypeReg, ""})
	err := MicrovmTarcopyUnpack(bytes.NewReader(traversal), destination, nil, MicrovmCopyModeItem)
	if err == nil || !strings.Contains(err.Error(), "leaves its destination") {
		t.Fatalf("%v", err)
	}
	if microvmExists(filepath.Join(work, "escaped")) {
		t.Fatal("an entry left its destination")
	}

	throughLink := tarWith(
		entry{"top", tar.TypeDir, ""},
		entry{"top/out", tar.TypeSymlink, outside},
		entry{"top/out/file", tar.TypeReg, ""},
	)
	err = MicrovmTarcopyUnpack(bytes.NewReader(throughLink), destination, nil, MicrovmCopyModeItem)
	if err == nil || !strings.Contains(err.Error(), "not a directory") {
		t.Fatalf("%v", err)
	}
	if microvmExists(filepath.Join(outside, "file")) {
		t.Fatal("wrote through a symlink")
	}

	device := tarWith(entry{"top", tar.TypeChar, ""})
	if err := MicrovmTarcopyUnpack(bytes.NewReader(device), destination, nil, MicrovmCopyModeItem); err == nil {
		t.Fatal("a device entry was unpacked")
	}
}

func TestStopFlushesBeforeItAnswersAndThenPowersOff(t *testing.T) {
	harness := newMicrovmHarness()
	if err := harness.client(t).Stop(); err != nil {
		t.Fatal(err)
	}
	if !harness.system.quiesced.Load() {
		t.Fatal("the agent answered before it quiesced")
	}
	for range 100 {
		if harness.system.poweredOff.Load() {
			return
		}
		time.Sleep(20 * time.Millisecond)
	}
	t.Fatal("the agent never powered the machine off")
}
