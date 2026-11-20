// Port of linkify 0.11.0 tests/types.rs

package linkify_test

import (
	"reflect"
	"testing"

	"github.com/BrokkAi/mjolnir/go/crates/linkify"
	"github.com/BrokkAi/mjolnir/go/rs"
)

func TestItTypes_send_and_sync(t *testing.T) {
	// The Rust test asks the compiler whether LinkFinder and Link are Send
	// and Sync. Go has no such property to ask about: a LinkFinder holds five
	// booleans and a Link holds a string, two offsets and a kind, and reading
	// either from several goroutines is safe.
	finder := linkify.LinkFinderNew()
	link := finder.Links("http://example.org").Next().Unwrap()
	done := make(chan string)
	for range 2 {
		go func() { done <- finder.Links(link.AsStr()).Next().Unwrap().AsStr() }()
	}
	for range 2 {
		itCommonAssertEq(t, <-done, "http://example.org")
	}
}

func TestItTypes_equality(t *testing.T) {
	finder := linkify.LinkFinderNew()
	first := finder.Links("http://example.org").Next()
	rs.Assert(first.IsSome())

	link := first.Unwrap()
	// Check that link has Debug
	//
	// The Rust test prints the link with `{:?}`. Link's Debug is derived, so
	// in Go it is the formatter's doing, from the registration in finder.go;
	// what this package owes it is that registration.
	info := rs.TypeInfoOf(reflect.TypeFor[linkify.Link]())
	if info == nil || info.RustName != "Link" {
		t.Fatalf("Link is not registered for Debug: %+v", info)
	}

	itCommonAssertEq(t, link.Kind(), linkify.LinkKind_Url)
}
