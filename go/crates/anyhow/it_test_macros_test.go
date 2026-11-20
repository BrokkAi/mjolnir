// Port of anyhow 1.0.102 tests/test_macros.rs
//
// test_ensure_nonbool and test_temporaries check what the macros accept at
// compile time and have no counterpart.

package anyhow

import "testing"

func TestItTestMacros_test_messages(t *testing.T) {
	assertEq(t, "oh no!", bailLiteral().Error())
	assertEq(t, "oh no!", bailError().Error())
}

func TestItTestMacros_test_messages_fmt(t *testing.T) {
	requireFormatter(t)
	assertEq(t, "oh no!", bailFmt().Error())
}

func TestItTestMacros_test_ensure(t *testing.T) {
	f := func() error {
		if !(1+1 == 2) {
			return Errorf("This is correct")
		}
		return nil
	}
	assert(t, f() == nil)

	v := 1
	f = func() error {
		if !(v+v == 1) {
			return Errorf("This is not correct")
		}
		return nil
	}
	assert(t, f() != nil)

	f = func() error {
		if !(v+v == 1) {
			return EnsureFailedDebug("v + v == 1", v+v, 1)
		}
		return nil
	}
	assertEq(t, f().Error(), "Condition failed: `v + v == 1` (2 vs 1)")
}

func TestItTestMacros_test_ensure_fmt(t *testing.T) {
	requireFormatter(t)
	v := 1
	f := func() error {
		if !(v+v == 2) {
			return Errorf("This is correct, v: {}", v)
		}
		return nil
	}
	assert(t, f() == nil)

	f = func() error {
		if !(v+v == 1) {
			return Errorf("This is not correct, v: {}", v)
		}
		return nil
	}
	assertEq(t, f().Error(), "This is not correct, v: 1")
}

func TestItTestMacros_test_brace_escape(t *testing.T) {
	requireFormatter(t)
	err := Errorf("unterminated ${{..}} expression")
	assertEq(t, "unterminated ${..} expression", err.Error())
}

// ensure.rs: a condition that is not a comparison has only its text.
func TestEnsureFailedHasOnlyTheCondition(t *testing.T) {
	err := EnsureFailed("false == true && false")
	assertEq(t, err.Error(), "Condition failed: `false == true && false`")
	text, ok := DowncastRef[string](err)
	assert(t, ok)
	assertEq(t, text, err.Error())
}
