// Package unicodewidth stands in for the Rust crate unicode-width 0.2.2, with
// its default `cjk` feature on: the displayed width, in terminal columns, of a
// character and of a string.
//
// It is a port of the crate's source, so that the Go implementation of
// Mjolnir puts every character in the same terminal cell as the Rust one. The
// lookup tables are the crate's own (Unicode 17.0.0), written into
// tables_data.go by gen.go from the crate's src/tables.rs; no other Unicode
// version and no other library's idea of width is involved.
//
// # Names
//
// The crate's two traits are implemented for `char` and `str`, which Go
// cannot give methods, so each method is a function named after the trait and
// the method:
//
//	UnicodeWidthChar::width(c)      UnicodeWidthCharWidth(c) rs.Option[int]
//	UnicodeWidthChar::width_cjk(c)  UnicodeWidthCharWidthCjk(c) rs.Option[int]
//	UnicodeWidthStr::width(s)       UnicodeWidthStrWidth(s) int
//	UnicodeWidthStr::width_cjk(s)   UnicodeWidthStrWidthCjk(s) int
//
// with the short names CharWidth, CharWidthCjk, StrWidth and StrWidthCjk for
// the same four functions. The traits themselves are the interfaces
// [UnicodeWidthChar] and [UnicodeWidthStr], for the types of other crates that
// implement them (ratatui's Span, Line, Text and Tabs implement
// UnicodeWidthStr). [UnicodeVersion] is `UNICODE_VERSION`.
//
// # Covered
//
// All of the crate: both character widths, both string widths with the whole
// string state machine of src/tables.rs (emoji presentation and text
// presentation selectors, ZWJ sequences, flags, tag sequences, keycaps, emoji
// modifiers, the Arabic, Buginese, Hebrew, Khmer, Kirat Rai, Lisu, Old Turkic
// and Tifinagh ligatures, the solidus overlay in a CJK context, CRLF).
//
// # Not covered
//
// The build of the crate without the `cjk` feature (smaller tables, sealed
// traits); Mjolnir does not build it.
//
// # Where Go differs from Rust
//
// A Rust `char` is a Unicode scalar value and a Rust `str` is valid UTF-8; a
// Go rune and a Go string need not be. A rune that is not a scalar value, and
// each byte of a string that is not part of a valid encoding, is measured as
// U+FFFD (width 1).
//
// # Tests
//
// it_tests_test.go is the crate's tests/tests.rs, tables_test.go the test
// module of src/tables.rs and lib_test.go the documentation examples of
// src/lib.rs; their data files are in testdata. utf8_test.go checks the
// measuring of runes and strings Rust cannot hold, described above.
// oracle_test.go compares with the crate itself through go/port/oracle
// (module `unicode`): the two character widths of every scalar value and the
// two string widths of the string holding only it; and the two string widths
// of the corpus of corpus_test.go (fixed cases, 20,000 generated strings
// biased to the sequences the rules single out, and long inputs), with
// 180,000 more strings under MJ_ORACLE=live.
//
// # The rules, from the crate's documentation
//
// What follows is the section "Rules for determining width" of the crate's
// documentation (src/lib.rs), as it is written there.
//
//	This crate currently uses the following rules to determine the width of a
//	character or string, in order of decreasing precedence. These may be tweaked in the future.
//
//	1. In the following cases, the width of a string differs from the sum of the widths of its constituent characters:
//	   - The sequence `"\r\n"` has width 1.
//	   - Emoji-specific ligatures:
//	     - Well-formed, fully-qualified [emoji ZWJ sequences] have width 2.
//	     - [Emoji modifier sequences] have width 2.
//	     - [Emoji presentation sequences] have width 2.
//	     - Outside of an East Asian context, [text presentation sequences] have width 1 if their base character:
//	       - Has the [`Emoji_Presentation`] property, and
//	       - Is not in the [Enclosed Ideographic Supplement] block.
//	   - [`'\u{2018}'`, `'\u{2019}'`, `'\u{201C}'`, and `'\u{201D}'`][General Punctuation] always have width 1
//	     when followed by '\u{FE00}' or '\u{FE02}', and width 2 when followed by '\u{FE01}'.
//	   - Script-specific ligatures:
//	     - For all the following ligatures, the insertion of any number of [default-ignorable][`Default_Ignorable_Code_Point`]
//	       [combining marks] anywhere in the sequence will not change the total width. In addition, for all non-Arabic
//	       ligatures, the insertion of any number of [`'\u{200D}'` ZERO WIDTH JOINER](https://www.unicode.org/versions/Unicode16.0.0/core-spec/chapter-23/#G23126)s
//	       will not affect the width.
//	     - **[Arabic]**: A character sequence consisting of one character with [`Joining_Group`]`=Lam`,
//	       followed by any number of characters with [`Joining_Type`]`=Transparent`, followed by one character
//	       with [`Joining_Group`]`=Alef`, has total width 1. For example: `لا`‎, `لآ`‎, `ڸا`‎, `لٟٞأ`
//	     - **[Buginese]**: `"\u{1A15}\u{1A17}\u{200D}\u{1A10}"` (<a, -i> ya, `ᨕᨗ‍ᨐ`) has total width 1.
//	     - **[Hebrew]**: `"א\u{200D}ל"` (Alef-Lamed, `א‍ל`) has total width 1.
//	     - **[Khmer]**: Coeng signs consisting of `'\u{17D2}'` followed by a character in
//	       `'\u{1780}'..='\u{1782}' | '\u{1784}'..='\u{1787}' | '\u{1789}'..='\u{178C}' | '\u{178E}'..='\u{1793}' | '\u{1795}'..='\u{1798}' | '\u{179B}'..='\u{179D}' | '\u{17A0}' | '\u{17A2}'  | '\u{17A7}' | '\u{17AB}'..='\u{17AC}' | '\u{17AF}'`
//	       have width 0.
//	     - **[Kirat Rai]**: Any sequence canonically equivalent to `'\u{16D68}'`, `'\u{16D69}'`, or `'\u{16D6A}'` has total width 1.
//	     - **[Lisu]**: Tone letter combinations consisting of a character in the range `'\u{A4F8}'..='\u{A4FB}'`
//	       followed by a character in the range `'\u{A4FC}'..='\u{A4FD}'` have width 1. For example: `ꓹꓼ`
//	     - **[Old Turkic]**: `"\u{10C32}\u{200D}\u{10C03}"` (`𐰲‍𐰃`) has total width 1.
//	     - **[Tifinagh]**: A sequence of a Tifinagh consonant in the range `'\u{2D31}'..='\u{2D65}' | '\u{2D6F}'`, followed by either
//	       [`'\u{2D7F}'` TIFINAGH CONSONANT JOINER] or `'\u{200D}'`, followed by another Tifinangh consonant, has total width 1.
//	       For example: `ⵏ⵿ⴾ`
//	   - In an East Asian context only, `<`, `=`, or `>` have width 2 when followed by [`'\u{0338}'` COMBINING LONG SOLIDUS OVERLAY].
//	     The two characters may be separated by any number of characters whose canonical decompositions consist only of characters meeting
//	     one of the following requirements:
//	     - Has [`Canonical_Combining_Class`] greater than 1, or
//	     - Is a [default-ignorable][`Default_Ignorable_Code_Point`] [combining mark][combining marks].
//	2. In all other cases, the width of the string equals the sum of its character widths:
//	   1. [`'\u{2D7F}'` TIFINAGH CONSONANT JOINER] has width 1 (outside of the ligatures described previously).
//	   2. [`'\u{115F}'` HANGUL CHOSEONG FILLER](https://util.unicode.org/UnicodeJsps/character.jsp?a=115F) and
//	      [`'\u{17A4}'` KHMER INDEPENDENT VOWEL QAA](https://util.unicode.org/UnicodeJsps/character.jsp?a=17A4) have width 2.
//	   3. [`'\u{17D8}'` KHMER SIGN BEYYAL](https://util.unicode.org/UnicodeJsps/character.jsp?a=17D8) has width 3.
//	   4. The following have width 0:
//	      - [Characters](https://util.unicode.org/UnicodeJsps/list-unicodeset.jsp?a=%5Cp%7BDefault_Ignorable_Code_Point%7D)
//	        with the [`Default_Ignorable_Code_Point`] property.
//	      - [Characters](https://util.unicode.org/UnicodeJsps/list-unicodeset.jsp?a=%5Cp%7BGrapheme_Extend%7D)
//	        with the [`Grapheme_Extend`] property.
//	      - [Characters](https://util.unicode.org/UnicodeJsps/list-unicodeset.jsp?a=%5Cp%7BHangul_Syllable_Type%3DV%7D%5Cp%7BHangul_Syllable_Type%3DT%7D)
//	        with a [`Hangul_Syllable_Type`] of `Vowel_Jamo` (`V`) or `Trailing_Jamo` (`T`).
//	      - The following [`Prepended_Concatenation_Mark`]s:
//	        - [`'\u{0605}'` NUMBER MARK ABOVE](https://util.unicode.org/UnicodeJsps/character.jsp?a=0605),
//	        - [`'\u{070F}'` SYRIAC ABBREVIATION MARK](https://util.unicode.org/UnicodeJsps/character.jsp?a=070F),
//	        - [`'\u{0890}'` POUND MARK ABOVE](https://util.unicode.org/UnicodeJsps/character.jsp?a=0890),
//	        - [`'\u{0891}'` PIASTRE MARK ABOVE](https://util.unicode.org/UnicodeJsps/character.jsp?a=0891), and
//	        - [`'\u{08E2}'` DISPUTED END OF AYAH](https://util.unicode.org/UnicodeJsps/character.jsp?a=08E2).
//	      - [Characters](https://util.unicode.org/UnicodeJsps/list-unicodeset.jsp?a=%5Cp%7BGrapheme_Cluster_Break%3DPrepend%7D-%5Cp%7BPrepended_Concatenation_Mark%7D)
//	        with the [`Grapheme_Extend=Prepend`] property, that are not also [`Prepended_Concatenation_Mark`]s.
//	      - [`'\u{A8FA}'` DEVANAGARI CARET](https://util.unicode.org/UnicodeJsps/character.jsp?a=A8FA).
//	   5. [Characters](https://util.unicode.org/UnicodeJsps/list-unicodeset.jsp?a=%5Cp%7BEast_Asian_Width%3DF%7D%5Cp%7BEast_Asian_Width%3DW%7D)
//	      with an [`East_Asian_Width`] of [`Fullwidth`] or [`Wide`] have width 2.
//	   6. Characters fulfilling all of the following conditions have width 2 in an East Asian context, and width 1 otherwise:
//	      - Fulfills one of the following conditions:
//	        - Has an [`East_Asian_Width`] of [`Ambiguous`], or
//	        - Has a [`Line_Break`] of [`AI`], or
//	        - Has a canonical decomposition to an [`Ambiguous`] character followed by [`'\u{0338}'` COMBINING LONG SOLIDUS OVERLAY], or
//	        - Is [`'\u{0387}'` GREEK ANO TELEIA](https://util.unicode.org/UnicodeJsps/character.jsp?a=0387); and
//	      - Does not have a [`General_Category`] of `Letter` or `Modifier_Symbol`.
//	   7. All other characters have width 1.
//
//	[`'\u{0338}'` COMBINING LONG SOLIDUS OVERLAY]: https://util.unicode.org/UnicodeJsps/character.jsp?a=0338
//	[`'\u{2D7F}'` TIFINAGH CONSONANT JOINER]: https://util.unicode.org/UnicodeJsps/character.jsp?a=2D7F
//
//	[`Canonical_Combining_Class`]: https://www.unicode.org/versions/Unicode16.0.0/core-spec/chapter-3/#G50313
//	[`Default_Ignorable_Code_Point`]: https://www.unicode.org/versions/Unicode16.0.0/core-spec/chapter-5/#G40095
//	[`East_Asian_Width`]: https://www.unicode.org/reports/tr11/#ED1
//	[`Emoji_Presentation`]: https://unicode.org/reports/tr51/#def_emoji_presentation
//	[`General_Category`]: https://www.unicode.org/versions/Unicode16.0.0/core-spec/chapter-4/#G124142
//	[`Grapheme_Extend=Prepend`]: https://www.unicode.org/reports/tr29/#Prepend
//	[`Grapheme_Extend`]: https://www.unicode.org/versions/Unicode16.0.0/core-spec/chapter-3/#G52443
//	[`Hangul_Syllable_Type`]: https://www.unicode.org/versions/Unicode16.0.0/core-spec/chapter-3/#G45593
//	[`Joining_Group`]: https://www.unicode.org/versions/Unicode14.0.0/ch09.pdf#G36862
//	[`Joining_Type`]: http://www.unicode.org/versions/Unicode16.0.0/core-spec/chapter-9/#G50009
//	[`Line_Break`]: https://www.unicode.org/reports/tr14/#LD5
//	[`Prepended_Concatenation_Mark`]: https://www.unicode.org/versions/Unicode16.0.0/core-spec/chapter-23/#G37908
//	[`Script`]: https://www.unicode.org/reports/tr24/#Script
//
//	[`Fullwidth`]: https://www.unicode.org/reports/tr11/#ED2
//	[`Wide`]: https://www.unicode.org/reports/tr11/#ED4
//	[`Ambiguous`]: https://www.unicode.org/reports/tr11/#ED6
//
//	[`AI`]: https://www.unicode.org/reports/tr14/#AI
//
//	[combining marks]: https://www.unicode.org/versions/Unicode16.0.0/core-spec/chapter-3/#G30602
//
//	[emoji ZWJ sequences]: https://www.unicode.org/reports/tr51/#def_emoji_sequence
//	[Emoji modifier sequences]: https://www.unicode.org/reports/tr51/#def_emoji_modifier_sequence
//	[Emoji presentation sequences]: https://unicode.org/reports/tr51/#def_emoji_presentation_sequence
//	[text presentation sequences]: https://unicode.org/reports/tr51/#def_text_presentation_sequence
//
//	[General Punctuation]: https://www.unicode.org/charts/PDF/Unicode-16.0/U160-2000.pdf
//	[Enclosed Ideographic Supplement]: https://unicode.org/charts/nameslist/n_1F200.html
//
//	[Arabic]: https://www.unicode.org/versions/Unicode16.0.0/core-spec/chapter-9/#G7480
//	[Buginese]: https://www.unicode.org/versions/Unicode16.0.0/core-spec/chapter-17/#G26743
//	[Hebrew]: https://www.unicode.org/versions/Unicode16.0.0/core-spec/chapter-9/#G6528
//	[Khmer]: https://www.unicode.org/versions/Unicode16.0.0/core-spec/chapter-16/#G64642
//	[Kirat Rai]: https://www.unicode.org/versions/Unicode16.0.0/core-spec/chapter-13/#G746409
//	[Lisu]: https://www.unicode.org/versions/Unicode16.0.0/core-spec/chapter-18/#G44587
//	[Old Turkic]: https://www.unicode.org/versions/Unicode16.0.0/core-spec/chapter-14/#G41975
//	[Tifinagh]: http://www.unicode.org/versions/Unicode16.0.0/core-spec/chapter-19/#G43184
//
//
//	## Canonical equivalence
//
//	Canonically equivalent strings are assigned the same width (CJK and non-CJK).
package unicodewidth

//go:generate go run gen.go
