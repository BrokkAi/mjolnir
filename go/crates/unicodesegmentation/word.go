// Port of unicode-segmentation 1.13.3 src/word.rs

// Copyright 2012-2014 The Rust Project Developers. See the COPYRIGHT
// file at the top-level directory of this distribution and at
// http://rust-lang.org/COPYRIGHT.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

package unicodesegmentation

import (
	"unicode/utf8"

	"github.com/BrokkAi/mjolnir/go/rs"
	"github.com/BrokkAi/mjolnir/go/rs/strs"
)

// WordUnicodeWords is the crate's `UnicodeWords`:
// an iterator over the substrings of a string which, after splitting the string on
// [word boundaries](http://www.unicode.org/reports/tr29/#Word_Boundaries),
// contain any characters with the
// [Alphabetic](http://unicode.org/reports/tr44/#Alphabetic)
// property, or with
// [General_Category=Number](http://unicode.org/reports/tr44/#General_Category_Values).
//
// This struct is created by [UnicodeWords], the `unicode_words` method on the
// `UnicodeSegmentation` trait. See its documentation for more.
type WordUnicodeWords struct {
	inner wordWordsIter `rs:"inner"`
}

// Next is `Iterator::next`: the next word from the front.
func (s *WordUnicodeWords) Next() rs.Option[string] {
	switch i := s.inner.(type) {
	case *wordWordsIter_Ascii:
		return i.F0.Next()
	case *wordWordsIter_Unicode:
		return i.F0.Next()
	default:
		panic(rs.Unreachable())
	}
}

// SizeHint is `Iterator::size_hint`.
func (s *WordUnicodeWords) SizeHint() (int, rs.Option[int]) {
	switch i := s.inner.(type) {
	case *wordWordsIter_Ascii:
		return i.F0.SizeHint()
	case *wordWordsIter_Unicode:
		return i.F0.SizeHint()
	default:
		panic(rs.Unreachable())
	}
}

// NextBack is `DoubleEndedIterator::next_back`: the next word from the back.
func (s *WordUnicodeWords) NextBack() rs.Option[string] {
	switch i := s.inner.(type) {
	case *wordWordsIter_Ascii:
		return i.F0.NextBack()
	case *wordWordsIter_Unicode:
		return i.F0.NextBack()
	default:
		panic(rs.Unreachable())
	}
}

// WordUnicodeWordIndices is the crate's `UnicodeWordIndices`:
// an iterator over the substrings of a string which, after splitting the string on
// [word boundaries](http://www.unicode.org/reports/tr29/#Word_Boundaries),
// contain any characters with the
// [Alphabetic](http://unicode.org/reports/tr44/#Alphabetic)
// property, or with
// [General_Category=Number](http://unicode.org/reports/tr44/#General_Category_Values).
// This iterator also provides the byte offsets for each substring.
//
// This struct is created by [UnicodeWordIndices], the `unicode_word_indices`
// method on the `UnicodeSegmentation` trait. See its documentation for more.
type WordUnicodeWordIndices struct {
	inner wordIndicesIter `rs:"inner"`
}

// Next is `Iterator::next`: the next word from the front, with its byte
// offset in the string.
func (s *WordUnicodeWordIndices) Next() rs.Option[rs.Tuple2[int, string]] {
	switch i := s.inner.(type) {
	case *wordIndicesIter_Ascii:
		return i.F0.Next()
	case *wordIndicesIter_Unicode:
		return i.F0.Next()
	default:
		panic(rs.Unreachable())
	}
}

// SizeHint is `Iterator::size_hint`.
func (s *WordUnicodeWordIndices) SizeHint() (int, rs.Option[int]) {
	switch i := s.inner.(type) {
	case *wordIndicesIter_Ascii:
		return i.F0.SizeHint()
	case *wordIndicesIter_Unicode:
		return i.F0.SizeHint()
	default:
		panic(rs.Unreachable())
	}
}

// NextBack is `DoubleEndedIterator::next_back`: the next word from the back,
// with its byte offset in the string.
func (s *WordUnicodeWordIndices) NextBack() rs.Option[rs.Tuple2[int, string]] {
	switch i := s.inner.(type) {
	case *wordIndicesIter_Ascii:
		return i.F0.NextBack()
	case *wordIndicesIter_Unicode:
		return i.F0.NextBack()
	default:
		panic(rs.Unreachable())
	}
}

// WordUWordBounds is the crate's `UWordBounds`:
// external iterator for a string's
// [word boundaries](http://www.unicode.org/reports/tr29/#Word_Boundaries).
//
// This struct is created by [SplitWordBounds], the `split_word_bounds` method
// on the `UnicodeSegmentation` trait. See its documentation for more.
type WordUWordBounds struct {
	string_ string                       `rs:"string"`
	cat     rs.Option[tablesWordWordCat] `rs:"cat"`
	catb    rs.Option[tablesWordWordCat] `rs:"catb"`
}

var _ = rs.RegisterStruct[WordUWordBounds]("UWordBounds")

// Clone is the derived `Clone`.
func (s *WordUWordBounds) Clone() WordUWordBounds {
	return *s
}

// WordUWordBoundIndices is the crate's `UWordBoundIndices`:
// external iterator for word boundaries and byte offsets.
//
// This struct is created by [SplitWordBoundIndices], the
// `split_word_bound_indices` method on the `UnicodeSegmentation` trait. See
// its documentation for more.
//
// Rust keeps the address of the string here (`start_offset`) and finds the
// offset of each piece by subtracting it from the piece's address. The
// address of a Go string is not a stable number, so this iterator counts the
// bytes taken from the front instead: a piece from the front starts there,
// and a piece from the back starts that far plus what the inner iterator has
// left.
type WordUWordBoundIndices struct {
	front int
	iter  WordUWordBounds `rs:"iter"`
}

// AsStr is `UWordBoundIndices::as_str`:
// view the underlying data (the part yet to be iterated) as a slice of the original string.
//
//	iter := unicodesegmentation.SplitWordBoundIndices("Hello world")
//	iter.AsStr() // "Hello world"
//	iter.Next()
//	iter.AsStr() // " world"
//	iter.Next()
//	iter.AsStr() // "world"
func (s *WordUWordBoundIndices) AsStr() string {
	return s.iter.AsStr()
}

// Clone is the derived `Clone`.
func (s *WordUWordBoundIndices) Clone() WordUWordBoundIndices {
	return *s
}

// Next is `Iterator::next`: the next piece from the front, with its byte
// offset in the string.
func (s *WordUWordBoundIndices) Next() rs.Option[rs.Tuple2[int, string]] {
	piece, ok := s.iter.Next().Get()
	if !ok {
		return rs.None[rs.Tuple2[int, string]]()
	}
	start := s.front
	s.front += len(piece)
	return rs.Some(rs.T2(start, piece))
}

// SizeHint is `Iterator::size_hint`.
func (s *WordUWordBoundIndices) SizeHint() (int, rs.Option[int]) {
	return s.iter.SizeHint()
}

// NextBack is `DoubleEndedIterator::next_back`: the next piece from the back,
// with its byte offset in the string.
func (s *WordUWordBoundIndices) NextBack() rs.Option[rs.Tuple2[int, string]] {
	piece, ok := s.iter.NextBack().Get()
	if !ok {
		return rs.None[rs.Tuple2[int, string]]()
	}
	return rs.Some(rs.T2(s.front+len(s.iter.string_), piece))
}

// wordUWordBoundsState is the crate's `UWordBoundsState`:
// state machine for word boundary rules.
//
// It is a `Copy` enum that the iterators compare with `==` at every
// character, so it is held as a value that `==` compares: a tag, and the
// field of each of the two variants that have one (zero in every other
// variant).
type wordUWordBoundsState struct {
	tag          wordUWordBoundsStateTag
	regional     wordRegionalState
	formatExtend wordFormatExtendType
}

type wordUWordBoundsStateTag uint8

const (
	wordUWordBoundsStateTag_Start wordUWordBoundsStateTag = iota
	wordUWordBoundsStateTag_Letter
	wordUWordBoundsStateTag_HLetter
	wordUWordBoundsStateTag_Numeric
	wordUWordBoundsStateTag_Katakana
	wordUWordBoundsStateTag_ExtendNumLet
	wordUWordBoundsStateTag_Regional
	wordUWordBoundsStateTag_FormatExtend
	wordUWordBoundsStateTag_Zwj
	wordUWordBoundsStateTag_Emoji
	wordUWordBoundsStateTag_WSegSpace
)

var (
	wordUWordBoundsState_Start        = wordUWordBoundsState{tag: wordUWordBoundsStateTag_Start}
	wordUWordBoundsState_Letter       = wordUWordBoundsState{tag: wordUWordBoundsStateTag_Letter}
	wordUWordBoundsState_HLetter      = wordUWordBoundsState{tag: wordUWordBoundsStateTag_HLetter}
	wordUWordBoundsState_Numeric      = wordUWordBoundsState{tag: wordUWordBoundsStateTag_Numeric}
	wordUWordBoundsState_Katakana     = wordUWordBoundsState{tag: wordUWordBoundsStateTag_Katakana}
	wordUWordBoundsState_ExtendNumLet = wordUWordBoundsState{tag: wordUWordBoundsStateTag_ExtendNumLet}
	wordUWordBoundsState_Zwj          = wordUWordBoundsState{tag: wordUWordBoundsStateTag_Zwj}
	wordUWordBoundsState_Emoji        = wordUWordBoundsState{tag: wordUWordBoundsStateTag_Emoji}
	wordUWordBoundsState_WSegSpace    = wordUWordBoundsState{tag: wordUWordBoundsStateTag_WSegSpace}
)

// wordUWordBoundsState_Regional is `UWordBoundsState::Regional(RegionalState)`.
func wordUWordBoundsState_Regional(f0 wordRegionalState) wordUWordBoundsState {
	return wordUWordBoundsState{tag: wordUWordBoundsStateTag_Regional, regional: f0}
}

// wordUWordBoundsState_FormatExtend is
// `UWordBoundsState::FormatExtend(FormatExtendType)`.
func wordUWordBoundsState_FormatExtend(f0 wordFormatExtendType) wordUWordBoundsState {
	return wordUWordBoundsState{tag: wordUWordBoundsStateTag_FormatExtend, formatExtend: f0}
}

// wordFormatExtendType is the crate's `FormatExtendType`:
// subtypes for FormatExtend state in UWordBoundsState
type wordFormatExtendType uint8

const (
	wordFormatExtendType_AcceptAny wordFormatExtendType = iota
	wordFormatExtendType_AcceptNone
	wordFormatExtendType_RequireLetter
	wordFormatExtendType_RequireHLetter
	wordFormatExtendType_AcceptQLetter
	wordFormatExtendType_RequireNumeric
)

// wordRegionalState is the crate's `RegionalState`.
type wordRegionalState uint8

const (
	wordRegionalState_Half wordRegionalState = iota
	wordRegionalState_Full
	wordRegionalState_Unknown
)

func wordIsEmoji(ch rune) bool {
	_, _, cat := tablesEmojiEmojiCategory(ch)
	return cat == tablesEmojiEmojiCat_EcExtendedPictographic
}

// SizeHint is `Iterator::size_hint`.
func (s *WordUWordBounds) SizeHint() (int, rs.Option[int]) {
	slen := len(s.string_)
	return min(slen, 1), rs.Some(slen)
}

// Next is `Iterator::next`: the next piece from the front.
func (s *WordUWordBounds) Next() rs.Option[string] {
	const (
		AcceptNone     = wordFormatExtendType_AcceptNone
		RequireLetter  = wordFormatExtendType_RequireLetter
		RequireHLetter = wordFormatExtendType_RequireHLetter
		AcceptQLetter  = wordFormatExtendType_AcceptQLetter
		RequireNumeric = wordFormatExtendType_RequireNumeric
	)
	var (
		Start        = wordUWordBoundsState_Start
		Letter       = wordUWordBoundsState_Letter
		HLetter      = wordUWordBoundsState_HLetter
		Numeric      = wordUWordBoundsState_Numeric
		Katakana     = wordUWordBoundsState_Katakana
		ExtendNumLet = wordUWordBoundsState_ExtendNumLet
		Zwj          = wordUWordBoundsState_Zwj
		Emoji        = wordUWordBoundsState_Emoji
		WSegSpace    = wordUWordBoundsState_WSegSpace
	)
	if s.string_ == "" {
		return rs.None[string]()
	}

	takeCurr := true
	takeCat := true
	idx := 0
	saveidx := 0
	state := Start
	cat := tablesWordWordCat_WcAny
	savecat := tablesWordWordCat_WcAny

	// If extend/format/zwj were skipped. Handles precedence of WB3d over WB4
	skippedFormatExtend := false
	// for (curr, ch) in self.string.char_indices()
	next := 0
loop:
	for next < len(s.string_) {
		curr := next
		ch, size := utf8.DecodeRuneInString(s.string_[curr:])
		next = curr + size

		idx = curr
		// Whether or not the previous category was ZWJ
		// ZWJs get collapsed, so this handles precedence of WB3c over WB4
		prevZwj := cat == tablesWordWordCat_WcZwj
		// if there's a category cached, grab it
		if cached, ok := s.cat.Take().Get(); ok {
			cat = cached
		} else {
			cat = tablesWordWordCategoryOf(ch)
		}
		takeCat = true

		// handle rule WB4
		// just skip all format, extend, and zwj chars
		// note that Start is a special case: if there's a bunch of Format | Extend
		// characters at the beginning of a block of text, dump them out as one unit.
		//
		// (This is not obvious from the wording of UAX#29, but if you look at the
		// test cases http://www.unicode.org/Public/UNIDATA/auxiliary/WordBreakTest.txt
		// then the "correct" interpretation of WB4 becomes apparent.)
		if state != Start {
			switch cat {
			case tablesWordWordCat_WcExtend, tablesWordWordCat_WcFormat, tablesWordWordCat_WcZwj:
				skippedFormatExtend = true
				continue loop
			}
		}

		// rule WB3c
		// WB4 makes all ZWJs collapse into the previous state
		// but you can still be in a Zwj state if you started with Zwj
		//
		// This means that an EP + Zwj will collapse into EP, which is wrong,
		// since EP+EP is not a boundary but EP+ZWJ+EP is
		//
		// Thus, we separately keep track of whether or not the last character
		// was a ZWJ. This is an additional bit of state tracked outside of the
		// state enum; the state enum represents the last non-zwj state encountered.
		// When prev_zwj is true, for the purposes of WB3c, we are in the Zwj state,
		// however we are in the previous state for the purposes of all other rules.
		if prevZwj && wordIsEmoji(ch) {
			state = Emoji
			continue loop
		}
		// Don't use `continue` in this match without updating `cat`
		switch state.tag {
		case wordUWordBoundsStateTag_Start:
			if cat == tablesWordWordCat_WcCr {
				if ncat, ok := s.getNextCat(idx).Get(); ok && ncat == tablesWordWordCat_WcLf {
					idx += 1 // rule WB3
				}
				break loop // rule WB3a
			}
			switch cat {
			case tablesWordWordCat_WcALetter:
				state = Letter // rule WB5, WB6, WB9, WB13a
			case tablesWordWordCat_WcHebrewLetter:
				state = HLetter // rule WB5, WB6, WB7a, WB7b, WB9, WB13a
			case tablesWordWordCat_WcNumeric:
				state = Numeric // rule WB8, WB10, WB12, WB13a
			case tablesWordWordCat_WcKatakana:
				state = Katakana // rule WB13, WB13a
			case tablesWordWordCat_WcExtendNumLet:
				state = ExtendNumLet // rule WB13a, WB13b
			case tablesWordWordCat_WcRegionalIndicator:
				state = wordUWordBoundsState_Regional(wordRegionalState_Half) // rule WB13c
			case tablesWordWordCat_WcLf, tablesWordWordCat_WcNewline:
				break loop // rule WB3a
			case tablesWordWordCat_WcZwj:
				state = Zwj // rule WB3c
			case tablesWordWordCat_WcWSegSpace:
				state = WSegSpace // rule WB3d
			default:
				if ncat, ok := s.getNextCat(idx).Get(); ok {
					// rule WB4
					if ncat == tablesWordWordCat_WcFormat || ncat == tablesWordWordCat_WcExtend || ncat == tablesWordWordCat_WcZwj {
						state = wordUWordBoundsState_FormatExtend(AcceptNone)
						s.cat = rs.Some(ncat)
						continue loop
					}
				}
				break loop // rule WB999
			}
		case wordUWordBoundsStateTag_WSegSpace:
			if cat == tablesWordWordCat_WcWSegSpace && !skippedFormatExtend {
				state = WSegSpace
			} else {
				takeCurr = false
				break loop
			}
		case wordUWordBoundsStateTag_Zwj:
			// We already handle WB3c above.
			takeCurr = false
			break loop
		case wordUWordBoundsStateTag_Letter, wordUWordBoundsStateTag_HLetter:
			switch {
			case cat == tablesWordWordCat_WcALetter:
				state = Letter // rule WB5
			case cat == tablesWordWordCat_WcHebrewLetter:
				state = HLetter // rule WB5
			case cat == tablesWordWordCat_WcNumeric:
				state = Numeric // rule WB9
			case cat == tablesWordWordCat_WcExtendNumLet:
				state = ExtendNumLet // rule WB13a
			case cat == tablesWordWordCat_WcDoubleQuote && state == HLetter:
				savecat = cat
				saveidx = idx
				state = wordUWordBoundsState_FormatExtend(RequireHLetter) // rule WB7b
			case cat == tablesWordWordCat_WcSingleQuote && state == HLetter:
				state = wordUWordBoundsState_FormatExtend(AcceptQLetter) // rule WB7a
			case cat == tablesWordWordCat_WcMidLetter || cat == tablesWordWordCat_WcMidNumLet || cat == tablesWordWordCat_WcSingleQuote:
				savecat = cat
				saveidx = idx
				state = wordUWordBoundsState_FormatExtend(RequireLetter) // rule WB6
			default:
				takeCurr = false
				break loop
			}
		case wordUWordBoundsStateTag_Numeric:
			switch cat {
			case tablesWordWordCat_WcNumeric:
				state = Numeric // rule WB8
			case tablesWordWordCat_WcALetter:
				state = Letter // rule WB10
			case tablesWordWordCat_WcHebrewLetter:
				state = HLetter // rule WB10
			case tablesWordWordCat_WcExtendNumLet:
				state = ExtendNumLet // rule WB13a
			case tablesWordWordCat_WcMidNum, tablesWordWordCat_WcMidNumLet, tablesWordWordCat_WcSingleQuote:
				savecat = cat
				saveidx = idx
				state = wordUWordBoundsState_FormatExtend(RequireNumeric) // rule WB12
			default:
				takeCurr = false
				break loop
			}
		case wordUWordBoundsStateTag_Katakana:
			switch cat {
			case tablesWordWordCat_WcKatakana:
				state = Katakana // rule WB13
			case tablesWordWordCat_WcExtendNumLet:
				state = ExtendNumLet // rule WB13a
			default:
				takeCurr = false
				break loop
			}
		case wordUWordBoundsStateTag_ExtendNumLet:
			switch cat {
			case tablesWordWordCat_WcExtendNumLet:
				state = ExtendNumLet // rule WB13a
			case tablesWordWordCat_WcALetter:
				state = Letter // rule WB13b
			case tablesWordWordCat_WcHebrewLetter:
				state = HLetter // rule WB13b
			case tablesWordWordCat_WcNumeric:
				state = Numeric // rule WB13b
			case tablesWordWordCat_WcKatakana:
				state = Katakana // rule WB13b
			default:
				takeCurr = false
				break loop
			}
		case wordUWordBoundsStateTag_Regional:
			switch state.regional {
			case wordRegionalState_Full:
				// if it reaches here we've gone too far,
				// a full flag can only compose with ZWJ/Extend/Format
				// proceeding it.
				takeCurr = false
				break loop
			case wordRegionalState_Half:
				switch cat {
				case tablesWordWordCat_WcRegionalIndicator:
					state = wordUWordBoundsState_Regional(wordRegionalState_Full) // rule WB13c
				default:
					takeCurr = false
					break loop
				}
			default:
				rs.PanicAt(0, "internal error: entered unreachable code: RegionalState::Unknown should not occur on forward iteration")
			}
		case wordUWordBoundsStateTag_Emoji:
			// We already handle WB3c above. If you've reached this point, the emoji sequence is over.
			takeCurr = false
			break loop
		case wordUWordBoundsStateTag_FormatExtend:
			t := state.formatExtend
			// handle FormatExtends depending on what type
			switch {
			case t == RequireNumeric && cat == tablesWordWordCat_WcNumeric:
				state = Numeric // rule WB11
			case (t == RequireLetter || t == AcceptQLetter) && cat == tablesWordWordCat_WcALetter:
				state = Letter // rule WB7
			case (t == RequireLetter || t == AcceptQLetter) && cat == tablesWordWordCat_WcHebrewLetter:
				state = HLetter // WB7a
			case t == RequireHLetter && cat == tablesWordWordCat_WcHebrewLetter:
				state = HLetter // rule WB7b
			case t == AcceptNone || t == AcceptQLetter:
				takeCurr = false // emit all the Format|Extend characters
				takeCat = false
				break loop
			default:
				break loop // rewind (in if statement below)
			}
		default:
			panic(rs.Unreachable())
		}
	}

	if state.tag == wordUWordBoundsStateTag_FormatExtend {
		t := state.formatExtend
		// we were looking for something and didn't find it; we have to back up
		if t == RequireLetter || t == RequireHLetter || t == RequireNumeric {
			idx = saveidx
			cat = savecat
			takeCurr = false
		}
	}

	if takeCurr {
		_, size := utf8.DecodeRuneInString(s.string_[idx:])
		idx = idx + size
		s.cat = rs.None[tablesWordWordCat]()
	} else if takeCat {
		s.cat = rs.Some(cat)
	} else {
		s.cat = rs.None[tablesWordWordCat]()
	}

	retstr := s.string_[:idx]
	s.string_ = s.string_[idx:]
	return rs.Some(retstr)
}

// NextBack is `DoubleEndedIterator::next_back`: the next piece from the back.
func (s *WordUWordBounds) NextBack() rs.Option[string] {
	const (
		AcceptAny      = wordFormatExtendType_AcceptAny
		AcceptNone     = wordFormatExtendType_AcceptNone
		RequireLetter  = wordFormatExtendType_RequireLetter
		RequireHLetter = wordFormatExtendType_RequireHLetter
		AcceptQLetter  = wordFormatExtendType_AcceptQLetter
		RequireNumeric = wordFormatExtendType_RequireNumeric
	)
	var (
		Start        = wordUWordBoundsState_Start
		Letter       = wordUWordBoundsState_Letter
		HLetter      = wordUWordBoundsState_HLetter
		Numeric      = wordUWordBoundsState_Numeric
		Katakana     = wordUWordBoundsState_Katakana
		ExtendNumLet = wordUWordBoundsState_ExtendNumLet
		Zwj          = wordUWordBoundsState_Zwj
		WSegSpace    = wordUWordBoundsState_WSegSpace
	)
	if s.string_ == "" {
		return rs.None[string]()
	}

	takeCurr := true
	takeCat := true
	idx := len(s.string_)
	_, lastSize := utf8.DecodeLastRuneInString(s.string_)
	idx -= lastSize
	previdx := idx
	saveidx := idx
	state := Start
	savestate := Start
	cat := tablesWordWordCat_WcAny

	// WB3c is context-sensitive (ZWJ + Extended_Pictographic),
	// while WB4 collapses Extend/Format and would otherwise hide that context.
	// We therefore keep this context outside the main state machine:
	// whether the nearest non-(Extend|Format) char to the right is emoji.
	rightSignificantIsEmoji := false

	skippedFormatExtend := false

	// for (curr, ch) in self.string.char_indices().rev()
	end := len(s.string_)
loop:
	for end > 0 {
		ch, size := utf8.DecodeLastRuneInString(s.string_[:end])
		curr := end - size
		end = curr

		previdx = idx
		idx = curr

		// if there's a category cached, grab it
		if cached, ok := s.catb.Take().Get(); ok {
			cat = cached
		} else {
			cat = tablesWordWordCategoryOf(ch)
		}
		takeCat = true

		// backward iterator over word boundaries. Mostly the same as the forward
		// iterator, with two weirdnesses:
		// (1) If we encounter a single quote in the Start state, we have to check for a
		//     Hebrew Letter immediately before it.
		// (2) Format and Extend char handling takes some gymnastics.

		// Reverse-direction WB3c check: when we encounter ZWJ and the nearest
		// significant right-side char is emoji, do not break here.
		if cat == tablesWordWordCat_WcZwj && state != Zwj && rightSignificantIsEmoji {
			continue loop
		}

		// Keep the right-side WB3c context up to date as we move left.
		// Ignore Extend/Format here to mirror WB4 collapsing behavior.
		if cat != tablesWordWordCat_WcExtend && cat != tablesWordWordCat_WcFormat {
			rightSignificantIsEmoji = wordIsEmoji(ch)
		}

		if cat == tablesWordWordCat_WcExtend || cat == tablesWordWordCat_WcFormat || (cat == tablesWordWordCat_WcZwj && state != Zwj) {
			// WB3c has more priority so we should not
			// fold in that case
			if !(state.tag == wordUWordBoundsStateTag_FormatExtend || state == Start) {
				saveidx = previdx
				savestate = state
				state = wordUWordBoundsState_FormatExtend(AcceptNone)
			}

			if state != Start {
				continue loop
			}
		} else if state == wordUWordBoundsState_FormatExtend(AcceptNone) {
			// finished a scan of some Format|Extend chars, restore previous state
			state = savestate
			previdx = saveidx
			takeCat = false
			skippedFormatExtend = true
		}

		// Don't use `continue` in this match without updating `catb`
		switch {
		case state == Start || state == wordUWordBoundsState_FormatExtend(AcceptAny):
			switch {
			case cat == tablesWordWordCat_WcALetter:
				state = Letter // rule WB5, WB7, WB10, WB13b
			case cat == tablesWordWordCat_WcHebrewLetter:
				state = HLetter // rule WB5, WB7, WB7c, WB10, WB13b
			case cat == tablesWordWordCat_WcNumeric:
				state = Numeric // rule WB8, WB9, WB11, WB13b
			case cat == tablesWordWordCat_WcKatakana:
				state = Katakana // rule WB13, WB13b
			case cat == tablesWordWordCat_WcExtendNumLet:
				state = ExtendNumLet // rule WB13a
			case cat == tablesWordWordCat_WcRegionalIndicator:
				state = wordUWordBoundsState_Regional(wordRegionalState_Unknown) // rule WB13c
			// rule WB4:
			case cat == tablesWordWordCat_WcExtend || cat == tablesWordWordCat_WcFormat || cat == tablesWordWordCat_WcZwj:
				state = wordUWordBoundsState_FormatExtend(AcceptAny)
			case cat == tablesWordWordCat_WcSingleQuote:
				saveidx = idx
				state = wordUWordBoundsState_FormatExtend(AcceptQLetter) // rule WB7a
			case cat == tablesWordWordCat_WcWSegSpace:
				state = WSegSpace
			case cat == tablesWordWordCat_WcCr || cat == tablesWordWordCat_WcLf || cat == tablesWordWordCat_WcNewline:
				if state == Start {
					if cat == tablesWordWordCat_WcLf {
						if pcat, ok := s.getPrevCat(idx).Get(); ok && pcat == tablesWordWordCat_WcCr {
							idx -= 1 // rule WB3
						}
					}
				} else {
					takeCurr = false
				}
				break loop // rule WB3a
			case wordIsEmoji(ch):
				state = Zwj
			default:
				break loop // rule WB999
			}
		case state.tag == wordUWordBoundsStateTag_Zwj:
			switch cat {
			// rule WB3c
			case tablesWordWordCat_WcZwj:
				state = wordUWordBoundsState_FormatExtend(AcceptAny)
			default:
				takeCurr = false
				break loop
			}
		case state.tag == wordUWordBoundsStateTag_WSegSpace:
			// rule WB3d
			if cat == tablesWordWordCat_WcWSegSpace && !skippedFormatExtend {
				state = WSegSpace
			} else {
				takeCurr = false
				break loop
			}
		case state.tag == wordUWordBoundsStateTag_Letter || state.tag == wordUWordBoundsStateTag_HLetter:
			switch {
			case cat == tablesWordWordCat_WcALetter:
				state = Letter // rule WB5
			case cat == tablesWordWordCat_WcHebrewLetter:
				state = HLetter // rule WB5
			case cat == tablesWordWordCat_WcNumeric:
				state = Numeric // rule WB10
			case cat == tablesWordWordCat_WcExtendNumLet:
				state = ExtendNumLet // rule WB13b
			case cat == tablesWordWordCat_WcDoubleQuote && state == HLetter:
				saveidx = previdx
				state = wordUWordBoundsState_FormatExtend(RequireHLetter) // rule WB7c
			case cat == tablesWordWordCat_WcMidLetter || cat == tablesWordWordCat_WcMidNumLet || cat == tablesWordWordCat_WcSingleQuote:
				saveidx = previdx
				state = wordUWordBoundsState_FormatExtend(RequireLetter) // rule WB7
			default:
				takeCurr = false
				break loop
			}
		case state.tag == wordUWordBoundsStateTag_Numeric:
			switch cat {
			case tablesWordWordCat_WcNumeric:
				state = Numeric // rule WB8
			case tablesWordWordCat_WcALetter:
				state = Letter // rule WB9
			case tablesWordWordCat_WcHebrewLetter:
				state = HLetter // rule WB9
			case tablesWordWordCat_WcExtendNumLet:
				state = ExtendNumLet // rule WB13b
			case tablesWordWordCat_WcMidNum, tablesWordWordCat_WcMidNumLet, tablesWordWordCat_WcSingleQuote:
				saveidx = previdx
				state = wordUWordBoundsState_FormatExtend(RequireNumeric) // rule WB11
			default:
				takeCurr = false
				break loop
			}
		case state.tag == wordUWordBoundsStateTag_Katakana:
			switch cat {
			case tablesWordWordCat_WcKatakana:
				state = Katakana // rule WB13
			case tablesWordWordCat_WcExtendNumLet:
				state = ExtendNumLet // rule WB13b
			default:
				takeCurr = false
				break loop
			}
		case state.tag == wordUWordBoundsStateTag_ExtendNumLet:
			switch cat {
			case tablesWordWordCat_WcExtendNumLet:
				state = ExtendNumLet // rule WB13a
			case tablesWordWordCat_WcALetter:
				state = Letter // rule WB13a
			case tablesWordWordCat_WcHebrewLetter:
				state = HLetter // rule WB13a
			case tablesWordWordCat_WcNumeric:
				state = Numeric // rule WB13a
			case tablesWordWordCat_WcKatakana:
				state = Katakana // rule WB13a
			default:
				takeCurr = false
				break loop
			}
		case state.tag == wordUWordBoundsStateTag_Regional:
			regionalState := state.regional
			switch cat {
			// rule WB13c
			case tablesWordWordCat_WcRegionalIndicator:
				if regionalState == wordRegionalState_Unknown {
					// self.string[..previdx].chars().rev()
					//     .map(|c| wd::word_category(c).2)
					//     .filter(|&c| !(c == wd::WC_ZWJ || c == wd::WC_Extend || c == wd::WC_Format))
					//     .take_while(|&c| c == wd::WC_Regional_Indicator)
					//     .count()
					count := 0
					for rest := s.string_[:previdx]; rest != ""; {
						c, size := utf8.DecodeLastRuneInString(rest)
						rest = rest[:len(rest)-size]
						c2 := tablesWordWordCategoryOf(c)
						if c2 == tablesWordWordCat_WcZwj || c2 == tablesWordWordCat_WcExtend || c2 == tablesWordWordCat_WcFormat {
							continue
						}
						if c2 != tablesWordWordCat_WcRegionalIndicator {
							break
						}
						count++
					}
					if count%2 == 0 {
						regionalState = wordRegionalState_Full
					} else {
						regionalState = wordRegionalState_Half
					}
				}
				if regionalState == wordRegionalState_Full {
					takeCurr = false
					break loop
				} else {
					state = wordUWordBoundsState_Regional(wordRegionalState_Full)
				}
			default:
				takeCurr = false
				break loop
			}
		case state.tag == wordUWordBoundsStateTag_Emoji:
			if wordIsEmoji(ch) {
				// rule WB3c
				state = Zwj
			} else {
				takeCurr = false
				break loop
			}
		case state.tag == wordUWordBoundsStateTag_FormatExtend:
			t := state.formatExtend
			switch {
			case t == RequireNumeric && cat == tablesWordWordCat_WcNumeric:
				state = Numeric // rule WB12
			case t == RequireLetter && cat == tablesWordWordCat_WcALetter:
				state = Letter // rule WB6
			case t == RequireLetter && cat == tablesWordWordCat_WcHebrewLetter:
				state = HLetter // rule WB6
			case t == AcceptQLetter && cat == tablesWordWordCat_WcHebrewLetter:
				state = HLetter // rule WB7a
			case t == RequireHLetter && cat == tablesWordWordCat_WcHebrewLetter:
				state = HLetter // rule WB7b
			default:
				break loop // backtrack will happens
			}
		default:
			panic(rs.Unreachable())
		}
	}

	if state.tag == wordUWordBoundsStateTag_FormatExtend {
		t := state.formatExtend
		// if we required something but didn't find it, backtrack
		if t == RequireLetter ||
			t == RequireHLetter ||
			t == RequireNumeric ||
			t == AcceptNone ||
			t == AcceptQLetter {
			previdx = saveidx
			takeCat = false
			takeCurr = false
		}
	}

	if takeCurr {
		s.catb = rs.None[tablesWordWordCat]()
	} else {
		idx = previdx
		if takeCat {
			s.catb = rs.Some(cat)
		} else {
			s.catb = rs.None[tablesWordWordCat]()
		}
	}

	retstr := s.string_[idx:]
	s.string_ = s.string_[:idx]
	return rs.Some(retstr)
}

// AsStr is `UWordBounds::as_str`:
// view the underlying data (the part yet to be iterated) as a slice of the original string.
//
//	iter := unicodesegmentation.SplitWordBounds("Hello world")
//	iter.AsStr() // "Hello world"
//	iter.Next()
//	iter.AsStr() // " world"
//	iter.Next()
//	iter.AsStr() // "world"
func (s *WordUWordBounds) AsStr() string {
	return s.string_
}

func (s *WordUWordBounds) getNextCat(idx int) rs.Option[tablesWordWordCat] {
	_, size := utf8.DecodeRuneInString(s.string_[idx:])
	nidx := idx + size
	if nidx < len(s.string_) {
		nch, _ := utf8.DecodeRuneInString(s.string_[nidx:])
		return rs.Some(tablesWordWordCategoryOf(nch))
	} else {
		return rs.None[tablesWordWordCat]()
	}
}

func (s *WordUWordBounds) getPrevCat(idx int) rs.Option[tablesWordWordCat] {
	if idx > 0 {
		nch, _ := utf8.DecodeLastRuneInString(s.string_[:idx])
		return rs.Some(tablesWordWordCategoryOf(nch))
	} else {
		return rs.None[tablesWordWordCat]()
	}
}

// wordAsciiWordBoundIter is the crate's `AsciiWordBoundIter`:
// ASCII‑fast‑path word‑boundary iterator for strings that contain only ASCII characters.
//
// Since we handle only ASCII characters, we can use a much simpler set of
// word break values than the full Unicode algorithm.
// https://www.unicode.org/reports/tr29/#Table_Word_Break_Property_Values
//
// | Word_Break value | ASCII code points that belong to it                             |
// | -----------------| --------------------------------------------------------------- |
// | CR               | U+000D (CR)                                                     |
// | LF               | U+000A (LF)                                                     |
// | Newline          | U+000B (VT), U+000C (FF)                                        |
// | Single_Quote     | U+0027 (')                                                      |
// | Double_Quote     | U+0022 (")                                                      |
// | MidNumLet        | U+002E (.) FULL STOP                                            |
// | MidLetter        | U+003A (:) COLON                                                |
// | MidNum           | U+002C (,), U+003B (;)                                          |
// | Numeric          | U+0030 – U+0039 (0 … 9)                                         |
// | ALetter          | U+0041 – U+005A (A … Z), U+0061 – U+007A (a … z)                |
// | ExtendNumLet     | U+005F (_) underscore                                           |
// | WSegSpace        | U+0020 (SPACE)                                                  |
//
// The macro MidNumLetQ boils down to: U+002E (.) FULL STOP and U+0027 (')
// AHLetter is the same as ALetter, so we don't need to distinguish it.
//
// Any other single ASCII byte is its own boundary (the default WB999).
type wordAsciiWordBoundIter struct {
	rest   string `rs:"rest"`
	offset int    `rs:"offset"`
}

func wordAsciiWordBoundIterNew(s string) wordAsciiWordBoundIter {
	return wordAsciiWordBoundIter{rest: s, offset: 0}
}

func wordAsciiWordBoundIterIsCore(b uint8) bool {
	return strs.IsAsciiAlphanumeric(b) || b == '_'
}

func wordAsciiWordBoundIterIsInfix(b uint8, prev uint8, next uint8) bool {
	switch {
	// Numeric separators such as "1,000" or "3.14" (WB11/WB12)
	//
	// "Numeric (MidNum | MidNumLetQ) Numeric"
	case (b == '.' || b == ',' || b == ';' || b == '\'') && strs.IsAsciiDigit(prev) && strs.IsAsciiDigit(next):
		return true

	// Dot or colon inside an alphabetic word ("e.g.", "http://") (WB6/WB7)
	//
	// "(MidLetter | MidNumLetQ) AHLetter (MidLetter | MidNumLetQ)"
	// MidLetter  = b':'
	// MidNumLetQ = b'.' | b'\''
	case (b == '\'' || b == '.' || b == ':') && strs.IsAsciiAlphabetic(prev) && strs.IsAsciiAlphabetic(next):
		return true
	default:
		return false
	}
}

// Next is `Iterator::next`.
func (s *wordAsciiWordBoundIter) Next() rs.Option[rs.Tuple2[int, string]] {
	if s.rest == "" {
		return rs.None[rs.Tuple2[int, string]]()
	}

	bytes := s.rest
	len_ := len(bytes)

	// 1) Keep horizontal whitespace together.
	// Spec: WB3d joins adjacent *WSegSpace* into a single segment.
	if bytes[0] == ' ' {
		i := 1
		for i < len_ && bytes[i] == ' ' {
			i += 1
		}
		word := s.rest[:i]
		pos := s.offset
		s.rest = s.rest[i:]
		s.offset += i
		return rs.Some(rs.T2(pos, word))
	}

	// 2) Core-run (letters/digits/underscore + infix)
	// Spec: ALetter × ALetter, Numeric × Numeric etc. (WB5–WB13b)
	if wordAsciiWordBoundIterIsCore(bytes[0]) {
		i := 1
		for i < len_ {
			b := bytes[i]
			if wordAsciiWordBoundIterIsCore(b) ||
				(i+1 < len_ && wordAsciiWordBoundIterIsInfix(b, bytes[i-1], bytes[i+1])) {
				i += 1
			} else {
				break
			}
		}
		word := s.rest[:i]
		pos := s.offset
		s.rest = s.rest[i:]
		s.offset += i
		return rs.Some(rs.T2(pos, word))
	}

	// 3) Do not break within CRLF.
	// Spec: WB3 treats CR+LF as a single non‑breaking pair.
	if bytes[0] == '\r' && len_ >= 2 && bytes[1] == '\n' {
		word := s.rest[:2]
		pos := s.offset
		s.rest = s.rest[2:]
		s.offset += 2
		return rs.Some(rs.T2(pos, word))
	} else {
		// 4) Otherwise, break everywhere
		// Spec: the catch‑all rule WB999.
		word := s.rest[:1]
		pos := s.offset
		s.rest = s.rest[1:]
		s.offset += 1
		return rs.Some(rs.T2(pos, word))
	}
}

// NextBack is `DoubleEndedIterator::next_back`.
func (s *wordAsciiWordBoundIter) NextBack() rs.Option[rs.Tuple2[int, string]] {
	rest := s.rest
	if rest == "" {
		return rs.None[rs.Tuple2[int, string]]()
	}
	bytes := rest
	len_ := len(bytes)

	// 1) Group runs of spaces
	// Spec: WB3d joins adjacent *WSegSpace* into a single segment.
	if bytes[len_-1] == ' ' {
		// find start of this last run of spaces
		start := len_ - 1
		for start > 0 && bytes[start-1] == ' ' {
			start -= 1
		}
		word := rest[start:]
		pos := s.offset + start
		s.rest = rest[:start]
		return rs.Some(rs.T2(pos, word))
	}

	// 2) Trailing Core-run (letters/digits/underscore + infix)
	// Spec: ALetter × ALetter, Numeric × Numeric etc. (WB5–WB13b)
	if wordAsciiWordBoundIterIsCore(bytes[len_-1]) {
		// scan backwards as long as we see `is_core` or an `is_infix`
		start := len_ - 1
		for start > 0 {
			b := bytes[start-1]
			prev := b
			if start >= 2 {
				prev = bytes[start-2]
			}
			next := bytes[start] // the byte we just included
			if wordAsciiWordBoundIterIsCore(b) || wordAsciiWordBoundIterIsInfix(b, prev, next) {
				start -= 1
			} else {
				break
			}
		}
		word := rest[start:]
		pos := s.offset + start
		s.rest = rest[:start]
		return rs.Some(rs.T2(pos, word))
	}

	// 3) Non-core: CR+LF as one token, otherwise single char
	// Spec: WB3 treats CR+LF as a single non‑breaking pair.
	if len_ >= 2 && bytes[len_-2] == '\r' && bytes[len_-1] == '\n' {
		start := len_ - 2
		word := rest[start:]
		pos := s.offset + start
		s.rest = rest[:start]
		return rs.Some(rs.T2(pos, word))
	}

	// 4) Fallback – every other byte is its own segment
	// Spec: the catch‑all rule WB999.
	start := len_ - 1
	word := rest[start:]
	pos := s.offset + start
	s.rest = rest[:start]
	return rs.Some(rs.T2(pos, word))
}

func wordAsciiWordOk(t *rs.Tuple2[int, string]) bool {
	return wordHasAsciiAlphanumeric(t.F1)
}

func wordUnicodeWordOk(t *rs.Tuple2[int, string]) bool {
	return wordHasAlphanumeric(t.F1)
}

// The four iterators below are the crate's type aliases for adaptors of
// Rust's standard library, `Filter` and `Map`, around the iterators above.
// Each holds the inner iterator; the functions the crate gives the adaptors
// are named in the comments. `Filter::size_hint` is no lower bound and the
// inner iterator's upper bound.

// wordAsciiWordsIter is the crate's `AsciiWordsIter`:
// `Filter<Map<AsciiWordBoundIter, fn((usize, &str)) -> &str>, fn(&&str) -> bool>`,
// built in wordNewUnicodeWordsAscii with wordStripPos and
// wordHasAsciiAlphanumeric.
type wordAsciiWordsIter struct {
	iter wordAsciiWordBoundIter `rs:"iter"`
}

func (s *wordAsciiWordsIter) Next() rs.Option[string] {
	for {
		item, ok := s.iter.Next().Get()
		if !ok {
			return rs.None[string]()
		}
		if w := wordStripPos(item); wordHasAsciiAlphanumeric(w) {
			return rs.Some(w)
		}
	}
}

func (s *wordAsciiWordsIter) NextBack() rs.Option[string] {
	for {
		item, ok := s.iter.NextBack().Get()
		if !ok {
			return rs.None[string]()
		}
		if w := wordStripPos(item); wordHasAsciiAlphanumeric(w) {
			return rs.Some(w)
		}
	}
}

func (s *wordAsciiWordsIter) SizeHint() (int, rs.Option[int]) {
	// AsciiWordBoundIter keeps `Iterator`'s default size_hint, (0, None).
	return 0, rs.None[int]()
}

// wordUnicodeWordsIter is the crate's `UnicodeWordsIter`:
// `Filter<UWordBounds, fn(&&str) -> bool>`, built in
// wordNewUnicodeWordsGeneral with wordHasAlphanumeric.
type wordUnicodeWordsIter struct {
	iter WordUWordBounds `rs:"iter"`
}

func (s *wordUnicodeWordsIter) Next() rs.Option[string] {
	for {
		w, ok := s.iter.Next().Get()
		if !ok {
			return rs.None[string]()
		}
		if wordHasAlphanumeric(w) {
			return rs.Some(w)
		}
	}
}

func (s *wordUnicodeWordsIter) NextBack() rs.Option[string] {
	for {
		w, ok := s.iter.NextBack().Get()
		if !ok {
			return rs.None[string]()
		}
		if wordHasAlphanumeric(w) {
			return rs.Some(w)
		}
	}
}

func (s *wordUnicodeWordsIter) SizeHint() (int, rs.Option[int]) {
	_, upper := s.iter.SizeHint()
	return 0, upper
}

// wordAsciiIndicesIter is the crate's `AsciiIndicesIter`:
// `Filter<AsciiWordBoundIter, fn(&(usize, &str)) -> bool>`, built in
// wordNewUnicodeWordIndices with wordAsciiWordOk.
type wordAsciiIndicesIter struct {
	iter wordAsciiWordBoundIter `rs:"iter"`
}

func (s *wordAsciiIndicesIter) Next() rs.Option[rs.Tuple2[int, string]] {
	for {
		item, ok := s.iter.Next().Get()
		if !ok {
			return rs.None[rs.Tuple2[int, string]]()
		}
		if wordAsciiWordOk(&item) {
			return rs.Some(item)
		}
	}
}

func (s *wordAsciiIndicesIter) NextBack() rs.Option[rs.Tuple2[int, string]] {
	for {
		item, ok := s.iter.NextBack().Get()
		if !ok {
			return rs.None[rs.Tuple2[int, string]]()
		}
		if wordAsciiWordOk(&item) {
			return rs.Some(item)
		}
	}
}

func (s *wordAsciiIndicesIter) SizeHint() (int, rs.Option[int]) {
	return 0, rs.None[int]()
}

// wordUnicodeIndicesIter is the crate's `UnicodeIndicesIter`:
// `Filter<UWordBoundIndices, fn(&(usize, &str)) -> bool>`, built in
// wordNewUnicodeWordIndices with wordUnicodeWordOk.
type wordUnicodeIndicesIter struct {
	iter WordUWordBoundIndices `rs:"iter"`
}

func (s *wordUnicodeIndicesIter) Next() rs.Option[rs.Tuple2[int, string]] {
	for {
		item, ok := s.iter.Next().Get()
		if !ok {
			return rs.None[rs.Tuple2[int, string]]()
		}
		if wordUnicodeWordOk(&item) {
			return rs.Some(item)
		}
	}
}

func (s *wordUnicodeIndicesIter) NextBack() rs.Option[rs.Tuple2[int, string]] {
	for {
		item, ok := s.iter.NextBack().Get()
		if !ok {
			return rs.None[rs.Tuple2[int, string]]()
		}
		if wordUnicodeWordOk(&item) {
			return rs.Some(item)
		}
	}
}

func (s *wordUnicodeIndicesIter) SizeHint() (int, rs.Option[int]) {
	_, upper := s.iter.SizeHint()
	return 0, upper
}

// wordWordsIter is the crate's `WordsIter`.
type wordWordsIter interface{ isWordWordsIter() }

type wordWordsIter_Ascii struct{ F0 wordAsciiWordsIter }
type wordWordsIter_Unicode struct{ F0 wordUnicodeWordsIter }

func (*wordWordsIter_Ascii) isWordWordsIter()   {}
func (*wordWordsIter_Unicode) isWordWordsIter() {}

// wordIndicesIter is the crate's `IndicesIter`.
type wordIndicesIter interface{ isWordIndicesIter() }

type wordIndicesIter_Ascii struct{ F0 wordAsciiIndicesIter }
type wordIndicesIter_Unicode struct{ F0 wordUnicodeIndicesIter }

func (*wordIndicesIter_Ascii) isWordIndicesIter()   {}
func (*wordIndicesIter_Unicode) isWordIndicesIter() {}

func wordNewUnicodeWords(s string) *WordUnicodeWords {
	var inner wordWordsIter
	if strs.IsAscii(s) {
		inner = &wordWordsIter_Ascii{F0: wordNewUnicodeWordsAscii(s)}
	} else {
		inner = &wordWordsIter_Unicode{F0: wordNewUnicodeWordsGeneral(s)}
	}
	return &WordUnicodeWords{inner: inner}
}

func wordNewUnicodeWordIndices(s string) *WordUnicodeWordIndices {
	var inner wordIndicesIter
	if strs.IsAscii(s) {
		inner = &wordIndicesIter_Ascii{F0: wordAsciiIndicesIter{iter: wordNewAsciiWordBoundIndices(s)}}
	} else {
		inner = &wordIndicesIter_Unicode{F0: wordUnicodeIndicesIter{iter: *wordNewWordBoundIndices(s)}}
	}
	return &WordUnicodeWordIndices{inner: inner}
}

func wordNewWordBounds(s string) *WordUWordBounds {
	return &WordUWordBounds{
		string_: s,
		cat:     rs.None[tablesWordWordCat](),
		catb:    rs.None[tablesWordWordCat](),
	}
}

func wordNewWordBoundIndices(s string) *WordUWordBoundIndices {
	return &WordUWordBoundIndices{
		front: 0,
		iter:  *wordNewWordBounds(s),
	}
}

func wordNewAsciiWordBoundIndices(s string) wordAsciiWordBoundIter {
	return wordAsciiWordBoundIterNew(s)
}

func wordHasAlphanumeric(s string) bool {
	// s.chars().any(is_alphanumeric)
	for _, c := range s {
		if tablesUtilIsAlphanumeric(c) {
			return true
		}
	}
	return false
}

func wordHasAsciiAlphanumeric(s string) bool {
	// s.chars().any(|c| c.is_ascii_alphanumeric())
	for _, c := range s {
		if strs.IsAsciiAlphanumeric(c) {
			return true
		}
	}
	return false
}

func wordStripPos(t rs.Tuple2[int, string]) string {
	return t.F1
}

func wordNewUnicodeWordsAscii(s string) wordAsciiWordsIter {
	return wordAsciiWordsIter{iter: wordNewAsciiWordBoundIndices(s)}
}

func wordNewUnicodeWordsGeneral(s string) wordUnicodeWordsIter {
	return wordUnicodeWordsIter{iter: *wordNewWordBounds(s)}
}
