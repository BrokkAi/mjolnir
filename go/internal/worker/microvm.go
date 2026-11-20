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
	"errors"
	"fmt"
	"strings"
	"syscall"
	"unicode"
	"unicode/utf8"

	"github.com/BrokkAi/acp-go/jsonvalue"
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
