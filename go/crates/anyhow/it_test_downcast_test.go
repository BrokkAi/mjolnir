// Port of anyhow 1.0.102 tests/test_downcast.rs
//
// `&str` and `String` are both string in Go. test_downcast_mut is covered by
// test_downcast_ref: a Go value reached through DowncastRef with a pointer
// type can be changed through it. test_drop and test_large_alignment have no
// counterpart (nothing is dropped, and Go places the value).

package anyhow

import "testing"

func TestItTestDowncast_test_downcast(t *testing.T) {
	literal, err := Downcast[string](bailLiteral())
	assert(t, err == nil)
	assertEq(t, "oh no!", literal)

	io, err := Downcast[*ioError](bailError())
	assert(t, err == nil)
	assertEq(t, "oh no!", io.Error())
}

func TestItTestDowncast_test_downcast_fmt(t *testing.T) {
	requireFormatter(t)
	formatted, err := Downcast[string](bailFmt())
	assert(t, err == nil)
	assertEq(t, "oh no!", formatted)
}

func TestItTestDowncast_test_downcast_ref(t *testing.T) {
	literal, ok := DowncastRef[string](bailLiteral())
	assert(t, ok)
	assertEq(t, "oh no!", literal)

	io, ok := DowncastRef[*ioError](bailError())
	assert(t, ok)
	assertEq(t, "oh no!", io.Error())
}

func TestItTestDowncast_test_downcast_ref_fmt(t *testing.T) {
	requireFormatter(t)
	formatted, ok := DowncastRef[string](bailFmt())
	assert(t, ok)
	assertEq(t, "oh no!", formatted)
}

func TestItTestDowncast_test_downcast_mut(t *testing.T) {
	bailed := bailError()
	io, ok := DowncastRef[*ioError](bailed)
	assert(t, ok)
	io.text = "clobber"
	again, _ := DowncastRef[*ioError](bailed)
	assertEq(t, again.Error(), "clobber")
	assertEq(t, bailed.Error(), "clobber")
}

func TestItTestDowncast_test_as_ref(t *testing.T) {
	err := bailError()
	refDyn := AsRef(err)
	assertEq(t, "oh no!", refDyn.Error())
	_, isIo := refDyn.(*ioError)
	assert(t, isIo)
}

func TestItTestDowncast_test_unsuccessful_downcast(t *testing.T) {
	err := bailError()
	_, ok := DowncastRef[string](err)
	assert(t, !ok)
	_, failed := Downcast[string](err)
	assert(t, failed == err)
}

// The Go side of the downcast rule: a type argument names the Rust type
// whether the error holds the Go value or a pointer to it.

type copyError struct {
	found int64
}

func (e copyError) Error() string { return "copy error" }

func TestDowncastRefMatchesValueAndPointerSpellings(t *testing.T) {
	byValue := Context(New(copyError{found: 7}), "context")
	value, ok := DowncastRef[copyError](byValue)
	assert(t, ok)
	assertEq(t, value.found, int64(7))
	pointer, ok := DowncastRef[*copyError](byValue)
	assert(t, ok)
	assertEq(t, pointer.found, int64(7))

	byPointer := Context(New(&copyError{found: 8}), "context")
	value, ok = DowncastRef[copyError](byPointer)
	assert(t, ok)
	assertEq(t, value.found, int64(8))
	pointer, ok = DowncastRef[*copyError](byPointer)
	assert(t, ok)
	assertEq(t, pointer.found, int64(8))

	_, ok = DowncastRef[*lowLevel](byPointer)
	assert(t, !ok)
}

// An enum is an interface over its variants; downcasting to the enum finds
// whichever variant the error holds.

type sealedError interface {
	error
	isSealedError()
}

type sealedErrorTimeout struct{}

func (*sealedErrorTimeout) Error() string  { return "timed out" }
func (*sealedErrorTimeout) isSealedError() {}

func TestDowncastRefFindsAnEnumByItsInterface(t *testing.T) {
	err := Context(Context(New(&sealedErrorTimeout{}), "inner"), "outer")
	found, ok := DowncastRef[sealedError](err)
	assert(t, ok)
	_, isTimeout := found.(*sealedErrorTimeout)
	assert(t, isTimeout)

	// A context value is found the same way, in front of the error under it.
	err = Context(Msg("root"), &sealedErrorTimeout{})
	assert(t, Is[sealedError](err))
	assert(t, Is[string](err))
}

// What the crate documents under "Effect on downcasting" for the chain: the
// `dyn Error` downcast of an element sees only that element.
func TestDowncastRefOnChainElementsSeesOnlyTheElement(t *testing.T) {
	io := ioErrorFromRawOsError(2)
	err := Context(Context(New(io), typedCtx{text: "typed"}), "outer")
	assert(t, Is[*ioError](err))
	assert(t, Is[typedCtx](err))

	chain := Chain(err)
	assertEq(t, len(chain), 3)
	assert(t, !Is[*ioError](chain[0]))
	assert(t, !Is[typedCtx](chain[0]))
	assert(t, !Is[*ioError](chain[1]))
	assert(t, !Is[typedCtx](chain[1]))
	found, ok := DowncastRef[*ioError](chain[2])
	assert(t, ok)
	assert(t, found == io)
}
