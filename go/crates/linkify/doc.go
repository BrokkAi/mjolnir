// Package linkify stands in for the Rust crate `linkify` 0.11.0, which finds
// URLs and email addresses in plain text and takes care to end a link in the
// right place: before trailing punctuation, inside surrounding brackets and
// quotes, after a balanced closing parenthesis.
//
// Mjolnir uses it in one place, mj-chat/src/chat/rendering.rs, to underline
// the URLs written out in chat text and make them clickable:
//
//	finder := linkify.LinkFinderNew()
//	finder.Kinds([]linkify.LinkKind{linkify.LinkKind_Url})
//	for part := range finder.Spans(text).All() {
//		if part.Kind().IsSome() { ... part.AsStr() ... }
//	}
//
// Where a link starts and ends is visible on the terminal, so the package is
// a port of the crate's source: the same scan from each `:`, `.` and `@`, the
// same rules for schemes, hosts, IPv4 addresses, top-level domains, user
// info, ports, brackets and quotes, and the same treatment of non-ASCII
// characters with and without IRI parsing. The crate uses no Unicode tables;
// every decision is a comparison against character ranges, and the port
// makes the same comparisons.
//
// # Covered
//
// The whole crate:
//
//   - src/finder.rs: LinkFinder with all four options
//     (email_domain_must_have_dot, url_must_have_scheme, url_can_be_iri,
//     kinds), Link, LinkKind, Span, and the Links and Spans iterators.
//   - src/url.rs: the URL scanner (trigger `:`, needs `://`) and the plain
//     domain scanner (trigger `.`, used when a scheme is optional).
//   - src/domains.rs: the end of the authority part.
//   - src/email.rs: the email scanner (trigger `@`).
//
// # Names and shapes
//
//	linkify::LinkFinder::new()             linkify.LinkFinderNew()
//	finder.kinds(&[LinkKind::Url])         finder.Kinds([]linkify.LinkKind{linkify.LinkKind_Url})
//	finder.links(text), finder.spans(text) finder.Links(text), finder.Spans(text)
//	link.as_str(), span.kind()             link.AsStr(), span.Kind()
//
// Where the shapes differ from Rust's:
//
//   - LinkFinderNew and LinkFinderDefault return a pointer and the setters
//     return the finder they were called on, so a chain of calls reads as it
//     does in Rust.
//   - Links and Spans have Next, as the translation rules prescribe for an
//     iterator, and also All (a sequence to range over) and Collect. Like the
//     Rust iterators they scan only as far as they are asked to.
//   - Link and Span are read-only records with value receivers, so
//     `finder.Links(text).Next().Unwrap().AsStr()` can be written in one
//     expression as in Rust.
//   - `Link::kind` returns `&LinkKind` and `Span::kind` returns
//     `Option<&LinkKind>`; here they return LinkKind and rs.Option[LinkKind].
//   - Offsets are byte offsets into the text, as in Rust.
//
// A Rust `&str` is always valid UTF-8. A Go string need not be; a byte that
// is not part of a valid encoding is treated as one non-ASCII character, so
// the finder never panics and the spans always cover the text exactly.
//
// # Not covered
//
// Nothing of the crate's API is left out. The `Send` and `Sync` checks of
// tests/types.rs have no Go counterpart.
//
// # Tests
//
// The crate's four integration test files are ported (it_*_test.go), with
// the documentation examples of src/lib.rs. oracle_test.go compares links and
// spans with the real crate over a generated corpus: every scheme, separator,
// authority and path shape the scanners distinguish, one link in each of some
// seventy delimiter contexts (brackets, quotes, HTML attributes, Markdown,
// trailing punctuation, non-ASCII neighbours), email local parts, and
// unstructured text made of the characters the scanners branch on, under all
// combinations of finder options that change the scan.
package linkify
