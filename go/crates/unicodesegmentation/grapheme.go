// Port of unicode-segmentation 1.13.3 src/grapheme.rs

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
	"strconv"
	"unicode/utf8"

	"github.com/BrokkAi/mjolnir/go/rs"
	"github.com/BrokkAi/mjolnir/go/rs/strs"
)

// GraphemeGraphemeIndices is the crate's `GraphemeIndices`:
// external iterator for grapheme clusters and byte offsets.
//
// This struct is created by [GraphemeIndices], the `grapheme_indices` method
// on the `UnicodeSegmentation` trait. See its documentation for more.
//
// Rust keeps the address of the string here (`start_offset`) and finds the
// offset of each cluster by subtracting it from the cluster's address. The
// address of a Go string is not a stable number, so the offset is read from
// the cursors, which hold the same number.
type GraphemeGraphemeIndices struct {
	iter GraphemeGraphemes `rs:"iter"`
}

// AsStr is `GraphemeIndices::as_str`:
// view the underlying data (the part yet to be iterated) as a slice of the original string.
//
//	iter := unicodesegmentation.GraphemeIndices("abc", true)
//	iter.AsStr() // "abc"
//	iter.Next()
//	iter.AsStr() // "bc"
//	iter.Next()
//	iter.Next()
//	iter.AsStr() // ""
func (s *GraphemeGraphemeIndices) AsStr() string {
	return s.iter.AsStr()
}

// Clone is the derived `Clone`.
func (s *GraphemeGraphemeIndices) Clone() GraphemeGraphemeIndices {
	return *s
}

// Next is `Iterator::next`: the next cluster from the front, with its byte
// offset in the string.
func (s *GraphemeGraphemeIndices) Next() rs.Option[rs.Tuple2[int, string]] {
	start := s.iter.cursor.CurCursor()
	cluster, ok := s.iter.Next().Get()
	if !ok {
		return rs.None[rs.Tuple2[int, string]]()
	}
	return rs.Some(rs.T2(start, cluster))
}

// SizeHint is `Iterator::size_hint`.
func (s *GraphemeGraphemeIndices) SizeHint() (int, rs.Option[int]) {
	return s.iter.SizeHint()
}

// NextBack is `DoubleEndedIterator::next_back`: the next cluster from the
// back, with its byte offset in the string.
func (s *GraphemeGraphemeIndices) NextBack() rs.Option[rs.Tuple2[int, string]] {
	cluster, ok := s.iter.NextBack().Get()
	if !ok {
		return rs.None[rs.Tuple2[int, string]]()
	}
	return rs.Some(rs.T2(s.iter.cursorBack.CurCursor(), cluster))
}

// GraphemeGraphemes is the crate's `Graphemes`:
// external iterator for a string's
// [grapheme clusters](http://www.unicode.org/reports/tr29/#Grapheme_Cluster_Boundaries).
//
// This struct is created by [Graphemes], the `graphemes` method on the
// `UnicodeSegmentation` trait. See its documentation for more.
type GraphemeGraphemes struct {
	string_    string         `rs:"string"`
	cursor     GraphemeCursor `rs:"cursor"`
	cursorBack GraphemeCursor `rs:"cursor_back"`
}

var _ = rs.RegisterStruct[GraphemeGraphemes]("Graphemes")

// AsStr is `Graphemes::as_str`:
// view the underlying data (the part yet to be iterated) as a slice of the original string.
//
//	iter := unicodesegmentation.Graphemes("abc", true)
//	iter.AsStr() // "abc"
//	iter.Next()
//	iter.AsStr() // "bc"
//	iter.Next()
//	iter.Next()
//	iter.AsStr() // ""
func (s *GraphemeGraphemes) AsStr() string {
	return strs.Slice(s.string_, s.cursor.CurCursor(), s.cursorBack.CurCursor())
}

// Clone is the derived `Clone`.
func (s *GraphemeGraphemes) Clone() GraphemeGraphemes {
	return *s
}

// SizeHint is `Iterator::size_hint`.
func (s *GraphemeGraphemes) SizeHint() (int, rs.Option[int]) {
	slen := s.cursorBack.CurCursor() - s.cursor.CurCursor()
	return min(slen, 1), rs.Some(slen)
}

// Next is `Iterator::next`: the next cluster from the front.
func (s *GraphemeGraphemes) Next() rs.Option[string] {
	start := s.cursor.CurCursor()
	if start == s.cursorBack.CurCursor() {
		return rs.None[string]()
	}
	next := rs.Unwrap(s.cursor.NextBoundary(s.string_, 0)).Unwrap()
	return rs.Some(strs.Slice(s.string_, start, next))
}

// NextBack is `DoubleEndedIterator::next_back`: the next cluster from the
// back.
func (s *GraphemeGraphemes) NextBack() rs.Option[string] {
	end := s.cursorBack.CurCursor()
	if end == s.cursor.CurCursor() {
		return rs.None[string]()
	}
	prev := rs.Unwrap(s.cursorBack.PrevBoundary(s.string_, 0)).Unwrap()
	return rs.Some(strs.Slice(s.string_, prev, end))
}

func graphemeNewGraphemes(s string, isExtended bool) *GraphemeGraphemes {
	len_ := len(s)
	return &GraphemeGraphemes{
		string_:    s,
		cursor:     *GraphemeCursorNew(0, len_, isExtended),
		cursorBack: *GraphemeCursorNew(len_, len_, isExtended),
	}
}

func graphemeNewGraphemeIndices(s string, isExtended bool) *GraphemeGraphemeIndices {
	return &GraphemeGraphemeIndices{
		iter: *graphemeNewGraphemes(s, isExtended),
	}
}

// graphemeGraphemeState is the crate's `GraphemeState`:
// maybe unify with PairResult?
// An enum describing information about a potential boundary.
//
// It is held as a value that `==` compares, a tag and the field of the one
// variant that has one, because the cursor tests it at every character.
type graphemeGraphemeState struct {
	tag graphemeGraphemeStateTag
	// Whether the ZWJ char has been seen already an only a "\p{Extended_Pictographic} Extend*"
	// part of GB11 has to be checked
	seenZwj bool
}

type graphemeGraphemeStateTag uint8

const (
	graphemeGraphemeStateTag_Unknown graphemeGraphemeStateTag = iota
	graphemeGraphemeStateTag_NotBreak
	graphemeGraphemeStateTag_Break
	graphemeGraphemeStateTag_InCbConsonant
	graphemeGraphemeStateTag_Regional
	graphemeGraphemeStateTag_Emoji
)

var (
	// No information is known.
	graphemeGraphemeState_Unknown = graphemeGraphemeState{tag: graphemeGraphemeStateTag_Unknown}
	// It is known to not be a boundary.
	graphemeGraphemeState_NotBreak = graphemeGraphemeState{tag: graphemeGraphemeStateTag_NotBreak}
	// It is known to be a boundary.
	graphemeGraphemeState_Break = graphemeGraphemeState{tag: graphemeGraphemeStateTag_Break}
	// The codepoint after it has Indic_Conjunct_Break=Consonant,
	// so there is a break before so a boundary if it is preceded by another
	// InCB=Consonant follwoed by a sequence consisting of one or more InCB=Linker
	// and zero or more InCB = Extend (in any order).
	graphemeGraphemeState_InCbConsonant = graphemeGraphemeState{tag: graphemeGraphemeStateTag_InCbConsonant}
	// The codepoint after is a Regional Indicator Symbol, so a boundary iff
	// it is preceded by an even number of RIS codepoints. (GB12, GB13)
	graphemeGraphemeState_Regional = graphemeGraphemeState{tag: graphemeGraphemeStateTag_Regional}
)

// graphemeGraphemeState_Emoji is `GraphemeState::Emoji { seen_zwj }`:
// the codepoint after is Extended_Pictographic,
// so whether it's a boundary depends on pre-context according to GB11.
func graphemeGraphemeState_Emoji(seenZwj bool) graphemeGraphemeState {
	return graphemeGraphemeState{tag: graphemeGraphemeStateTag_Emoji, seenZwj: seenZwj}
}

// FmtDebug is the derived `Debug`.
func (s graphemeGraphemeState) FmtDebug(f *rs.Formatter) error {
	switch s.tag {
	case graphemeGraphemeStateTag_Unknown:
		return f.WriteStr("Unknown")
	case graphemeGraphemeStateTag_NotBreak:
		return f.WriteStr("NotBreak")
	case graphemeGraphemeStateTag_Break:
		return f.WriteStr("Break")
	case graphemeGraphemeStateTag_InCbConsonant:
		return f.WriteStr("InCbConsonant")
	case graphemeGraphemeStateTag_Regional:
		return f.WriteStr("Regional")
	case graphemeGraphemeStateTag_Emoji:
		return f.DebugStruct("Emoji").Field("seen_zwj", s.seenZwj).Finish()
	default:
		panic(rs.Unreachable(s.tag))
	}
}

// GraphemeCursor is the crate's `GraphemeCursor`:
// cursor-based segmenter for grapheme clusters.
//
// This allows working with ropes and other datastructures where the string is not contiguous or
// fully known at initialization time.
type GraphemeCursor struct {
	// Current cursor position.
	offset int `rs:"offset"`
	// Total length of the string.
	len int `rs:"len"`
	// A config flag indicating whether this cursor computes legacy or extended
	// grapheme cluster boundaries (enables GB9a and GB9b if set).
	isExtended bool `rs:"is_extended"`
	// Information about the potential boundary at `offset`
	state graphemeGraphemeState `rs:"state"`
	// Category of codepoint immediately preceding cursor, if known.
	catBefore rs.Option[tablesGraphemeGraphemeCat] `rs:"cat_before"`
	// Category of codepoint immediately after cursor, if known.
	catAfter rs.Option[tablesGraphemeGraphemeCat] `rs:"cat_after"`
	// If set, at least one more codepoint immediately preceding this offset
	// is needed to resolve whether there's a boundary at `offset`.
	preContextOffset rs.Option[int] `rs:"pre_context_offset"`
	// The number of `InCB=Linker` codepoints preceding `offset`
	// (potentially intermingled with `InCB=Extend`).
	incbLinkerCount rs.Option[int] `rs:"incb_linker_count"`
	// The number of RIS codepoints preceding `offset`. If `pre_context_offset`
	// is set, then counts the number of RIS between that and `offset`, otherwise
	// is an accurate count relative to the string.
	risCount rs.Option[int] `rs:"ris_count"`
	// Set if a call to `prev_boundary` or `next_boundary` was suspended due
	// to needing more input.
	resuming bool `rs:"resuming"`
	// Cached grapheme category and associated scalar value range.
	graphemeCatCache rs.Tuple3[uint32, uint32, tablesGraphemeGraphemeCat] `rs:"grapheme_cat_cache"`
}

var _ = rs.RegisterStruct[GraphemeCursor]("GraphemeCursor")

// GraphemeIncomplete is the crate's `GraphemeIncomplete`:
// an error return indicating that not enough content was available in the
// provided chunk to satisfy the query, and that more content must be provided.
//
// It is the error of the [GraphemeCursor] queries, one of
// [*GraphemeIncomplete_PreContext], [*GraphemeIncomplete_PrevChunk],
// [*GraphemeIncomplete_NextChunk] and [*GraphemeIncomplete_InvalidOffset].
// Rust gives the type no `Display`; Error returns its `Debug` form.
type GraphemeIncomplete interface {
	error
	isGraphemeIncomplete()
}

// GraphemeIncomplete_PreContext is `GraphemeIncomplete::PreContext(usize)`:
// more pre-context is needed. The caller should call `provide_context`
// with a chunk ending at the offset given, then retry the query. This
// will only be returned if the `chunk_start` parameter is nonzero.
type GraphemeIncomplete_PreContext struct{ F0 int }

// GraphemeIncomplete_PrevChunk is `GraphemeIncomplete::PrevChunk`:
// when requesting `prev_boundary`, the cursor is moving past the beginning
// of the current chunk, so the chunk before that is requested. This will
// only be returned if the `chunk_start` parameter is nonzero.
type GraphemeIncomplete_PrevChunk struct{}

// GraphemeIncomplete_NextChunk is `GraphemeIncomplete::NextChunk`:
// when requesting `next_boundary`, the cursor is moving past the end of the
// current chunk, so the chunk after that is requested. This will only be
// returned if the chunk ends before the `len` parameter provided on
// creation of the cursor.
type GraphemeIncomplete_NextChunk struct{} // requesting chunk following the one given

// GraphemeIncomplete_InvalidOffset is `GraphemeIncomplete::InvalidOffset`:
// an error returned when the chunk given does not contain the cursor position.
type GraphemeIncomplete_InvalidOffset struct{}

func (*GraphemeIncomplete_PreContext) isGraphemeIncomplete()    {}
func (*GraphemeIncomplete_PrevChunk) isGraphemeIncomplete()     {}
func (*GraphemeIncomplete_NextChunk) isGraphemeIncomplete()     {}
func (*GraphemeIncomplete_InvalidOffset) isGraphemeIncomplete() {}

func (e *GraphemeIncomplete_PreContext) Error() string {
	return "PreContext(" + strconv.Itoa(e.F0) + ")"
}
func (*GraphemeIncomplete_PrevChunk) Error() string     { return "PrevChunk" }
func (*GraphemeIncomplete_NextChunk) Error() string     { return "NextChunk" }
func (*GraphemeIncomplete_InvalidOffset) Error() string { return "InvalidOffset" }

var _ = rs.RegisterEnum[GraphemeIncomplete]("GraphemeIncomplete",
	rs.Variant[GraphemeIncomplete_PreContext]("PreContext", rs.ShapeTuple),
	rs.Variant[GraphemeIncomplete_PrevChunk]("PrevChunk", rs.ShapeUnit),
	rs.Variant[GraphemeIncomplete_NextChunk]("NextChunk", rs.ShapeUnit),
	rs.Variant[GraphemeIncomplete_InvalidOffset]("InvalidOffset", rs.ShapeUnit),
)

// graphemePairResult is the crate's `PairResult`:
// an enum describing the result from lookup of a pair of categories.
type graphemePairResult uint8

const (
	// definitely not a break
	graphemePairResult_NotBreak graphemePairResult = iota
	// definitely a break
	graphemePairResult_Break
	// a break iff not in extended mode
	graphemePairResult_Extended
	// a break unless in extended mode and preceded by
	// a sequence of 0 or more InCB=Extend and one or more
	// InCB = Linker (in any order),
	// preceded by another InCB=Consonant
	graphemePairResult_InCbConsonant
	// a break if preceded by an even number of RIS
	graphemePairResult_Regional
	// a break if preceded by emoji base and (Extend)*
	graphemePairResult_Emoji
)

func graphemeCheckPair(before tablesGraphemeGraphemeCat, after tablesGraphemeGraphemeCat) graphemePairResult {
	const (
		GC_CR                    = tablesGraphemeGraphemeCat_GcCr
		GC_LF                    = tablesGraphemeGraphemeCat_GcLf
		GC_Control               = tablesGraphemeGraphemeCat_GcControl
		GC_L                     = tablesGraphemeGraphemeCat_GcL
		GC_V                     = tablesGraphemeGraphemeCat_GcV
		GC_LV                    = tablesGraphemeGraphemeCat_GcLv
		GC_LVT                   = tablesGraphemeGraphemeCat_GcLvt
		GC_T                     = tablesGraphemeGraphemeCat_GcT
		GC_Extend                = tablesGraphemeGraphemeCat_GcExtend
		GC_ZWJ                   = tablesGraphemeGraphemeCat_GcZwj
		GC_SpacingMark           = tablesGraphemeGraphemeCat_GcSpacingMark
		GC_Prepend               = tablesGraphemeGraphemeCat_GcPrepend
		GC_InCB_Consonant        = tablesGraphemeGraphemeCat_GcInCBConsonant
		GC_Extended_Pictographic = tablesGraphemeGraphemeCat_GcExtendedPictographic
		GC_Regional_Indicator    = tablesGraphemeGraphemeCat_GcRegionalIndicator
	)
	// A Rust `match (before, after)`: the first case that holds wins.
	switch {
	case before == GC_CR && after == GC_LF:
		return graphemePairResult_NotBreak // GB3
	case before == GC_Control || before == GC_CR || before == GC_LF:
		return graphemePairResult_Break // GB4
	case after == GC_Control || after == GC_CR || after == GC_LF:
		return graphemePairResult_Break // GB5
	case before == GC_L && (after == GC_L || after == GC_V || after == GC_LV || after == GC_LVT):
		return graphemePairResult_NotBreak // GB6
	case (before == GC_LV || before == GC_V) && (after == GC_V || after == GC_T):
		return graphemePairResult_NotBreak // GB7
	case (before == GC_LVT || before == GC_T) && after == GC_T:
		return graphemePairResult_NotBreak // GB8
	case after == GC_Extend || after == GC_ZWJ:
		return graphemePairResult_NotBreak // GB9
	case after == GC_SpacingMark:
		return graphemePairResult_Extended // GB9a
	case before == GC_Prepend:
		return graphemePairResult_Extended // GB9b
	case after == GC_InCB_Consonant:
		return graphemePairResult_InCbConsonant // GB9c
	case before == GC_ZWJ && after == GC_Extended_Pictographic:
		return graphemePairResult_Emoji // GB11
	case before == GC_Regional_Indicator && after == GC_Regional_Indicator:
		return graphemePairResult_Regional // GB12, GB13
	default:
		return graphemePairResult_Break // GB999
	}
}

// GraphemeCursorNew is `GraphemeCursor::new`:
// create a new cursor. The string and initial offset are given at creation
// time, but the contents of the string are not. The `is_extended` parameter
// controls whether extended grapheme clusters are selected.
//
// The `offset` parameter must be on a codepoint boundary.
//
//	s := "हिन्दी"
//	legacy := unicodesegmentation.GraphemeCursorNew(0, len(s), false)
//	legacy.NextBoundary(s, 0) // Some(len("ह")), nil
//	extended := unicodesegmentation.GraphemeCursorNew(0, len(s), true)
//	extended.NextBoundary(s, 0) // Some(len("हि")), nil
func GraphemeCursorNew(offset int, len_ int, isExtended bool) *GraphemeCursor {
	var state graphemeGraphemeState
	if offset == 0 || offset == len_ {
		state = graphemeGraphemeState_Break
	} else {
		state = graphemeGraphemeState_Unknown
	}
	return &GraphemeCursor{
		offset:           offset,
		len:              len_,
		state:            state,
		isExtended:       isExtended,
		catBefore:        rs.None[tablesGraphemeGraphemeCat](),
		catAfter:         rs.None[tablesGraphemeGraphemeCat](),
		preContextOffset: rs.None[int](),
		incbLinkerCount:  rs.None[int](),
		risCount:         rs.None[int](),
		resuming:         false,
		graphemeCatCache: rs.T3(uint32(0), uint32(0), tablesGraphemeGraphemeCat_GcControl),
	}
}

// Clone is the derived `Clone`.
func (s *GraphemeCursor) Clone() GraphemeCursor {
	return *s
}

func (s *GraphemeCursor) graphemeCategory(ch rune) tablesGraphemeGraphemeCat {
	if ch <= 0x7e {
		// Special-case optimization for ascii, except U+007F.  This
		// improves performance even for many primarily non-ascii texts,
		// due to use of punctuation and white space characters from the
		// ascii range.
		if ch >= 0x20 {
			return tablesGraphemeGraphemeCat_GcAny
		} else if ch == '\n' {
			return tablesGraphemeGraphemeCat_GcLf
		} else if ch == '\r' {
			return tablesGraphemeGraphemeCat_GcCr
		} else {
			return tablesGraphemeGraphemeCat_GcControl
		}
	} else {
		// If this char isn't within the cached range, update the cache to the
		// range that includes it.
		if uint32(ch) < s.graphemeCatCache.F0 || uint32(ch) > s.graphemeCatCache.F1 {
			lower, upper, cat := tablesGraphemeGraphemeCategory(ch)
			s.graphemeCatCache = rs.T3(lower, upper, cat)
		}
		return s.graphemeCatCache.F2
	}
}

// Not sure I'm gonna keep this, the advantage over new() seems thin.

// SetCursor is `GraphemeCursor::set_cursor`:
// set the cursor to a new location in the same string.
//
//	s := "abcd"
//	cursor := unicodesegmentation.GraphemeCursorNew(0, len(s), false)
//	cursor.CurCursor() // 0
//	cursor.SetCursor(2)
//	cursor.CurCursor() // 2
func (s *GraphemeCursor) SetCursor(offset int) {
	if offset != s.offset {
		s.offset = offset
		if offset == 0 || offset == s.len {
			s.state = graphemeGraphemeState_Break
		} else {
			s.state = graphemeGraphemeState_Unknown
		}
		// reset state derived from text around cursor
		s.catBefore = rs.None[tablesGraphemeGraphemeCat]()
		s.catAfter = rs.None[tablesGraphemeGraphemeCat]()
		s.incbLinkerCount = rs.None[int]()
		s.risCount = rs.None[int]()
	}
}

// CurCursor is `GraphemeCursor::cur_cursor`:
// the current offset of the cursor. Equal to the last value provided to
// `new()` or `set_cursor()`, or returned from `next_boundary()` or
// `prev_boundary()`.
//
//	// Two flags (🇷🇸🇮🇴), each flag is two RIS codepoints, each RIS is 4 bytes.
//	flags := "\U0001F1F7\U0001F1F8\U0001F1EE\U0001F1F4"
//	cursor := unicodesegmentation.GraphemeCursorNew(4, len(flags), false)
//	cursor.CurCursor() // 4
//	cursor.NextBoundary(flags, 0) // Some(8), nil
//	cursor.CurCursor() // 8
func (s *GraphemeCursor) CurCursor() int {
	return s.offset
}

// ProvideContext is `GraphemeCursor::provide_context`:
// provide additional pre-context when it is needed to decide a boundary.
// The end of the chunk must coincide with the value given in the
// `GraphemeIncomplete::PreContext` request.
//
//	flags := "\U0001F1F7\U0001F1F8\U0001F1EE\U0001F1F4"
//	cursor := unicodesegmentation.GraphemeCursorNew(8, len(flags), false)
//	// Not enough pre-context to decide if there's a boundary between the two flags.
//	cursor.IsBoundary(flags[8:], 8) // false, &GraphemeIncomplete_PreContext{F0: 8}
//	// Provide one more Regional Indicator Symbol of pre-context
//	cursor.ProvideContext(flags[4:8], 4)
//	// Still not enough context to decide.
//	cursor.IsBoundary(flags[8:], 8) // false, &GraphemeIncomplete_PreContext{F0: 4}
//	// Provide additional requested context.
//	cursor.ProvideContext(flags[0:4], 0)
//	// That's enough to decide (it always is when context goes to the start of the string)
//	cursor.IsBoundary(flags[8:], 8) // true, nil
func (s *GraphemeCursor) ProvideContext(chunk string, chunkStart int) {
	if !(saturatingAdd(chunkStart, len(chunk)) == s.preContextOffset.Unwrap()) {
		rs.PanicAt(0, "assertion failed: chunk_start.saturating_add(chunk.len()) == self.pre_context_offset.unwrap()")
	}
	s.preContextOffset = rs.None[int]()
	if s.isExtended && chunkStart+len(chunk) == s.offset {
		ch, _, ok := strNextBackChar(chunk)
		if !ok {
			optionUnwrapNone()
		}
		if s.graphemeCategory(ch) == tablesGraphemeGraphemeCat_GcPrepend {
			s.decide(false) // GB9b
			return
		}
	}
	switch s.state.tag {
	case graphemeGraphemeStateTag_InCbConsonant:
		s.handleIncbConsonant(chunk, chunkStart)
	case graphemeGraphemeStateTag_Regional:
		s.handleRegional(chunk, chunkStart)
	case graphemeGraphemeStateTag_Emoji:
		s.handleEmoji(chunk, chunkStart, s.state.seenZwj)
	default:
		if s.catBefore.IsNone() && s.offset == len(chunk)+chunkStart {
			ch, _, ok := strNextBackChar(chunk)
			if !ok {
				optionUnwrapNone()
			}
			s.catBefore = rs.Some(s.graphemeCategory(ch))
		}
	}
}

func (s *GraphemeCursor) decide(isBreak bool) {
	if isBreak {
		s.state = graphemeGraphemeState_Break
	} else {
		s.state = graphemeGraphemeState_NotBreak
	}
}

func (s *GraphemeCursor) decision(isBreak bool) (bool, error) {
	s.decide(isBreak)
	return isBreak, nil
}

func (s *GraphemeCursor) isBoundaryResult() (bool, error) {
	if s.state == graphemeGraphemeState_Break {
		return true, nil
	} else if s.state == graphemeGraphemeState_NotBreak {
		return false, nil
	} else if preContextOffset, ok := s.preContextOffset.Get(); ok {
		return false, &GraphemeIncomplete_PreContext{F0: preContextOffset}
	} else {
		rs.PanicAt(0, "internal error: entered unreachable code: inconsistent state")
		return false, nil
	}
}

// For handling rule GB9c:
//
// There's an `InCB=Consonant` after this, and we need to look back
// to verify whether there should be a break.
//
// Seek backward to find an `InCB=Linker` preceded by an `InCB=Consonsnt`
// (potentially separated by some number of `InCB=Linker` or `InCB=Extend`).
// If we find the consonant in question, then there's no break; if we find a consonant
// with no linker, or a non-linker non-extend non-consonant, or the start of text, there's a break;
// otherwise we need more context
func (s *GraphemeCursor) handleIncbConsonant(chunk string, chunkStart int) {
	// GB9c only applies to extended grapheme clusters
	if !s.isExtended {
		s.decide(true)
		return
	}

	incbLinkerCount := s.incbLinkerCount.UnwrapOr(0)

	// for ch in chunk.chars().rev()
	for rest := chunk; rest != ""; {
		ch, size := utf8.DecodeLastRuneInString(rest)
		rest = rest[:len(rest)-size]
		if tablesIsIncbLinker(ch) {
			// We found an InCB linker
			incbLinkerCount += 1
			s.incbLinkerCount = rs.Some(incbLinkerCount)
		} else if tablesDerivedPropertyInCBExtend(ch) {
			// We ignore InCB extends, continue
		} else {
			// Prev character is neither linker nor extend, break suppressed iff it's InCB=Consonant
			result := !(s.incbLinkerCount.UnwrapOr(0) > 0 &&
				s.graphemeCategory(ch) == tablesGraphemeGraphemeCat_GcInCBConsonant)
			s.decide(result)
			return
		}
	}

	if chunkStart == 0 {
		// Start of text and we still haven't found a consonant, so break
		s.decide(true)
	} else {
		// We need more context
		s.preContextOffset = rs.Some(chunkStart)
		s.state = graphemeGraphemeState_InCbConsonant
	}
}

func (s *GraphemeCursor) handleRegional(chunk string, chunkStart int) {
	risCount := s.risCount.UnwrapOr(0)
	// for ch in chunk.chars().rev()
	for rest := chunk; rest != ""; {
		ch, size := utf8.DecodeLastRuneInString(rest)
		rest = rest[:len(rest)-size]
		if s.graphemeCategory(ch) != tablesGraphemeGraphemeCat_GcRegionalIndicator {
			s.risCount = rs.Some(risCount)
			s.decide(risCount%2 == 0)
			return
		}
		risCount += 1
	}
	s.risCount = rs.Some(risCount)
	if chunkStart == 0 {
		s.decide(risCount%2 == 0)
	} else {
		s.preContextOffset = rs.Some(chunkStart)
		s.state = graphemeGraphemeState_Regional
	}
}

func (s *GraphemeCursor) handleEmoji(chunk string, chunkStart int, seenZwj bool) {
	// \p{Extended_Pictographic} Extend* ZWJ 	× 	\p{Extended_Pictographic}
	iter := chunk // chunk.chars().rev()
	if !seenZwj {
		if ch, size, ok := strNextBackChar(iter); ok {
			iter = iter[:len(iter)-size]
			if s.graphemeCategory(ch) != tablesGraphemeGraphemeCat_GcZwj {
				s.decide(true)
				return
			} else {
				seenZwj = true
			}
		}
	}
	for iter != "" {
		ch, size := utf8.DecodeLastRuneInString(iter)
		iter = iter[:len(iter)-size]
		switch s.graphemeCategory(ch) {
		case tablesGraphemeGraphemeCat_GcExtend:
		case tablesGraphemeGraphemeCat_GcExtendedPictographic:
			s.decide(false)
			return
		default:
			s.decide(true)
			return
		}
	}
	if chunkStart == 0 {
		s.decide(true)
	} else {
		s.preContextOffset = rs.Some(chunkStart)
		s.state = graphemeGraphemeState_Emoji(seenZwj)
	}
}

// IsBoundary is `GraphemeCursor::is_boundary`:
// determine whether the current cursor location is a grapheme cluster boundary.
// Only a part of the string need be supplied. If `chunk_start` is nonzero or
// the length of `chunk` is not equal to `len` on creation, then this method
// may return `GraphemeIncomplete::PreContext`. The caller should then
// call `provide_context` with the requested chunk, then retry calling this
// method.
//
// For partial chunks, if the cursor is not at the beginning or end of the
// string, the chunk should contain at least the codepoint following the cursor.
// If the string is nonempty, the chunk must be nonempty.
//
// All calls should have consistent chunk contents (ie, if a chunk provides
// content for a given slice, all further chunks covering that slice must have
// the same content for it).
//
//	flags := "\U0001F1F7\U0001F1F8\U0001F1EE\U0001F1F4"
//	cursor := unicodesegmentation.GraphemeCursorNew(8, len(flags), false)
//	cursor.IsBoundary(flags, 0) // true, nil
//	cursor.SetCursor(12)
//	cursor.IsBoundary(flags, 0) // false, nil
//
// The error, when there is one, is a [GraphemeIncomplete].
func (s *GraphemeCursor) IsBoundary(chunk string, chunkStart int) (bool, error) {
	if s.state == graphemeGraphemeState_Break {
		return true, nil
	}
	if s.state == graphemeGraphemeState_NotBreak {
		return false, nil
	}
	if (s.offset < chunkStart || s.offset >= saturatingAdd(chunkStart, len(chunk))) &&
		(s.offset > saturatingAdd(chunkStart, len(chunk)) || s.catAfter.IsNone()) {
		return false, &GraphemeIncomplete_InvalidOffset{}
	}
	if preContextOffset, ok := s.preContextOffset.Get(); ok {
		return false, &GraphemeIncomplete_PreContext{F0: preContextOffset}
	}
	offsetInChunk := saturatingSub(s.offset, chunkStart)
	if s.catAfter.IsNone() {
		ch, _, ok := strNextChar(strs.SliceFrom(chunk, offsetInChunk))
		if !ok {
			optionUnwrapNone()
		}
		s.catAfter = rs.Some(s.graphemeCategory(ch))
	}
	if s.offset == chunkStart {
		needPreContext := true
		switch s.catAfter.Unwrap() {
		case tablesGraphemeGraphemeCat_GcInCBConsonant:
			s.state = graphemeGraphemeState_InCbConsonant
		case tablesGraphemeGraphemeCat_GcRegionalIndicator:
			s.state = graphemeGraphemeState_Regional
		case tablesGraphemeGraphemeCat_GcExtendedPictographic:
			s.state = graphemeGraphemeState_Emoji(false)
		default:
			needPreContext = s.catBefore.IsNone()
		}
		if needPreContext {
			s.preContextOffset = rs.Some(chunkStart)
			return false, &GraphemeIncomplete_PreContext{F0: chunkStart}
		}
	}
	if s.catBefore.IsNone() {
		ch, _, ok := strNextBackChar(strs.SliceTo(chunk, offsetInChunk))
		if !ok {
			optionUnwrapNone()
		}
		s.catBefore = rs.Some(s.graphemeCategory(ch))
	}
	switch graphemeCheckPair(s.catBefore.Unwrap(), s.catAfter.Unwrap()) {
	case graphemePairResult_NotBreak:
		return s.decision(false)
	case graphemePairResult_Break:
		return s.decision(true)
	case graphemePairResult_Extended:
		isExtended := s.isExtended
		return s.decision(!isExtended)
	case graphemePairResult_InCbConsonant:
		s.handleIncbConsonant(strs.SliceTo(chunk, offsetInChunk), chunkStart)
		return s.isBoundaryResult()
	case graphemePairResult_Regional:
		if risCount, ok := s.risCount.Get(); ok {
			return s.decision((risCount % 2) == 0)
		}
		s.handleRegional(strs.SliceTo(chunk, offsetInChunk), chunkStart)
		return s.isBoundaryResult()
	case graphemePairResult_Emoji:
		s.handleEmoji(strs.SliceTo(chunk, offsetInChunk), chunkStart, false)
		return s.isBoundaryResult()
	default:
		panic(rs.Unreachable())
	}
}

// NextBoundary is `GraphemeCursor::next_boundary`:
// find the next boundary after the current cursor position. Only a part of
// the string need be supplied. If the chunk is incomplete, then this
// method might return `GraphemeIncomplete::PreContext` or
// `GraphemeIncomplete::NextChunk`. In the former case, the caller should
// call `provide_context` with the requested chunk, then retry. In the
// latter case, the caller should provide the chunk following the one
// given, then retry.
//
// See `is_boundary` for expectations on the provided chunk.
//
//	flags := "\U0001F1F7\U0001F1F8\U0001F1EE\U0001F1F4"
//	cursor := unicodesegmentation.GraphemeCursorNew(4, len(flags), false)
//	cursor.NextBoundary(flags, 0) // Some(8), nil
//	cursor.NextBoundary(flags, 0) // Some(16), nil
//	cursor.NextBoundary(flags, 0) // None, nil
//
// And an example that uses partial strings:
//
//	s := "abcd"
//	cursor := unicodesegmentation.GraphemeCursorNew(0, len(s), false)
//	cursor.NextBoundary(s[:2], 0)  // Some(1), nil
//	cursor.NextBoundary(s[:2], 0)  // None, &GraphemeIncomplete_NextChunk{}
//	cursor.NextBoundary(s[2:4], 2) // Some(2), nil
//	cursor.NextBoundary(s[2:4], 2) // Some(3), nil
//	cursor.NextBoundary(s[2:4], 2) // Some(4), nil
//	cursor.NextBoundary(s[2:4], 2) // None, nil
//
// The error, when there is one, is a [GraphemeIncomplete].
func (s *GraphemeCursor) NextBoundary(chunk string, chunkStart int) (rs.Option[int], error) {
	if s.offset == s.len {
		return rs.None[int](), nil
	}
	iter := strs.SliceFrom(chunk, saturatingSub(s.offset, chunkStart)) // .chars()
	ch, size, ok := strNextChar(iter)
	if !ok {
		return rs.None[int](), &GraphemeIncomplete_NextChunk{}
	}
	iter = iter[size:]
	for {
		if s.resuming {
			if s.catAfter.IsNone() {
				s.catAfter = rs.Some(s.graphemeCategory(ch))
			}
		} else {
			s.offset = saturatingAdd(s.offset, size) // ch.len_utf8()
			s.state = graphemeGraphemeState_Unknown
			s.catBefore = s.catAfter.Take()
			if s.catBefore.IsNone() {
				s.catBefore = rs.Some(s.graphemeCategory(ch))
			}
			if tablesIsIncbLinker(ch) {
				// Some(self.incb_linker_count.map_or(1, |c| c + 1))
				if c, ok := s.incbLinkerCount.Get(); ok {
					s.incbLinkerCount = rs.Some(c + 1)
				} else {
					s.incbLinkerCount = rs.Some(1)
				}
			} else if !tablesDerivedPropertyInCBExtend(ch) {
				s.incbLinkerCount = rs.Some(0)
			}
			if s.catBefore.Unwrap() == tablesGraphemeGraphemeCat_GcRegionalIndicator {
				// self.ris_count.map(|c| c + 1)
				if c, ok := s.risCount.Get(); ok {
					s.risCount = rs.Some(c + 1)
				}
			} else {
				s.risCount = rs.Some(0)
			}
			if nextCh, nextSize, ok := strNextChar(iter); ok {
				iter = iter[nextSize:]
				ch, size = nextCh, nextSize
				s.catAfter = rs.Some(s.graphemeCategory(ch))
			} else if s.offset == s.len {
				s.decide(true)
			} else {
				s.resuming = true
				return rs.None[int](), &GraphemeIncomplete_NextChunk{}
			}
		}
		s.resuming = true
		isBoundary, err := s.IsBoundary(chunk, chunkStart)
		if err != nil {
			return rs.None[int](), err
		}
		if isBoundary {
			s.resuming = false
			return rs.Some(s.offset), nil
		}
		s.resuming = false
	}
}

// PrevBoundary is `GraphemeCursor::prev_boundary`:
// find the previous boundary after the current cursor position. Only a part
// of the string need be supplied. If the chunk is incomplete, then this
// method might return `GraphemeIncomplete::PreContext` or
// `GraphemeIncomplete::PrevChunk`. In the former case, the caller should
// call `provide_context` with the requested chunk, then retry. In the
// latter case, the caller should provide the chunk preceding the one
// given, then retry.
//
// See `is_boundary` for expectations on the provided chunk.
//
//	flags := "\U0001F1F7\U0001F1F8\U0001F1EE\U0001F1F4"
//	cursor := unicodesegmentation.GraphemeCursorNew(12, len(flags), false)
//	cursor.PrevBoundary(flags, 0) // Some(8), nil
//	cursor.PrevBoundary(flags, 0) // Some(0), nil
//	cursor.PrevBoundary(flags, 0) // None, nil
//
// And an example that uses partial strings (note the exact return is not
// guaranteed, and may be `PrevChunk` or `PreContext` arbitrarily):
//
//	s := "abcd"
//	cursor := unicodesegmentation.GraphemeCursorNew(4, len(s), false)
//	cursor.PrevBoundary(s[2:4], 2) // Some(3), nil
//	cursor.PrevBoundary(s[2:4], 2) // None, &GraphemeIncomplete_PrevChunk{}
//	cursor.PrevBoundary(s[0:2], 0) // Some(2), nil
//	cursor.PrevBoundary(s[0:2], 0) // Some(1), nil
//	cursor.PrevBoundary(s[0:2], 0) // Some(0), nil
//	cursor.PrevBoundary(s[0:2], 0) // None, nil
//
// The error, when there is one, is a [GraphemeIncomplete].
func (s *GraphemeCursor) PrevBoundary(chunk string, chunkStart int) (rs.Option[int], error) {
	if s.offset == 0 {
		return rs.None[int](), nil
	}
	if s.offset == chunkStart {
		return rs.None[int](), &GraphemeIncomplete_PrevChunk{}
	}
	iter := strs.SliceTo(chunk, saturatingSub(s.offset, chunkStart)) // .chars().rev()
	ch, size, ok := strNextBackChar(iter)
	if !ok {
		optionUnwrapNone()
	}
	iter = iter[:len(iter)-size]
	for {
		if s.offset == chunkStart {
			s.resuming = true
			return rs.None[int](), &GraphemeIncomplete_PrevChunk{}
		}
		if s.resuming {
			s.catBefore = rs.Some(s.graphemeCategory(ch))
		} else {
			s.offset -= size // ch.len_utf8()
			s.catAfter = s.catBefore.Take()
			s.state = graphemeGraphemeState_Unknown
			if incbLinkerCount, ok := s.incbLinkerCount.Get(); ok {
				if incbLinkerCount > 0 && tablesIsIncbLinker(ch) {
					s.incbLinkerCount = rs.Some(incbLinkerCount - 1)
				} else if tablesDerivedPropertyInCBExtend(ch) {
					s.incbLinkerCount = rs.Some(incbLinkerCount)
				} else {
					s.incbLinkerCount = rs.None[int]()
				}
			}
			if risCount, ok := s.risCount.Get(); ok {
				if risCount > 0 {
					s.risCount = rs.Some(risCount - 1)
				} else {
					s.risCount = rs.None[int]()
				}
			}
			if prevCh, prevSize, ok := strNextBackChar(iter); ok {
				iter = iter[:len(iter)-prevSize]
				ch, size = prevCh, prevSize
				s.catBefore = rs.Some(s.graphemeCategory(ch))
			} else if s.offset == 0 {
				s.decide(true)
			} else {
				s.resuming = true
				s.catAfter = rs.Some(s.graphemeCategory(ch))
				return rs.None[int](), &GraphemeIncomplete_PrevChunk{}
			}
		}
		s.resuming = true
		isBoundary, err := s.IsBoundary(chunk, chunkStart)
		if err != nil {
			return rs.None[int](), err
		}
		if isBoundary {
			s.resuming = false
			return rs.Some(s.offset), nil
		}
		s.resuming = false
	}
}

// The helpers below stand in for parts of Rust's standard library that the
// crate's code uses and that go/rs does not provide yet.

// saturatingAdd is `usize::saturating_add` for two offsets.
func saturatingAdd(a, b int) int {
	if sum := a + b; sum >= a {
		return sum
	}
	return rs.UsizeMax
}

// saturatingSub is `usize::saturating_sub` for two offsets.
func saturatingSub(a, b int) int {
	if a > b {
		return a - b
	}
	return 0
}

// optionUnwrapNone panics as `Option::unwrap` does on `None`, where the Go
// code holds the option as a comma-ok pair.
func optionUnwrapNone() {
	rs.PanicAt(1, "called `Option::unwrap()` on a `None` value")
}
