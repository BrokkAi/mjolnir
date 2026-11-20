// Port of anyhow 1.0.102 tests/test_source.rs

package anyhow

import "testing"

// testError is `enum TestError { Io(io::Error) }`, whose Display is the io
// error's and whose source is the io error.
type testError struct {
	io *ioError
}

func (e *testError) Error() string { return e.io.Error() }

func (e *testError) Unwrap() error { return e.io }

func TestItTestSource_test_literal_source(t *testing.T) {
	err := Errorf("oh no!")
	assert(t, Source(err).IsNone())
}

func TestItTestSource_test_variable_source(t *testing.T) {
	msg := "oh no!"
	err := Msg(msg)
	assert(t, Source(err).IsNone())
}

func TestItTestSource_test_fmt_source(t *testing.T) {
	requireFormatter(t)
	err := Errorf("{} {}!", "oh", "no")
	assert(t, Source(err).IsNone())
}

func TestItTestSource_test_io_source(t *testing.T) {
	io := ioErrorNew("Other", "oh no!")
	err := New(&testError{io: io})
	assertEq(t, "oh no!", Source(err).Unwrap().Error())
}

func TestItTestSource_test_anyhow_from_anyhow(t *testing.T) {
	err := Context(Errorf("oh no!"), "context")
	err = New(err)
	assertEq(t, "oh no!", Source(err).Unwrap().Error())
}
