//go:build linux

// Port of mj-worker/src/microvm/mod.rs.
//
// `mj-worker microvm`: the host engine that runs OCI images as Nitro
// Sandbox microVMs, and the guest agent that serves them.
//
// The engine is a Podman-compatible command line (microvm_cli.go) over a
// state directory it alone owns (microvm_state.go). Images become ext4 disks
// (microvm_image.go), volumes are ext4 disk files (microvm_volume.go), VMs
// are systemd transient units running Firecracker (microvm_vm.go), and the
// engine reaches into a VM over the vsock protocol (microvm_protocol.go,
// microvm_client.go) to a guest agent (microvm_guest_agent.go) that runs as
// PID 1 from an initrd the engine builds (microvm_boot.go).
//
// Firecracker, vsock, TAP devices and PID 1 duties exist only on Linux, so
// every microvm file builds only there. The Rust modules share one Go
// package with the rest of mj-worker, so every name from them starts with
// Microvm (or microvm), and free functions also carry their module's name
// (MicrovmVMRun is vm::run, MicrovmImagePrune is image::prune).

package worker

import (
	"bytes"
	"encoding/json"
	"errors"
	"fmt"
	"math"
	"os"
	"strconv"
	"strings"
	"syscall"
	"unicode"
	"unicode/utf8"

	"github.com/BrokkAi/acp-go/jsonvalue"
	"golang.org/x/sys/unix"
)

// MicrovmMain is the entry point for `mj-worker microvm <args>`; it
// returns the process exit status.
func MicrovmMain(args []string) (int, error) {
	return MicrovmCLIMain(args)
}

// MicrovmOwner is the owner a copy gives the entries it creates: Rust's
// `Option<(u32, u32)>` is a *MicrovmOwner.
type MicrovmOwner struct {
	UID uint32
	GID uint32
}

// microvmLossy is Rust's String::from_utf8_lossy.
func microvmLossy(data []byte) string {
	return strings.ToValidUTF8(string(data), "�")
}

// microvmDebugString formats a string as Rust's `{:?}` does.
func microvmDebugString(value string) string {
	var builder strings.Builder
	builder.WriteByte('"')
	for _, r := range value {
		switch r {
		case '"':
			builder.WriteString(`\"`)
		case '\\':
			builder.WriteString(`\\`)
		case '\n':
			builder.WriteString(`\n`)
		case '\r':
			builder.WriteString(`\r`)
		case '\t':
			builder.WriteString(`\t`)
		case 0:
			builder.WriteString(`\0`)
		default:
			if unicode.IsPrint(r) || r == ' ' {
				builder.WriteRune(r)
			} else {
				fmt.Fprintf(&builder, `\u{%x}`, r)
			}
		}
	}
	builder.WriteByte('"')
	return builder.String()
}

// microvmIOError is err as Rust's io::Error displays it, for the context
// chains anyhow prints: an OS error shows the system's description and its
// number (`No such file or directory (os error 2)`) without Go's operation
// and path prefix. It unwraps to err, so errors.Is and errors.As still see
// the errno.
func microvmIOError(err error) error {
	if err == nil {
		return nil
	}
	return microvmRustIOError{err: err}
}

type microvmRustIOError struct {
	err error
}

func (e microvmRustIOError) Error() string {
	var errno syscall.Errno
	if !errors.As(e.err, &errno) {
		return e.err.Error()
	}
	description := errno.Error()
	if description != "" {
		first, size := utf8.DecodeRuneInString(description)
		description = string(unicode.ToUpper(first)) + description[size:]
	}
	return fmt.Sprintf("%s (os error %d)", description, int(errno))
}

func (e microvmRustIOError) Unwrap() error { return e.err }

// microvmLabelsObject is a label map as serde_json::to_value writes a
// BTreeMap: an object with its keys in order, `{}` when empty.
func microvmLabelsObject(labels map[string]string) jsonvalue.Object {
	values := make(map[string]any, len(labels))
	for key, value := range labels {
		values[key] = value
	}
	return jsonvalue.ObjectOf(values)
}

// microvmStringValues is a list of strings as serde_json::to_value writes
// a Vec<String>: an array, `[]` when empty.
func microvmStringValues(items []string) []any {
	values := make([]any, len(items))
	for i, item := range items {
		values[i] = item
	}
	return values
}

// microvmWaitStatusString formats a wait status as Rust's ExitStatus
// Display does: `exit status: 3`, `signal: 9 (SIGKILL)`.
func microvmWaitStatusString(status syscall.WaitStatus) string {
	signalName := func(signal syscall.Signal) string {
		if name := unix.SignalName(signal); name != "" {
			return " (" + name + ")"
		}
		return ""
	}
	switch {
	case status.Exited():
		return fmt.Sprintf("exit status: %d", status.ExitStatus())
	case status.Signaled():
		text := fmt.Sprintf("signal: %d%s", int(status.Signal()), signalName(status.Signal()))
		if status.CoreDump() {
			text += " (core dumped)"
		}
		return text
	case status.Stopped():
		return fmt.Sprintf("stopped (not terminated) by signal: %d%s", int(status.StopSignal()), signalName(status.StopSignal()))
	case status.Continued():
		return "continued (WIFCONTINUED)"
	}
	return fmt.Sprintf("unrecognised wait status: %d %#x", int(status), int(status))
}

// microvmExitStatus is a finished process's status as Rust's ExitStatus
// Display writes it.
func microvmExitStatus(state *os.ProcessState) string {
	if state == nil {
		return "unknown status"
	}
	if status, ok := state.Sys().(syscall.WaitStatus); ok {
		return microvmWaitStatusString(status)
	}
	return fmt.Sprintf("exit status: %d", state.ExitCode())
}

// microvmMarshalJSON encodes v as serde_json::to_vec does: compact, and
// without encoding/json's HTML escaping.
func microvmMarshalJSON(v any) ([]byte, error) {
	var buffer bytes.Buffer
	encoder := json.NewEncoder(&buffer)
	encoder.SetEscapeHTML(false)
	if err := encoder.Encode(v); err != nil {
		return nil, err
	}
	return bytes.TrimSuffix(buffer.Bytes(), []byte("\n")), nil
}

// microvmPrettyJSON encodes v as serde_json::to_string_pretty does: two
// spaces of indentation, `"key": value`, and `[]`/`{}` for empty
// containers. Values the engine builds (jsonvalue objects, []any and
// scalars) keep serde_json's string escapes.
func microvmPrettyJSON(v any) (string, error) {
	var compact []byte
	var err error
	switch v.(type) {
	case jsonvalue.Object, []any, nil, string, bool, int64, uint64, float64:
		compact, err = jsonvalue.Marshal(v)
	default:
		compact, err = microvmMarshalJSON(v)
	}
	if err != nil {
		return "", err
	}
	var indented bytes.Buffer
	if err := json.Indent(&indented, compact, "", "  "); err != nil {
		return "", err
	}
	return indented.String(), nil
}

// microvmLines splits text as Rust's str::lines does: on `\n`, without a
// trailing `\r`, and with no empty last line for a final newline.
func microvmLines(text string) []string {
	if text == "" {
		return nil
	}
	lines := strings.Split(strings.TrimSuffix(text, "\n"), "\n")
	for i, line := range lines {
		lines[i] = strings.TrimSuffix(line, "\r")
	}
	return lines
}

// microvmIntError is a Rust integer parse error (core::num::ParseIntError);
// its text is the Display Rust prints.
type microvmIntError string

func (e microvmIntError) Error() string { return string(e) }

const (
	microvmIntEmpty        microvmIntError = "cannot parse integer from empty string"
	microvmIntInvalidDigit microvmIntError = "invalid digit found in string"
	microvmIntPosOverflow  microvmIntError = "number too large to fit in target type"
	microvmIntNegOverflow  microvmIntError = "number too small to fit in target type"
)

// microvmParseUint parses text as Rust's `u<bits>::from_str` does: an
// optional `+`, then decimal digits only.
func microvmParseUint(text string, bits int) (uint64, error) {
	if text == "" {
		return 0, microvmIntEmpty
	}
	digits := text
	if digits[0] == '+' || digits[0] == '-' {
		if digits[0] == '-' || len(digits) == 1 {
			return 0, microvmIntInvalidDigit
		}
		digits = digits[1:]
	}
	limit := ^uint64(0)
	if bits < 64 {
		limit = uint64(1)<<bits - 1
	}
	var value uint64
	for i := 0; i < len(digits); i++ {
		c := digits[i]
		if c < '0' || c > '9' {
			return 0, microvmIntInvalidDigit
		}
		digit := uint64(c - '0')
		if value > (limit-digit)/10 {
			return 0, microvmIntPosOverflow
		}
		value = value*10 + digit
	}
	return value, nil
}

// microvmParseInt64 parses text as Rust's `i64::from_str` does.
func microvmParseInt64(text string) (int64, error) {
	if text == "" {
		return 0, microvmIntEmpty
	}
	negative := text[0] == '-'
	digits := text
	if text[0] == '+' || text[0] == '-' {
		if len(text) == 1 {
			return 0, microvmIntInvalidDigit
		}
		digits = text[1:]
	}
	var magnitude uint64
	for i := 0; i < len(digits); i++ {
		c := digits[i]
		if c < '0' || c > '9' {
			return 0, microvmIntInvalidDigit
		}
		magnitude = magnitude*10 + uint64(c-'0')
		if negative && magnitude > 1<<63 {
			return 0, microvmIntNegOverflow
		}
		if !negative && magnitude > 1<<63-1 {
			return 0, microvmIntPosOverflow
		}
	}
	if negative {
		return -int64(magnitude-1) - 1, nil
	}
	return int64(magnitude), nil
}

// microvmParseU32 is Rust's `str::parse::<u32>().ok()`.
func microvmParseU32(text string) (uint32, bool) {
	value, err := microvmParseUint(text, 32)
	return uint32(value), err == nil
}

// microvmRustFloat formats f as Rust's f64 Display does: plain decimal
// digits, never an exponent, `inf`, `-inf` and `NaN`.
func microvmRustFloat(f float64) string {
	switch {
	case math.IsInf(f, 1):
		return "inf"
	case math.IsInf(f, -1):
		return "-inf"
	case math.IsNaN(f):
		return "NaN"
	}
	return strconv.FormatFloat(f, 'f', -1, 64)
}

// microvmRustJoin is Rust's Path::join on path text: an absolute part
// replaces the base, and nothing is cleaned.
func microvmRustJoin(base, part string) string {
	switch {
	case strings.HasPrefix(part, "/") || base == "":
		return part
	case strings.HasSuffix(base, "/"):
		return base + part
	}
	return base + "/" + part
}
