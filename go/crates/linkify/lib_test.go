// Port of linkify 0.11.0 src/lib.rs

package linkify_test

import (
	"testing"

	"github.com/BrokkAi/mjolnir/go/crates/linkify"
	"github.com/BrokkAi/mjolnir/go/rs"
)

// src/lib.rs has no `#[test]` functions. These are its documentation
// examples, which Rust runs as tests.

// Basic usage:
func TestLib_doc_basic_usage(t *testing.T) {
	input := "Have you seen http://example.com?"
	finder := linkify.LinkFinderNew()
	links := finder.Links(input).Collect()

	itCommonAssertEq(t, 1, len(links))
	link := links[0]

	itCommonAssertEq(t, "http://example.com", link.AsStr())
	itCommonAssertEq(t, 14, link.Start())
	itCommonAssertEq(t, 32, link.End())
	itCommonAssertEq(t, linkify.LinkKind_Url, link.Kind())
}

// Option to allow URLs without schemes:
func TestLib_doc_without_schemes(t *testing.T) {
	input := "Look, no scheme: example.org/foo"
	finder := linkify.LinkFinderNew()

	// true by default
	finder.UrlMustHaveScheme(false)

	links := finder.Links(input).Collect()
	itCommonAssertEq(t, links[0].AsStr(), "example.org/foo")
}

// Restrict the kinds of links:
func TestLib_doc_kinds(t *testing.T) {
	input := "http://example.com and foo@example.com"
	finder := linkify.LinkFinderNew()
	finder.Kinds([]linkify.LinkKind{linkify.LinkKind_Email})
	links := finder.Links(input).Collect()

	itCommonAssertEq(t, 1, len(links))
	link := links[0]
	itCommonAssertEq(t, "foo@example.com", link.AsStr())
	itCommonAssertEq(t, linkify.LinkKind_Email, link.Kind())
}

// Split the text into consecutive spans (mixed links and plain text).
func TestLib_doc_spans(t *testing.T) {
	input := "Have you seen http://example.com?"
	finder := linkify.LinkFinderNew()
	spans := finder.Spans(input).Collect()

	itCommonAssertEq(t, 3, len(spans))

	itCommonAssertEq(t, "Have you seen ", spans[0].AsStr())
	itCommonAssertEq(t, 0, spans[0].Start())
	itCommonAssertEq(t, 14, spans[0].End())
	itCommonAssertEq(t, rs.None[linkify.LinkKind](), spans[0].Kind())

	itCommonAssertEq(t, "http://example.com", spans[1].AsStr())
	itCommonAssertEq(t, 14, spans[1].Start())
	itCommonAssertEq(t, 32, spans[1].End())
	itCommonAssertEq(t, rs.Some(linkify.LinkKind_Url), spans[1].Kind())

	itCommonAssertEq(t, "?", spans[2].AsStr())
	itCommonAssertEq(t, 32, spans[2].Start())
	itCommonAssertEq(t, 33, spans[2].End())
	itCommonAssertEq(t, rs.None[linkify.LinkKind](), spans[2].Kind())
}

// The chain mj-chat writes (mj-chat/src/chat/rendering.rs): one finder,
// restricted to URLs, and the text split into spans.
func TestLib_mjolnir_use(t *testing.T) {
	finder := linkify.LinkFinderNew()
	finder.Kinds([]linkify.LinkKind{linkify.LinkKind_Url})
	shown := ""
	for part := range finder.Spans("mail a@b.co or see (https://example.com/a_(b)), done.").All() {
		if part.Kind().IsSome() {
			shown += "<" + part.AsStr() + ">"
		} else {
			shown += part.AsStr()
		}
	}
	itCommonAssertEq(t, shown, "mail a@b.co or see (<https://example.com/a_(b)>), done.")

	// LinkFinder::new().kinds(..).spans(..) in one expression, and
	// LinkFinder::default().
	first := linkify.LinkFinderDefault().Kinds([]linkify.LinkKind{linkify.LinkKind_Email}).Links("x a@b.co").Next().Unwrap()
	itCommonAssertEq(t, first.AsStr(), "a@b.co")
}
