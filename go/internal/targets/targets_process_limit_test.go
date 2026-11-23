// Port of the tests in mj-targets/src/targets/process_limit.rs.

package targets

import (
	"errors"
	"fmt"
	"strings"
	"testing"
)

// Hard-won: #1161: A full container PID limit must be recognized when the parent harness runs out of slots.
func TestEveryWayAFullTargetReportsItselfIsRecognised(t *testing.T) {
	for _, text := range []string{
		"failed to spawn thread: Resource temporarily unavailable (os error 11)",
		"sh: 1: Cannot fork",
		"OS can't spawn worker thread: Resource temporarily unavailable (os error 11)",
	} {
		err := fmt.Errorf("start the sub-agent: %w", errors.New(text))
		if !ShowsProcessExhaustion(err) {
			t.Fatal(text)
		}
	}
	for _, text := range []string{
		"Connection refused (os error 111)",
		"Connection timed out (os error 110)",
		"permission denied (os error 13)",
	} {
		if ShowsProcessExhaustion(errors.New(text)) {
			t.Fatal(text)
		}
	}
}

// Hard-won: #1161: PID exhaustion errors must explain the current and maximum process counts.
func TestTheMessageNamesTheCountsOrSaysTheyCouldNotBeRead(t *testing.T) {
	err := errors.New("sh: 1: Cannot fork")
	limit := uint64(8192)
	read := ProcessExhaustionMessage(&PidsUsage{Current: 8190, Max: &limit}, nil, err)
	if !strings.HasPrefix(read, "the parent's container ran out of process slots") {
		t.Fatal(read)
	}
	if !strings.Contains(read, "pids.current 8190 of pids.max 8192") {
		t.Fatal(read)
	}
	if !strings.Contains(read, "Close sub-agents you no longer need") {
		t.Fatal(read)
	}
	if !strings.HasSuffix(read, "The original error was: sh: 1: Cannot fork") {
		t.Fatal(read)
	}

	unread := ProcessExhaustionMessage(nil, errors.New("Cannot fork"), err)
	if !strings.Contains(unread, "its pids.current and pids.max could not be read") {
		t.Fatal(unread)
	}
	if !strings.Contains(unread, "Close sub-agents you no longer need") {
		t.Fatal(unread)
	}
}

// Hard-won: #1161: PID diagnostics must parse cgroup current/max values and the unlimited ceiling.
func TestPidCountsParseWithAndWithoutALimit(t *testing.T) {
	usage, err := parsePidsUsage("1684\n8192\n")
	if err != nil {
		t.Fatal(err)
	}
	if usage.Current != 1684 || usage.Max == nil || *usage.Max != 8192 {
		t.Fatalf("usage = %+v", usage)
	}
	usage, err = parsePidsUsage("12\nmax\n")
	if err != nil {
		t.Fatal(err)
	}
	if usage.Current != 12 || usage.Max != nil {
		t.Fatalf("usage = %+v", usage)
	}
	if _, err := parsePidsUsage(""); err == nil {
		t.Fatal("empty pid counts parsed")
	}
}
