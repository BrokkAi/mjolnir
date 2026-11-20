// Port of anyhow 1.0.102 tests/test_context.rs
//
// The Rust tests also watch each value being dropped (DetectDrop). A Go error
// is collected, not dropped, so those assertions have no counterpart; what
// each test says about downcasting is kept.

package anyhow

import (
	"strconv"
	"testing"
)

// https://github.com/dtolnay/anyhow/issues/18
func TestItTestContext_test_inference(t *testing.T) {
	x := "1"
	parsed, err := strconv.ParseUint(x, 10, 32)
	if err := Context(err, "..."); err != nil {
		t.Fatal(err)
	}
	assertEq(t, uint32(parsed), 1)
}

// highLevel and midLevel are the context_type! types: Display, not errors.
type highLevel struct {
	message string
}

func (c *highLevel) String() string { return c.message }

type midLevel struct {
	message string
}

func (c *midLevel) String() string { return c.message }

// lowLevel is the thiserror type `#[error("{message}")] struct LowLevel`.
type lowLevel struct {
	message string
}

func (e *lowLevel) Error() string { return e.message }

func makeChain() error {
	low := &lowLevel{message: "no such file or directory"}

	// impl Context for Result<T, E>
	mid := Context(low, &midLevel{message: "failed to load config"})

	// impl Context for Result<T, Error>
	high := Context(mid, &highLevel{message: "failed to start server"})

	return high
}

func TestItTestContext_test_downcast_ref(t *testing.T) {
	err := makeChain()

	assert(t, !Is[string](err))
	_, ok := DowncastRef[string](err)
	assert(t, !ok)

	assert(t, Is[*highLevel](err))
	high, _ := DowncastRef[*highLevel](err)
	assertEq(t, high.String(), "failed to start server")

	assert(t, Is[*midLevel](err))
	mid, _ := DowncastRef[*midLevel](err)
	assertEq(t, mid.String(), "failed to load config")

	assert(t, Is[*lowLevel](err))
	low, _ := DowncastRef[*lowLevel](err)
	assertEq(t, low.Error(), "no such file or directory")
}

func TestItTestContext_test_downcast_high(t *testing.T) {
	err := makeChain()

	high, failed := Downcast[*highLevel](err)
	assert(t, failed == nil)
	assertEq(t, high.message, "failed to start server")
}

func TestItTestContext_test_downcast_mid(t *testing.T) {
	err := makeChain()

	mid, failed := Downcast[*midLevel](err)
	assert(t, failed == nil)
	assertEq(t, mid.message, "failed to load config")
}

func TestItTestContext_test_downcast_low(t *testing.T) {
	err := makeChain()

	low, failed := Downcast[*lowLevel](err)
	assert(t, failed == nil)
	assertEq(t, low.message, "no such file or directory")
}

func TestItTestContext_test_unsuccessful_downcast(t *testing.T) {
	err := makeChain()

	_, failed := Downcast[string](err)
	// `Err(self)`: the error comes back whole.
	assert(t, failed == err)
}

func TestItTestContext_test_root_cause(t *testing.T) {
	err := makeChain()

	assertEq(t, RootCause(err).Error(), "no such file or directory")
}
