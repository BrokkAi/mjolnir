// Port of mj-tui/src/wizards/resources.rs tests.

package tui

import (
	"math"
	"testing"

	"github.com/BrokkAi/mjolnir/go/internal/chat"
	"github.com/BrokkAi/mjolnir/go/internal/core"
)

func TestDecimalMemoryRoundTripsBytesAndChecksInvalidSizes(t *testing.T) {
	for _, bytes := range []uint64{1, gib - 1, gib, 32 * gib, 48*gib + 123456, math.MaxUint64} {
		if parsed, ok := parseMemoryGib(memoryGibText(bytes)); !ok || parsed != bytes {
			t.Fatalf("%d: %q parsed as %d (%t)", bytes, memoryGibText(bytes), parsed, ok)
		}
	}
	for text, want := range map[string]uint64{
		"0.1": 107374182,
		"1.5": gib + gib/2,
		".5":  gib / 2,
	} {
		if parsed, ok := parseMemoryGib(text); !ok || parsed != want {
			t.Fatalf("%q parsed as %d (%t), want %d", text, parsed, ok, want)
		}
	}
	for _, text := range []string{"", "-1", "NaN", "1GiB", "1.2.3", "18446744073709551615"} {
		if parsed, ok := parseMemoryGib(text); ok {
			t.Fatalf("%q parsed as %d, want a refusal", text, parsed)
		}
	}
}

// Hard-won: 08c676c: the wizard printed a host limit with precision its field rejected.
func TestHostMemoryLimitMessageIsReadable(t *testing.T) {
	editor := ResourceEditor{
		cpu:    chat.TextInputFromValue("4"),
		memory: chat.TextInputFromValue("999"),
	}
	limit := 98*gib + gib/5 + 12345
	_, err := editor.allocation(&HostLimits{Cpus: 8, MemoryBytes: limit}, true)
	if err == nil {
		t.Fatal("999 GiB exceeds the host")
	}
	if message := err.Error(); message != "RAM exceeds this host's 98.2 GiB." {
		t.Fatalf("message %q", message)
	}
}

func TestExistingSessionsCanKeepASizeBelowTheNewSessionMinimum(t *testing.T) {
	editor := ResourceEditor{
		cpu:    chat.TextInputFromValue("1"),
		memory: chat.TextInputFromValue("2"),
	}

	allocation, err := editor.allocation(nil, false)
	if err != nil {
		t.Fatalf("an existing size is accepted: %v", err)
	}
	if allocation != (core.SessionResourceAllocationContainer{Cpus: 1, MemoryBytes: 2 * gib}) {
		t.Fatalf("allocation %+v", allocation)
	}
	if _, err := editor.allocation(nil, true); err == nil || err.Error() != "CPU must be at least 2." {
		t.Fatalf("error %v, want the new-session minimum", err)
	}
}
