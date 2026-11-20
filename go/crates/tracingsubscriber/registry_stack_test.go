// Port of tracing-subscriber 0.3.23 src/registry/stack.rs

package tracingsubscriber

import (
	"os"
	"testing"

	"github.com/BrokkAi/mjolnir/go/crates/tracing"
	"github.com/BrokkAi/mjolnir/go/rs"
)

func TestRegistryStack_pop_last_span(t *testing.T) {
	stack := spanStack{}
	id := tracing.SpanIdFromU64(1)
	stack.push(id)

	assertTrue(t, stack.pop(id))
}

func TestRegistryStack_pop_first_span(t *testing.T) {
	stack := spanStack{}
	stack.push(tracing.SpanIdFromU64(1))
	stack.push(tracing.SpanIdFromU64(2))

	id := tracing.SpanIdFromU64(1)
	assertTrue(t, stack.pop(id))
}

func assertTrue(t *testing.T, condition bool, message ...string) {
	t.Helper()
	if !condition {
		if len(message) > 0 {
			t.Fatal(message[0])
		}
		t.Fatal("assertion failed")
	}
}

func assertEq[T comparable](t *testing.T, left, right T) {
	t.Helper()
	if left != right {
		t.Fatalf("assertion `left == right` failed\n  left: %#v\n right: %#v", left, right)
	}
}

// assertEqual is `assert_eq!` on values compared as Rust compares them.
func assertEqual(t *testing.T, left, right any) {
	t.Helper()
	if !rs.Eq(left, right) {
		t.Fatalf("assertion `left == right` failed\n  left: %s\n right: %s", rs.DebugString(left), rs.DebugString(right))
	}
}

// ignored is `#[ignore]`: the test runs only with MJ_RUN_IGNORED=1.
func ignored(t *testing.T, reason string) {
	t.Helper()
	if os.Getenv("MJ_RUN_IGNORED") != "1" {
		t.Skip(reason)
	}
}
