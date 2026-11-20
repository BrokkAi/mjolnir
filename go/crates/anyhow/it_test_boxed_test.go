// Port of anyhow 1.0.102 tests/test_boxed.rs

package anyhow

import "testing"

// myError is the thiserror type `#[error("outer")] struct MyError { source:
// io::Error }`.
type myError struct {
	source *ioError
}

func (e *myError) Error() string { return "outer" }

func (e *myError) Unwrap() error { return e.source }

func TestItTestBoxed_test_boxed_str(t *testing.T) {
	var boxed error = stringError("oh no!")
	err := FromBoxed(boxed)
	assertEq(t, "oh no!", err.Error())
	inner, ok := DowncastRef[error](err)
	assert(t, ok)
	assertEq(t, "oh no!", inner.Error())
	// The box is found; the type inside it is not.
	assert(t, !Is[stringError](err))
}

func TestItTestBoxed_test_boxed_thiserror(t *testing.T) {
	err := New(&myError{source: ioErrorNew("Other", "oh no!")})
	assertEq(t, "oh no!", Source(err).Unwrap().Error())
}

func TestItTestBoxed_test_boxed_anyhow(t *testing.T) {
	err := Context(Errorf("oh no!"), "it failed")
	err = New(err)
	assertEq(t, "oh no!", Source(err).Unwrap().Error())
}
