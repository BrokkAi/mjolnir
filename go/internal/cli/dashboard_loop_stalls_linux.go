// Port of mj-cli/src/dashboard/loop_stalls.rs (the Linux Schedstat).

package cli

import (
	"io"
	"log/slog"
	"os"
	"strconv"
	"strings"
	"time"
	"unicode/utf8"
)

// schedstat is the calling thread's /proc/thread-self/schedstat, kept open
// so that a sample is one read. A nil schedstat is Rust's None.
type schedstat struct {
	file *os.File
}

func openSchedstat() *schedstat {
	file, err := os.Open("/proc/thread-self/schedstat")
	if err != nil {
		slog.Debug("dashboard loop scheduler statistics are unavailable", "error", err)
		return nil
	}
	return &schedstat{file: file}
}

// read is one sample, nil when it cannot be read or parsed, or when s is
// nil.
func (s *schedstat) read() *loopStallsSample {
	if s == nil {
		return nil
	}
	// Three decimal u64 counters: CPU ns, run-queue wait ns, timeslices.
	var bytes [80]byte
	read, err := s.file.ReadAt(bytes[:], 0)
	if err != nil && err != io.EOF {
		return nil
	}
	return parseSchedstat(bytes[:read])
}

func parseSchedstat(bytes []byte) *loopStallsSample {
	if !utf8.Valid(bytes) {
		return nil
	}
	// Rust's split_ascii_whitespace.
	fields := strings.FieldsFunc(string(bytes), func(r rune) bool {
		return r == ' ' || r == '\t' || r == '\n' || r == '\f' || r == '\r'
	})
	if len(fields) < 2 {
		return nil
	}
	cpu, err := strconv.ParseUint(fields[0], 10, 64)
	if err != nil {
		return nil
	}
	runqueue, err := strconv.ParseUint(fields[1], 10, 64)
	if err != nil {
		return nil
	}
	return &loopStallsSample{cpu: time.Duration(cpu), runqueue: time.Duration(runqueue)}
}
