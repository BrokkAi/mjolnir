package anyhow

import (
	"errors"
	"testing"

	"github.com/BrokkAi/mjolnir/go/rs"
)

// What this package promises Go code beyond the crate's own behaviour.

func TestContextOfNilIsNilAndDoesNotBuildTheContext(t *testing.T) {
	assert(t, Context(nil, "context") == nil)
	called := false
	assert(t, WithContext(nil, func() string {
		called = true
		return "context"
	}) == nil)
	assert(t, !called)

	err := WithContext(Msg("root"), func() string {
		called = true
		return "context"
	})
	assert(t, called)
	assertEq(t, AlternateString(err), "context: root")
	assert(t, New(nil) == nil)
}

func TestOptionContextIsCalledOnlyForNone(t *testing.T) {
	value, err := OptionContext(rs.Some(7), "missing")
	assert(t, err == nil)
	assertEq(t, value, 7)

	called := false
	value, err = OptionWithContext(rs.Some(8), func() string {
		called = true
		return "missing"
	})
	assert(t, err == nil && !called)
	assertEq(t, value, 8)

	value, err = OptionContext(rs.None[int](), "missing")
	assertEq(t, value, 0)
	assertEq(t, DebugString(err), "missing")
	text, ok := DowncastRef[string](err)
	assert(t, ok)
	assertEq(t, text, "missing")
}

func TestNewKeepsAnAnyhowErrorAsItIs(t *testing.T) {
	err := Context(Msg("root"), "context")
	assert(t, New(err) == err)
	wrapped := New(ioErrorFromRawOsError(2))
	assert(t, New(wrapped) == wrapped)
}

// An error that never passed through New is still an anyhow error to the
// functions of this package: `?` converts without a call.
func TestABareErrorReadsAsTheAnyhowErrorItWouldBe(t *testing.T) {
	bare := newTyped([]string{"outer", "middle", "inner"})
	wrapped := New(bare)
	assertEq(t, DisplayString(bare), DisplayString(wrapped))
	assertEq(t, AlternateString(bare), "outer: middle: inner")
	assertEq(t, AlternateString(wrapped), "outer: middle: inner")
	assertEq(t, DebugString(bare), "outer\n\nCaused by:\n    0: middle\n    1: inner")
	assertEq(t, DebugString(wrapped), DebugString(bare))
	assertEq(t, len(Chain(bare)), 3)
	assert(t, Chain(wrapped)[0] == error(bare))
	assert(t, RootCause(bare) == RootCause(wrapped))
}

var errSentinel = errors.New("sentinel")

func TestErrorsIsAndAsSeeThroughContexts(t *testing.T) {
	io := ioErrorFromRawOsError(13)
	err := Context(Context(New(io), "inner context"), typedCtx{text: "outer context"})

	assert(t, errors.Is(err, io))
	var found *ioError
	assert(t, errors.As(err, &found))
	assert(t, found == io)

	// A context value that is an error is found in front of its cause, as
	// DowncastRef finds it.
	typedContext := newTyped([]string{"typed context"})
	err = Context(Context(New(errSentinel), typedContext), "outer")
	assert(t, errors.Is(err, errSentinel))
	var typed *typedError
	assert(t, errors.As(err, &typed))
	assert(t, typed == typedContext)

	// Without a context too.
	assert(t, errors.Is(New(errSentinel), errSentinel))
	assert(t, errors.Is(FromBoxed(errSentinel), errSentinel))
	assert(t, !errors.Is(Msg("sentinel"), errSentinel))
}

// A source that is an anyhow error stands for its error object: the chain a
// ported `source()` returning `Some(error.as_ref())` produces.
type holdsAnyhow struct {
	inner error
}

func (e *holdsAnyhow) Error() string { return "holder" }
func (e *holdsAnyhow) Unwrap() error { return AsRef(e.inner) }

type holdsAnyhowHandle struct {
	inner error
}

func (e *holdsAnyhowHandle) Error() string { return "holder" }
func (e *holdsAnyhowHandle) Unwrap() error { return e.inner }

func TestASourceThatIsAnAnyhowErrorContributesItsChain(t *testing.T) {
	io := ioErrorFromRawOsError(2)
	inner := Context(New(io), "inner context")
	for _, holder := range []error{&holdsAnyhow{inner: inner}, &holdsAnyhowHandle{inner: inner}} {
		err := Context(New(holder), "outer")
		assertEq(t, AlternateString(err), "outer: holder: inner context: No such file or directory (os error 2)")
		chain := Chain(err)
		assertEq(t, len(chain), 4)
		// The context layer under the holder is a `dyn Error`: nothing
		// downcasts from it, and the io error is found where it sits.
		assert(t, !Is[*ioError](chain[2]))
		assert(t, Is[*ioError](chain[3]))
		// The anyhow downcast stops at the holder: it does not look at an
		// error's sources.
		assert(t, !Is[*ioError](err))
	}
}

func TestRootCauseOfNoErrorPanicsAsUnwrapOnNone(t *testing.T) {
	defer func() {
		recovered := recover()
		assert(t, recovered != nil)
		assertEq(t, rs.PanicInfoOf(recovered).Message, "called `Option::unwrap()` on a `None` value")
	}()
	RootCause(nil)
}
