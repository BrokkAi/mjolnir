// Port of the tests in mj-core/src/text.rs.

package core

import "testing"

// Hard-won: 25c6af83: Setup and Close showed user-facing counts with malformed noun suffixes.
func TestCountedAgreesTheNounWithTheCount(t *testing.T) {
	if got := Counted(uint(0), "entry", "entries"); got != "0 entries" {
		t.Fatalf("got %q", got)
	}
	if got := Counted(uint(1), "entry", "entries"); got != "1 entry" {
		t.Fatalf("got %q", got)
	}
	if got := Counted(int64(2), "command", "commands"); got != "2 commands" {
		t.Fatalf("got %q", got)
	}
}
