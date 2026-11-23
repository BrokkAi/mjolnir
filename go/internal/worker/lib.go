// Port of mj-worker/src/lib.rs.

// Package worker is Hel's target-side worker: the daemon and stdio proxy that
// run inside a container or on an SSH host. It is the Go port of the
// mj-worker crate (`brokk-mj-worker-runtime`, library `mj_worker`).
//
// The crate's modules (acp, relay, worker_runtime, terminal, user_shell,
// checkpoint and the rest) are files of this one package, named as in
// mj-core's port: module `foo/bar.rs` is `foo_bar.go`. The MicroVM engine is
// the mj-microvm crate, Go package microvm; the worker's MCP servers are
// mj-worker-mcp, Go package workermcp; and the push-review capture and
// pre-push gate are mj-worker-review, Go package workerreview.
//
// Rust's `crate::acp::RuntimeEvent` is a re-export of mj-core's; this package
// uses core.RuntimeEvent directly. A bounded tokio mpsc sender of runtime
// events is `chan<- core.RuntimeEvent`.
package worker

import (
	"errors"
	"fmt"
	"io/fs"
	"maps"
	"net"
	"os"
	"os/exec"
	"path/filepath"
	"slices"
	"strings"
	"time"
	"unicode/utf8"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// DefaultWorkerLogFilter is the default diagnostics filter: it includes the
// local Jev and Mjolnir guardian decision trails. An explicit RUST_LOG
// overrides it.
const DefaultWorkerLogFilter = "warn,mj_jev=info,mj_guardian=info"

// pathTryExists is Path::try_exists: false only for a path that is not
// there (it follows symlinks, so a dangling one is not there), an error for
// any other failure to inspect it.
func pathTryExists(path string) (bool, error) {
	_, err := os.Stat(path)
	if err == nil {
		return true, nil
	}
	if errors.Is(err, fs.ErrNotExist) {
		return false, nil
	}
	return false, err
}

// floorCharBoundary is str::floor_char_boundary: the largest index at most
// index that starts a character of text.
func floorCharBoundary(text string, index int) int {
	if index >= len(text) {
		return len(text)
	}
	for index > 0 && !utf8.RuneStart(text[index]) {
		index--
	}
	return index
}

// childCommand is a command whose environment is exactly environment, as
// Rust's env_clear followed by env does. As Rust's Command does, a PATH
// given to the child is also where a bare program name is looked up.
func childCommand(program string, args []string, environment map[string]string) *exec.Cmd {
	command := exec.Command(program, args...)
	command.Env = []string{}
	for _, name := range slices.Sorted(maps.Keys(environment)) {
		command.Env = append(command.Env, name+"="+environment[name])
	}
	if path, ok := environment["PATH"]; ok && !strings.ContainsAny(program, `/\`) {
		for _, directory := range filepath.SplitList(path) {
			if directory == "" {
				directory = "."
			}
			if resolved, err := exec.LookPath(filepath.Join(directory, program)); err == nil {
				command.Path = resolved
				command.Err = nil
				break
			}
		}
	}
	return command
}

// startWithOutputPipes starts command with its stdout and stderr on pipes of
// their own and returns the read ends. Unlike exec's StdoutPipe, Wait leaves
// these ends open, so a reader may still be draining when the child is
// reaped, as Rust's concurrent drain and wait require. Stdin stays the null
// device.
func startWithOutputPipes(command *exec.Cmd) (stdout, stderr *os.File, err error) {
	stdoutRead, stdoutWrite, err := os.Pipe()
	if err != nil {
		return nil, nil, err
	}
	stderrRead, stderrWrite, err := os.Pipe()
	if err != nil {
		stdoutRead.Close()
		stdoutWrite.Close()
		return nil, nil, err
	}
	command.Stdout = stdoutWrite
	command.Stderr = stderrWrite
	err = core.SpawnChild(command)
	// The child holds its own copies of the write ends; ours would keep the
	// pipes open after it and its descendants exit.
	stdoutWrite.Close()
	stderrWrite.Close()
	if err != nil {
		stdoutRead.Close()
		stderrRead.Close()
		return nil, nil, err
	}
	return stdoutRead, stderrRead, nil
}

// timeoutConn bounds each read and each write on conn separately, as
// set_read_timeout and set_write_timeout do on a Rust socket. Go deadlines
// are absolute, so every operation sets its own. A zero duration leaves that
// direction unbounded.
type timeoutConn struct {
	conn  net.Conn
	read  time.Duration
	write time.Duration
}

func (c timeoutConn) Read(buffer []byte) (int, error) {
	if c.read > 0 {
		if err := c.conn.SetReadDeadline(time.Now().Add(c.read)); err != nil {
			return 0, err
		}
	}
	return c.conn.Read(buffer)
}

func (c timeoutConn) Write(buffer []byte) (int, error) {
	if c.write > 0 {
		if err := c.conn.SetWriteDeadline(time.Now().Add(c.write)); err != nil {
			return 0, err
		}
	}
	return c.conn.Write(buffer)
}

// fromUTF8 is Rust's String::from_utf8, failing with the text of Rust's
// Utf8Error.
func fromUTF8(data []byte) (string, error) {
	for index := 0; index < len(data); {
		valid, complete := utf8Sequence(data[index:])
		switch {
		case complete:
			index += valid
		case valid > 0 && index+valid == len(data):
			return "", fmt.Errorf("incomplete utf-8 byte sequence from index %d", index)
		default:
			return "", fmt.Errorf("invalid utf-8 sequence of %d bytes from index %d", max(valid, 1), index)
		}
	}
	return string(data), nil
}

// utf8Sequence measures the UTF-8 sequence text starts with: valid is how
// many of its leading bytes form a valid sequence or a valid prefix of one,
// and complete whether that is a whole character. An invalid first byte
// gives zero.
func utf8Sequence(text []byte) (valid int, complete bool) {
	first := text[0]
	width := 0
	switch {
	case first < 0x80:
		return 1, true
	case first >= 0xC2 && first <= 0xDF:
		width = 2
	case first >= 0xE0 && first <= 0xEF:
		width = 3
	case first >= 0xF0 && first <= 0xF4:
		width = 4
	default:
		return 0, false
	}
	low, high := byte(0x80), byte(0xBF)
	switch first {
	case 0xE0:
		low = 0xA0
	case 0xED:
		high = 0x9F
	case 0xF0:
		low = 0x90
	case 0xF4:
		high = 0x8F
	}
	for offset := 1; offset < width; offset++ {
		if offset >= len(text) {
			return offset, false
		}
		next := text[offset]
		if offset == 1 && (next < low || next > high) || offset > 1 && (next < 0x80 || next > 0xBF) {
			return offset, false
		}
	}
	return width, true
}

// fromUTF8Lossy is Rust's String::from_utf8_lossy: every maximal invalid
// subsequence becomes one U+FFFD. strings.ToValidUTF8 differs: it replaces a
// whole run of invalid bytes with a single replacement.
func fromUTF8Lossy(data []byte) string {
	if utf8.Valid(data) {
		return string(data)
	}
	var out strings.Builder
	out.Grow(len(data) + 2)
	for len(data) > 0 {
		r, size := utf8.DecodeRune(data)
		if r != utf8.RuneError || size > 1 {
			out.Write(data[:size])
			data = data[size:]
			continue
		}
		out.WriteString("�")
		data = data[lossyInvalidPrefix(data):]
	}
	return out.String()
}

// lossyInvalidPrefix is the length of the maximal subpart of an ill-formed
// UTF-8 sequence at the start of data, as Rust's Utf8Chunks measures it: a
// valid lead byte followed by as many of its allowed continuation bytes as
// are present.
func lossyInvalidPrefix(data []byte) int {
	first := data[0]
	low, high := byte(0x80), byte(0xBF)
	width := 0
	switch {
	case first >= 0xC2 && first <= 0xDF:
		width = 2
	case first == 0xE0:
		width, low = 3, 0xA0
	case first >= 0xE1 && first <= 0xEC, first == 0xEE, first == 0xEF:
		width = 3
	case first == 0xED:
		width, high = 3, 0x9F
	case first == 0xF0:
		width, low = 4, 0x90
	case first >= 0xF1 && first <= 0xF3:
		width = 4
	case first == 0xF4:
		width, high = 4, 0x8F
	default:
		return 1
	}
	n := 1
	for n < width && n < len(data) {
		next := data[n]
		if n == 1 && (next < low || next > high) {
			break
		}
		if n > 1 && (next < 0x80 || next > 0xBF) {
			break
		}
		n++
	}
	return n
}
