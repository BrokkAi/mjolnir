// Package unicodesegmentation stands in for the Rust crate
// unicode-segmentation 1.13.3: the boundaries of grapheme clusters and of
// words in a string, by the rules of Unicode Standard Annex #29.
//
// It is a port of the crate's source, so that the Go implementation of
// Mjolnir splits text where the Rust one does: the terminal UI (ratatui,
// reedline, and Mjolnir's own wrapping and cursor movement) lays out and
// moves over text a grapheme cluster at a time. The tables are the crate's
// own (Unicode 17.0.0), written into tables_data.go by gen.go from the
// crate's src/tables.rs; no other Unicode version and no other library's idea
// of a boundary is involved.
//
// # Names
//
// The crate's trait `UnicodeSegmentation` is implemented for `str`, which Go
// cannot give methods, so each method is a function taking the string, under
// the method's name and under the trait's and the method's:
//
//	s.graphemes(is_extended)         Graphemes(s, isExtended)         UnicodeSegmentationGraphemes
//	s.grapheme_indices(is_extended)  GraphemeIndices(s, isExtended)   UnicodeSegmentationGraphemeIndices
//	s.unicode_words()                UnicodeWords(s)                  UnicodeSegmentationUnicodeWords
//	s.unicode_word_indices()         UnicodeWordIndices(s)            UnicodeSegmentationUnicodeWordIndices
//	s.split_word_bounds()            SplitWordBounds(s)               UnicodeSegmentationSplitWordBounds
//	s.split_word_bound_indices()     SplitWordBoundIndices(s)         UnicodeSegmentationSplitWordBoundIndices
//
// The short names being the methods', the iterator types the functions return
// carry the name of the crate module that defines them:
//
//	Graphemes           GraphemeGraphemes
//	GraphemeIndices     GraphemeGraphemeIndices
//	UnicodeWords        WordUnicodeWords
//	UnicodeWordIndices  WordUnicodeWordIndices
//	UWordBounds         WordUWordBounds
//	UWordBoundIndices   WordUWordBoundIndices
//
// Each has Rust's Next, NextBack (`DoubleEndedIterator`) and SizeHint, AsStr
// where the crate has `as_str`, and Clone where the crate derives `Clone`.
// The iterators with indices yield an `rs.Tuple2[int, string]`, the byte
// offset and the piece. All and Rev (range.go) make every iterator usable in
// a range loop: All takes from the front and Rev, Rust's `.rev()`, from the
// back; both consume the iterator they are called on.
//
// GraphemeCursor is the crate's cursor over text held in chunks, as a rope
// holds it: GraphemeCursorNew, SetCursor, CurCursor, ProvideContext,
// IsBoundary, NextBoundary and PrevBoundary. A query returns `(bool, error)`
// or `(rs.Option[int], error)` for Rust's `Result<bool, GraphemeIncomplete>`
// and `Result<Option<usize>, GraphemeIncomplete>`. The error is a
// GraphemeIncomplete, one of *GraphemeIncomplete_PreContext,
// *GraphemeIncomplete_PrevChunk, *GraphemeIncomplete_NextChunk and
// *GraphemeIncomplete_InvalidOffset; the crate gives the type no `Display`,
// and Error returns its `Debug` form (`PreContext(4)`, `NextChunk`).
//
// UnicodeVersion is `UNICODE_VERSION`.
//
// # Covered
//
// All of src/grapheme.rs and src/word.rs, src/lib.rs apart from the sentence
// methods, and the parts of src/tables.rs those read: the grapheme, word and
// emoji categories, InCB=Extend and InCB=Linker, Alphabetic and
// General_Category=N. As in the Rust build, `unicode_words` takes Alphabetic
// and Numeric from the standard library's tables (go/rs/strs), because the
// toolchain's Unicode version is the crate's; the crate's own tables give the
// same answers (TestStdAndCrateTablesAgree). Panics carry Rust's messages:
// slicing a chunk off a char boundary, unwrapping a missing character or
// context, the assertion in `provide_context`.
//
// # Not covered
//
// Sentence boundaries: the methods `unicode_sentences`,
// `split_sentence_bounds` and `split_sentence_bound_indices`, the types
// `UnicodeSentences`, `USentenceBounds` and `USentenceBoundIndices`,
// src/sentence.rs and the sentence tables. Nothing Mjolnir is built from
// calls them: Mjolnir, ratatui, reedline, unicode-truncate, tokenizers and the
// other crates that depend on this one use grapheme clusters, word bounds and
// the cursor. With them go the crate's test `test_sentences`, its data
// `TEST_SENTENCE` and their documentation examples. The crate's `no_std`
// feature changes nothing a caller sees.
//
// # Where Go differs from Rust
//
// A Rust `str` is valid UTF-8; a Go string need not be. Each byte of a string
// that is not part of a valid encoding is read as a U+FFFD of its own, one
// byte long, as a Go range loop reads it: the pieces hold the characters that
// the string with those bytes replaced by U+FFFD gives, and are slices of the
// string given. The offset after such a byte is a char boundary (Rust's rule,
// a byte that is not a UTF-8 continuation byte, cannot apply to it); on valid
// UTF-8 the boundaries, and the panics at offsets that are not boundaries,
// are Rust's.
//
// Rust finds the offset of a piece by subtracting the address of the string
// from the address of the piece; the iterators here count bytes instead, with
// the same results. The derived `Debug` of the iterators holds those
// addresses in Rust and is not reproduced.
//
// # Tests
//
// it_test_test.go is the crate's tests/test.rs, with Unicode's conformance
// cases (GraphemeBreakTest.txt and WordBreakTest.txt, as the crate's
// tests/testdata/mod.rs holds them) in it_testdata_test.go, which gen.go
// writes. grapheme_test.go, word_test.go and lib_test.go are the tests and
// the documentation examples of those source files. The crate's quickcheck
// and proptest properties run on strings drawn from a fixed seed with the
// same distributions. utf8_test.go checks the reading of strings that are not
// valid UTF-8, described above.
//
// oracle_test.go compares with the crate itself through go/port/oracle
// (module `unicode`):
//
//   - for every scalar value, the boundaries in a string of probes built
//     around it. The probes tell apart every combination of the properties
//     the algorithms read of a character (TestGraphemeProbesTellPropertiesApart,
//     TestWordProbesTellPropertiesApart), so agreement on every scalar value
//     means the tables are the crate's;
//   - graphemes (extended and legacy), word bounds and Unicode words,
//     forwards, backwards and from both ends in turn, with and without
//     offsets, over the corpus of corpus_test.go: fixed cases, 20,000
//     generated strings biased to the sequences the rules single out, and
//     long inputs, with 180,000 more strings under MJ_ORACLE=live;
//   - GraphemeCursor driven over strings in chunks, as a rope drives it and
//     in ways it should not be (1,500 scripts, 30,000 more under
//     MJ_ORACLE=live): every answer, every request for another chunk or for
//     context, and every panic.
package unicodesegmentation

//go:generate go run gen.go
