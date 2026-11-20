// Port of the tests of mj-worker/src/terminal.rs.

package worker

import (
	"context"
	"fmt"
	"math"
	"strings"
	"testing"
	"time"
	"unicode/utf8"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

func TestTheBufferKeepsTheTailAndLatchesTruncated(t *testing.T) {
	buffer := NewTerminalBuffer(8)
	buffer.Append([]byte("abcd"))
	if output, truncated := buffer.Read(); output != "abcd" || truncated {
		t.Fatalf("read = %q, %v", output, truncated)
	}

	buffer.Append([]byte("efghijkl"))
	output, truncated := buffer.Read()
	if output != "efghijkl" {
		t.Fatalf("the buffer must serve the last bytes: %q", output)
	}
	if !truncated {
		t.Fatal("expected truncated")
	}

	// Truncation latches: a later read cannot claim the output is whole.
	latched := NewTerminalBuffer(4)
	latched.Append([]byte(strings.Repeat("x", 4+terminalBufferSlackBytes+1)))
	if latched.BufferedLen() != 4 {
		t.Fatalf("buffered = %d", latched.BufferedLen())
	}
	latched.Append(nil)
	if _, truncated := latched.Read(); !truncated {
		t.Fatal("truncation must latch")
	}
}

func TestTheBufferServesOutputStartingOnACharacterBoundary(t *testing.T) {
	// "é" is two bytes; a cut through it must not reach the reader.
	buffer := NewTerminalBuffer(2)
	buffer.Append([]byte("aéb"))

	output, truncated := buffer.Read()
	if !truncated {
		t.Fatal("expected truncated")
	}
	if output != "b" {
		t.Fatalf("a partial character must not be served: %q", output)
	}
	if !utf8.RuneStart(output[0]) {
		t.Fatal("output does not start on a character boundary")
	}
}

func TestTheBufferStaysBoundedWhileAChildFloodsIt(t *testing.T) {
	limit := 4 * 1024
	buffer := NewTerminalBuffer(limit)
	for index := range 1024 {
		buffer.Append([]byte(strings.Repeat(string(rune('0'+index%10)), 1024)))
		if buffer.BufferedLen() > limit+terminalBufferSlackBytes {
			t.Fatalf("a flooded buffer must stay bounded, held %d", buffer.BufferedLen())
		}
	}

	output, truncated := buffer.Read()
	if !truncated {
		t.Fatal("expected truncated")
	}
	if len(output) != limit {
		t.Fatalf("a read must never exceed the limit: %d", len(output))
	}
	if !strings.HasSuffix(output, strings.Repeat("3", 1024)) {
		t.Fatal("the retained bytes must be the tail")
	}
}

func TestAShellLineCarriesTheCommandVerbatimAndQuotesTheArguments(t *testing.T) {
	// Kimi's shape: interpreter plus `-c <script>`.
	if got := ShellLine("/bin/bash", []string{"-c", "echo 'hi'; rm -rf /"}); got != `/bin/bash '-c' 'echo '\''hi'\''; rm -rf /'` {
		t.Fatalf("line = %s", got)
	}
	// Grok Build's shape: the whole line already in `command`.
	if got := ShellLine("/bin/bash -lc 'echo hi'", nil); got != "/bin/bash -lc 'echo hi'" {
		t.Fatalf("line = %s", got)
	}
}

// registerStuckTerminal registers a terminal whose supervisor never
// finishes. Its exit is already published, so teardown signals no process
// group: the PID is never used.
func registerStuckTerminal(t *testing.T, registry *TerminalRegistry, terminalID string) {
	exit := core.NewWatch[*TerminalExit](&TerminalExit{})
	pending := make(chan struct{})
	t.Cleanup(func() { close(pending) })
	registry.mu.Lock()
	defer registry.mu.Unlock()
	registry.terminals[terminalID] = &terminalEntry{
		group:      terminalProcessGroup{pid: math.MaxInt32, exit: exit},
		buffer:     newSharedTerminalBuffer(1024),
		exit:       exit,
		supervisor: core.Spawn(func() struct{} { <-pending; return struct{}{} }),
	}
}

func TestShutdownReapsStuckTerminalsUnderOneSharedDeadline(t *testing.T) {
	registry, err := NewTerminalRegistry()
	if err != nil {
		t.Fatal(err)
	}
	reports := make(chan core.RuntimeEvent, 16)
	for index := range 4 {
		registerStuckTerminal(t, registry, fmt.Sprintf("term-%d", index))
	}

	started := time.Now()
	registry.Shutdown(context.Background(), reports)
	elapsed := time.Since(started)

	if elapsed >= terminalReapTimeout*2 {
		t.Fatalf("teardown must be bounded by one reap timeout, took %s", elapsed)
	}
	var reported []string
	for {
		select {
		case event := <-reports:
			if warning, ok := event.(core.RuntimeEventWarning); ok {
				reported = append(reported, warning.Message)
				continue
			}
		default:
		}
		break
	}
	if len(reported) != 4 {
		t.Fatalf("%q", reported)
	}
	for index := range 4 {
		needle := fmt.Sprintf("term-%d did not finish", index)
		found := false
		for _, message := range reported {
			found = found || strings.Contains(message, needle)
		}
		if !found {
			t.Fatalf("every stuck terminal must still be reported: %q", reported)
		}
	}
}
