# `go/crates/pulldowncmark` for pulldown-cmark 0.10.3 (`default-features = false`)

Oracle module `go/port/oracle/src/pulldown_cmark_oracle.rs`, feature
`pulldown_cmark`.

Mjolnir renders agent messages in the dashboard and web viewer by walking
pulldown-cmark's event stream (`mj-chat`), with golden screen tests, so the Go
port must produce the identical event stream for any input. This is a port of
this crate version's parser, not an adapter over a Go Markdown library.

Port `firstpass.rs`, `parse.rs`, `scanners.rs`, `linklabel.rs`, `tree.rs`,
`strings.rs`, `entities.rs`, `puncttable.rs` and the `lib.rs` types (`Event`,
`Tag`, `TagEnd`, `CodeBlockKind`, `HeadingLevel`, `LinkType`, `Alignment`,
`Options`, `MetadataBlockKind`, `BlockQuoteKind` if present), `Parser::new`,
`new_ext`, `new_with_broken_link_callback` if used, `into_offset_iter`,
`reference_definitions`, and `TextMergeStream` only if used. Cover every
`Options` flag of this version. Port `html.rs` too, so the spec tests run as
they do in Rust.

API: `pulldowncmark.ParserNewExt(text, options)` returning an iterator of
events, `Event` and `Tag`/`TagEnd` as enums per the translation rules, and
`Options` as a bit set (document the constant spelling).

Tests: the crate's suite, including the CommonMark and extension spec examples
in `tests/suite/` driven through `push_html`, and differential tests on the
event stream with offsets: the spec inputs plus at least 10,000 generated and
mutated documents (nested lists and quotes, lazy continuation, setext vs
thematic breaks, reference definitions and broken references, emphasis runs,
code spans and fences, HTML blocks of each kind, tables with escaped pipes,
footnotes, task lists, entities and escapes, tabs, CRLF, Unicode punctuation,
long and deeply nested input), for each option set Mjolnir uses plus all
options on.
