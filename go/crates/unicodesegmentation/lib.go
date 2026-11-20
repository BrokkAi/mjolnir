// Port of unicode-segmentation 1.13.3 src/lib.rs

// Copyright 2012-2015 The Rust Project Developers. See the COPYRIGHT
// file at the top-level directory of this distribution and at
// http://rust-lang.org/COPYRIGHT.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

package unicodesegmentation

// The crate's re-exports:
//
//	pub use grapheme::{GraphemeCursor, GraphemeIncomplete};   GraphemeCursor, GraphemeIncomplete
//	pub use grapheme::{GraphemeIndices, Graphemes};           GraphemeGraphemeIndices, GraphemeGraphemes
//	pub use sentence::{..};                                   not ported, see doc.go
//	pub use tables::UNICODE_VERSION;                          UnicodeVersion
//	pub use word::{UWordBoundIndices, UWordBounds,            WordUWordBoundIndices, WordUWordBounds,
//	               UnicodeWordIndices, UnicodeWords};         WordUnicodeWordIndices, WordUnicodeWords

// The trait `UnicodeSegmentation` (methods for segmenting strings according to
// [Unicode Standard Annex #29](http://www.unicode.org/reports/tr29/)) is
// implemented for `str` alone, which Go cannot give methods. Each of its
// methods is a function here, under the name of the method (Graphemes) and
// under the name of the trait and the method (UnicodeSegmentationGraphemes).

// Graphemes is `UnicodeSegmentation::graphemes` for `str`:
// returns an iterator over the
// [grapheme clusters](http://www.unicode.org/reports/tr29/#Grapheme_Cluster_Boundaries) of `self`.
//
// If `is_extended` is true, the iterator is over the
// *extended grapheme clusters*;
// otherwise, the iterator is over the *legacy grapheme clusters*.
// [UAX#29](http://www.unicode.org/reports/tr29/#Grapheme_Cluster_Boundaries)
// recommends extended grapheme cluster boundaries for general processing.
//
// # Examples
//
//	gr1 := slices.Collect(unicodesegmentation.Graphemes("a̐éö̲", true).All())
//	// "a̐", "é", "ö̲"
//
//	gr2 := slices.Collect(unicodesegmentation.Graphemes("a\r\nb🇷🇺🇸🇹", true).All())
//	// "a", "\r\n", "b", "🇷🇺", "🇸🇹"
func Graphemes(s string, isExtended bool) *GraphemeGraphemes {
	return graphemeNewGraphemes(s, isExtended)
}

// GraphemeIndices is `UnicodeSegmentation::grapheme_indices` for `str`:
// returns an iterator over the grapheme clusters of `self` and their
// byte offsets. See `graphemes()` for more information.
//
// # Examples
//
//	for offset, grapheme := range unicodesegmentation.GraphemeIndices("a̐éö̲\r\n", true).All() {
//		// (0, "a̐"), (3, "é"), (6, "ö̲"), (11, "\r\n")
//	}
func GraphemeIndices(s string, isExtended bool) *GraphemeGraphemeIndices {
	return graphemeNewGraphemeIndices(s, isExtended)
}

// UnicodeWords is `UnicodeSegmentation::unicode_words` for `str`:
// returns an iterator over the words of `self`, separated on
// [UAX#29 word boundaries](http://www.unicode.org/reports/tr29/#Word_Boundaries).
//
// Here, "words" are just those substrings which, after splitting on
// UAX#29 word boundaries, contain any alphanumeric characters. That is, the
// substring must contain at least one character with the
// [Alphabetic](http://unicode.org/reports/tr44/#Alphabetic)
// property, or with
// [General_Category=Number](http://unicode.org/reports/tr44/#General_Category_Values).
//
// # Example
//
//	uws := "The quick (\"brown\") fox can't jump 32.3 feet, right?"
//	uw1 := slices.Collect(unicodesegmentation.UnicodeWords(uws).All())
//	// "The", "quick", "brown", "fox", "can't", "jump", "32.3", "feet", "right"
func UnicodeWords(s string) *WordUnicodeWords {
	return wordNewUnicodeWords(s)
}

// UnicodeWordIndices is `UnicodeSegmentation::unicode_word_indices` for `str`:
// returns an iterator over the words of `self`, separated on
// [UAX#29 word boundaries](http://www.unicode.org/reports/tr29/#Word_Boundaries), and their
// offsets.
//
// Here, "words" are just those substrings which, after splitting on
// UAX#29 word boundaries, contain any alphanumeric characters. That is, the
// substring must contain at least one character with the
// [Alphabetic](http://unicode.org/reports/tr44/#Alphabetic)
// property, or with
// [General_Category=Number](http://unicode.org/reports/tr44/#General_Category_Values).
//
// # Example
//
//	uwis := "The quick (\"brown\") fox can't jump 32.3 feet, right?"
//	for offset, word := range unicodesegmentation.UnicodeWordIndices(uwis).All() {
//		// (0, "The"), (4, "quick"), (12, "brown"), (20, "fox"), (24, "can't"),
//		// (30, "jump"), (35, "32.3"), (40, "feet"), (46, "right")
//	}
func UnicodeWordIndices(s string) *WordUnicodeWordIndices {
	return wordNewUnicodeWordIndices(s)
}

// SplitWordBounds is `UnicodeSegmentation::split_word_bounds` for `str`:
// returns an iterator over substrings of `self` separated on
// [UAX#29 word boundaries](http://www.unicode.org/reports/tr29/#Word_Boundaries).
//
// The concatenation of the substrings returned by this function is just the original string.
//
// # Example
//
//	swu1 := slices.Collect(unicodesegmentation.SplitWordBounds("The quick (\"brown\")  fox").All())
//	// "The", " ", "quick", " ", "(", "\"", "brown", "\"", ")", "  ", "fox"
func SplitWordBounds(s string) *WordUWordBounds {
	return wordNewWordBounds(s)
}

// SplitWordBoundIndices is `UnicodeSegmentation::split_word_bound_indices`
// for `str`:
// returns an iterator over substrings of `self`, split on UAX#29 word boundaries,
// and their offsets. See `split_word_bounds()` for more information.
//
// # Example
//
//	for offset, piece := range unicodesegmentation.SplitWordBoundIndices("Brr, it's 29.3°F!").All() {
//		// (0, "Brr"), (3, ","), (4, " "), (5, "it's"), (9, " "), (10, "29.3"),
//		// (14, "°"), (16, "F"), (17, "!")
//	}
func SplitWordBoundIndices(s string) *WordUWordBoundIndices {
	return wordNewWordBoundIndices(s)
}

// `unicode_sentences`, `split_sentence_bounds` and
// `split_sentence_bound_indices` are not ported; see doc.go.

// UnicodeSegmentationGraphemes is [Graphemes] under the name the port's rule
// for a trait method gives it.
func UnicodeSegmentationGraphemes(s string, isExtended bool) *GraphemeGraphemes {
	return Graphemes(s, isExtended)
}

// UnicodeSegmentationGraphemeIndices is [GraphemeIndices] under the name the
// port's rule for a trait method gives it.
func UnicodeSegmentationGraphemeIndices(s string, isExtended bool) *GraphemeGraphemeIndices {
	return GraphemeIndices(s, isExtended)
}

// UnicodeSegmentationUnicodeWords is [UnicodeWords] under the name the port's
// rule for a trait method gives it.
func UnicodeSegmentationUnicodeWords(s string) *WordUnicodeWords {
	return UnicodeWords(s)
}

// UnicodeSegmentationUnicodeWordIndices is [UnicodeWordIndices] under the name
// the port's rule for a trait method gives it.
func UnicodeSegmentationUnicodeWordIndices(s string) *WordUnicodeWordIndices {
	return UnicodeWordIndices(s)
}

// UnicodeSegmentationSplitWordBounds is [SplitWordBounds] under the name the
// port's rule for a trait method gives it.
func UnicodeSegmentationSplitWordBounds(s string) *WordUWordBounds {
	return SplitWordBounds(s)
}

// UnicodeSegmentationSplitWordBoundIndices is [SplitWordBoundIndices] under
// the name the port's rule for a trait method gives it.
func UnicodeSegmentationSplitWordBoundIndices(s string) *WordUWordBoundIndices {
	return SplitWordBoundIndices(s)
}
