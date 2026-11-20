// Package pulldowncmark stands in for the Rust crate `pulldown-cmark` 0.10.3,
// a CommonMark pull parser, built as Mjolnir builds it: with
// `default-features = false`, so without the `html` and `getopts` features
// and without `simd` and `serde`.
//
// Mjolnir renders agent messages in the dashboard and the web viewer by
// walking the crate's event stream (mj-chat/src/chat/rendering.rs, with
// ENABLE_STRIKETHROUGH and ENABLE_TABLES), and golden screen tests compare
// the result. The package is therefore a port of the crate's parser, file by
// file, and not an adapter over a Go Markdown library: for any input and any
// option set it yields the events, the source ranges and the reference
// definitions the crate yields.
//
// # Names
//
// Names follow the path a Rust caller writes, and enums follow the
// translation rules (a data-free enum is a constant, any other enum a sealed
// interface with one pointer-to-struct variant each):
//
//	Parser::new(text)                         pulldowncmark.ParserNew(text)
//	Parser::new_ext(text, options)            pulldowncmark.ParserNewExt(text, options)
//	Parser::new_with_broken_link_callback     pulldowncmark.ParserNewWithBrokenLinkCallback
//	parser.into_offset_iter()                 parser.IntoOffsetIter()
//	parser.reference_definitions()            parser.ReferenceDefinitions()
//	Options::empty(), Options::all()          pulldowncmark.OptionsEmpty(), pulldowncmark.OptionsAll()
//	Options::ENABLE_TABLES                    pulldowncmark.OptionsEnableTables
//	options.insert(Options::ENABLE_TABLES)    options.Insert(pulldowncmark.OptionsEnableTables)
//	Event::Start(Tag::Heading { level, .. })  *pulldowncmark.Event_Start{F0: *pulldowncmark.Tag_Heading{Level: ..}}
//	Event::End(TagEnd::List(ordered))         *pulldowncmark.Event_End{F0: *pulldowncmark.TagEnd_List{F0: ordered}}
//	Event::Text(text)                         *pulldowncmark.Event_Text{F0: text}
//	CodeBlockKind::Fenced(info)               *pulldowncmark.CodeBlockKind_Fenced{F0: info}
//	HeadingLevel::H1, Alignment::Center       pulldowncmark.HeadingLevel_H1, pulldowncmark.Alignment_Center
//	LinkType::Reference                       pulldowncmark.LinkType_Reference
//	MetadataBlockKind::YamlStyle              pulldowncmark.MetadataBlockKind_YamlStyle
//	html::push_html(&mut s, parser)           pulldowncmark.HtmlPushHtml(&s, parser)
//
// Options is the crate's `bitflags!` set: a uint32 whose flag constants are
// spelled Options plus the constant's name in PascalCase
// (OptionsEnableTables, OptionsEnableFootnotes, OptionsEnableStrikethrough,
// OptionsEnableTasklists, OptionsEnableSmartPunctuation,
// OptionsEnableHeadingAttributes, OptionsEnableYamlStyleMetadataBlocks,
// OptionsEnablePlusesDelimitedMetadataBlocks, OptionsEnableOldFootnotes).
// The flags combine with Go's `|`, `&` and `&^` or with the methods bitflags
// generates (Insert, Remove, Toggle, Set, Contains, Intersects, Union,
// Intersection, Difference, SymmetricDifference, Complement, Bits, IsEmpty,
// IsAll, and OptionsFromBits, OptionsFromBitsTruncate, OptionsFromBitsRetain,
// OptionsFromName). Its Debug text is bitflags' (`Options(ENABLE_TABLES |
// ENABLE_STRIKETHROUGH)`). As in the crate, ENABLE_OLD_FOOTNOTES includes the
// ENABLE_FOOTNOTES bit.
//
// What mj-chat writes looks like this in Go:
//
//	options := pulldowncmark.OptionsEmpty()
//	options.Insert(pulldowncmark.OptionsEnableStrikethrough)
//	options.Insert(pulldowncmark.OptionsEnableTables)
//	for event := range pulldowncmark.ParserNewExt(source, options).All() {
//		switch event := event.(type) {
//		case *pulldowncmark.Event_Start:
//			switch tag := event.F0.(type) {
//			case *pulldowncmark.Tag_Heading: ... tag.Level ...
//			}
//		case *pulldowncmark.Event_Text: ... event.F0 ...
//		}
//	}
//
// # Covered
//
//   - src/lib.rs: Event, Tag (with ToEnd), TagEnd (and TagEndFromTag),
//     CodeBlockKind (IsIndented, IsFenced), HeadingLevel (Display, and
//     HeadingLevelTryFromUsize with InvalidHeadingLevel), LinkType,
//     Alignment, MetadataBlockKind and Options, with every flag of this
//     version.
//   - src/parse.rs: Parser (ParserNew, ParserNewExt,
//     ParserNewWithBrokenLinkCallback, ReferenceDefinitions, IntoOffsetIter,
//     Next, and its Debug), OffsetIter (Next, ReferenceDefinitions), RefDefs
//     (Get, Iter), LinkDef, BrokenLink (IntoStatic), the BrokenLinkCallback
//     trait with BrokenLinkCallbackFunc for closures and
//     DefaultBrokenLinkCallback; and the inline passes behind them.
//   - src/firstpass.rs (the block pass), src/scanners.rs, src/linklabel.rs,
//     src/tree.rs, src/entities.rs and src/puncttable.rs, unexported.
//   - src/html.rs: HtmlPushHtml and HtmlWriteHtml. Mjolnir builds the crate
//     without its `html` feature; the module is ported so that the crate's
//     specification tests run here as they do in Rust, through push_html.
//   - The parts of two dependencies the parser needs: pulldown-cmark-escape
//     0.10.1 (escape.go, the HTML escaping html.rs uses) and unicase 2.9.0
//     (unicase.go and unicase_map.go, the case folding that matches link and
//     footnote labels).
//
// # Differences in shape
//
//   - CowStr is a Go string throughout (strings.go explains why); InlineStr
//     and StringTooLongError, which only build one of CowStr's
//     representations, are not ported.
//   - `core::ops::Range<usize>` is this package's Range (range.go), with a
//     Debug text of `start..end`, because go/rs has no range type yet.
//   - Rust's Parser is generic over its callback type; here the callback is
//     the BrokenLinkCallback interface held in an rs.Option, and a function
//     becomes one as BrokenLinkCallbackFunc. A callback returns
//     rs.Option[rs.Tuple2[string, string]] (URL and title).
//   - Parser and OffsetIter have Next, as the translation rules prescribe for
//     an iterator, and All, a sequence to range over. HtmlPushHtml and
//     HtmlWriteHtml take anything with Next (HtmlEventIterator).
//   - HtmlPushHtml appends to a *string; HtmlWriteHtml writes to an
//     io.Writer and returns its error.
//   - RefDefs.Iter yields the definitions in the order the document defines
//     them. Rust iterates a HashMap there, in an order that changes from run
//     to run, so no caller can depend on it.
//   - `{:?}` of anything that holds a string (Event::Text, Tag::Link,
//     LinkDef, BrokenLink, ...) shows the string as Go's Debug of a string
//     does, `Text("a")`, where Rust's derived Debug shows which
//     representation its CowStr has, `Text(Borrowed("a"))`,
//     `Text(Boxed("a"))` or `Text(Inlined(InlineStr { .. }))`. Every other
//     Debug text is the crate's.
//
// A Rust `&str` is always valid UTF-8 and a Go string need not be. Where the
// parser looks at characters (flanking of delimiter runs, label folding), a
// byte that is not part of a valid encoding reads as U+FFFD; it stays in the
// text as it is, and the parser never panics because of it.
//
// # Not covered
//
//   - src/utils.rs (TextMergeStream, TextMergeWithOffset): Mjolnir does not
//     use them.
//   - src/main.rs, the command-line program of the `getopts` feature.
//   - The `simd` feature (firstpass.rs's SIMD scanner and its tests) and the
//     `serde` feature (tests/serde.rs).
//   - Tree's Debug impl, which only the crate's own debugging uses.
//
// # Behaviour of the crate that callers can trip over
//
// The port reproduces these, as it reproduces everything else:
//
//   - With ENABLE_YAML_STYLE_METADATA_BLOCKS or
//     ENABLE_PLUSES_DELIMITED_METADATA_BLOCKS, a block that starts with a
//     `---` or `+++` line indented by one to three columns, followed by a
//     line of text and later a closing line (` ---\na\n---\n`), makes the
//     block pass start the document over, forever: parse_metadata_block
//     returns 0 for an indented block, and FirstPass::run continues from the
//     offset it returns. Mjolnir enables neither option.
//   - With ENABLE_HEADING_ATTRIBUTES, a setext heading in a block quote whose
//     attribute block sits on its own line (`> a\n> {}\n> =\n`) truncates a
//     text node to end before it starts. A debug build fails the debug
//     assertion in Tree::truncate_siblings; a release build panics with
//     "byte range starts at 6 but ends at 5" when it slices the text.
//   - The three-character case folds of unicase come out rotated
//     (unicase_map.go): a reference definition `[ﬃ]: /u` is found by the
//     link `[iff]` and not by `[ffi]`.
//
// # Tests
//
// The crate's tests are ported: the specification and extension examples of
// tests/suite (it_lib_suite_*_test.go, generated by testdata/port_suite.py
// and driven through HtmlPushHtml as test_markdown_html drives push_html),
// tests/html.rs, tests/errors.rs, and the unit tests of parse.rs,
// linklabel.rs, puncttable.rs and scanners.rs. The tests of strings.rs
// (CowStr's representations) and parse.rs's node_size and body_size (Rust's
// memory layout) are skipped with the reason.
//
// oracle_test.go compares the event stream with offsets, the reference
// definitions and the broken link callback's calls with the real crate
// (go/port/oracle/src/pulldown_cmark_oracle.rs): every specification
// example, more than twelve thousand generated and mutated documents
// (corpus_test.go) under six option sets, every Unicode character up to
// U+1FFFF and samples above it next to every kind of delimiter, the case
// folding of every character, and the Debug text of the events that hold no
// string and of Options. The comparisons run with debug assertions off, as
// the oracle is a release build.
package pulldowncmark
