// Port of mj-worker/src/lib.rs.

// Package worker is Hel's target-side worker: the daemon and stdio proxy that
// run inside a container or on an SSH host. It is the Go port of the
// mj-worker crate (`brokk-mj-worker-runtime`, library `mj_worker`).
//
// The crate's modules (acp, relay, worker_runtime, terminal, user_shell,
// checkpoint, review, microvm and the rest) are files of this one package,
// named as in mj-core's port: module `foo/bar.rs` is `foo_bar.go`.
//
// Rust's `crate::acp::RuntimeEvent` is a re-export of mj-core's; this package
// uses core.RuntimeEvent directly. A bounded tokio mpsc sender of runtime
// events is `chan<- core.RuntimeEvent`.
package worker

import (
	"strings"
	"unicode/utf8"
)

// DefaultWorkerLogFilter is the default diagnostics filter: it includes the
// local Jev and Mjolnir guardian decision trails. An explicit RUST_LOG
// overrides it.
const DefaultWorkerLogFilter = "warn,mj_jev=info,mj_guardian=info"

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
