// Port of the tests in mj-core/src/logging.rs.

package core

import (
	"fmt"
	"io"
	"strings"
	"sync"
	"sync/atomic"
	"syscall"
	"testing"
)

type loggingFullDisk struct {
	full   *atomic.Bool
	mu     *sync.Mutex
	output *[]byte
}

func (d loggingFullDisk) Write(bytes []byte) (int, error) {
	if d.full.Load() {
		return 0, syscall.ENOSPC
	}
	d.mu.Lock()
	defer d.mu.Unlock()
	*d.output = append(*d.output, bytes...)
	return len(bytes), nil
}

func (d loggingFullDisk) Flush() error {
	if d.full.Load() {
		return syscall.ENOSPC
	}
	return nil
}

func loggingTailBytes(tail []string) int {
	total := 0
	for _, line := range tail {
		total += len(line)
	}
	return total
}

// Hard-won: d7ca109e4: diagnostic ENOSPC panicked a worker and permanently stopped daemon logging.
func TestFullDiskDropsLinesRetainsBoundedTailAndRecoversOnce(t *testing.T) {
	var full atomic.Bool
	full.Store(true)
	var mu sync.Mutex
	var output []byte
	writer := NewResilientWriter(loggingFullDisk{full: &full, mu: &mu, output: &output})
	for index := range 1000 {
		if _, err := writer.Write([]byte(fmt.Sprintf("%d: %s\n", index, strings.Repeat("é", 100)))); err != nil {
			t.Fatal(err)
		}
	}
	if err := writer.Flush(); err != nil {
		t.Fatal(err)
	}
	failure := writer.DroppedLines()
	if failure == nil {
		t.Fatal("dropped lines are recorded")
	}
	if failure.Count != 1000 {
		t.Fatalf("count %d", failure.Count)
	}
	if failure.Kind != ioErrorKindName(syscall.ENOSPC) || failure.Kind != "StorageFull" {
		t.Fatalf("kind %q", failure.Kind)
	}
	if failure.FirstAt > failure.LastAt {
		t.Fatalf("%s after %s", failure.FirstAt, failure.LastAt)
	}
	tail := writer.RecentLog()
	if loggingTailBytes(tail) > recentLogBytes {
		t.Fatalf("tail holds %d bytes", loggingTailBytes(tail))
	}
	if !strings.HasPrefix(tail[len(tail)-1], "999:") {
		t.Fatalf("last line %q", tail[len(tail)-1])
	}
	mu.Lock()
	if len(output) != 0 {
		t.Fatal("nothing reached the full disk")
	}
	mu.Unlock()
	full.Store(false)
	if _, err := writer.Write([]byte("recovered\n")); err != nil {
		t.Fatal(err)
	}
	if _, err := writer.Write([]byte("next\n")); err != nil {
		t.Fatal(err)
	}
	mu.Lock()
	text := string(output)
	mu.Unlock()
	if count := strings.Count(text, "dropped 1000 log lines between"); count != 1 {
		t.Fatalf("summary appears %d times in %q", count, text)
	}
	if !strings.Contains(text, failure.Error) {
		t.Fatalf("%q lacks %q", text, failure.Error)
	}
	if !strings.HasSuffix(text, "recovered\nnext\n") {
		t.Fatalf("output %q", text)
	}
	if writer.DroppedLines() != nil {
		t.Fatal("the failure interval ended")
	}
	recent := strings.Join(writer.RecentLog(), "")
	if count := strings.Count(recent, "dropped 1000 log lines between"); count != 1 {
		t.Fatalf("summary appears %d times in the tail", count)
	}
	if !strings.HasSuffix(recent, "recovered\nnext\n") {
		t.Fatalf("tail %q", recent)
	}
}

// Hard-won: d7ca109e4: oversized and escaped diagnostic tails must stay bounded on disk.
func TestOversizedUnicodeLineKeepsTheLatestBytesWithoutPanicking(t *testing.T) {
	writer := NewResilientWriter(io.Discard)
	if _, err := writer.Write([]byte(strings.Repeat("é", 100_000) + "last\n")); err != nil {
		t.Fatal(err)
	}
	tail := writer.RecentLog()
	if loggingTailBytes(tail) > recentLogBytes {
		t.Fatalf("tail holds %d bytes", loggingTailBytes(tail))
	}
	if !strings.HasSuffix(tail[len(tail)-1], "last\n") {
		t.Fatalf("last line ends %q", tail[len(tail)-1][len(tail[len(tail)-1])-10:])
	}
}
