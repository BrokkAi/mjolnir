// Port of core 1.99.1 src/str/pattern.rs

package strs

import (
	"slices"
	"strings"
)

// Rust's `Pattern` trait lets one method take a `&str`, a `char`, a set of
// chars or a predicate. Go has no overloading, so every such method here
// comes in four spellings (Split, SplitChar, SplitChars, SplitFn), and each
// builds the searcher Rust would have built: strSearcher, charSearcher, or
// multiCharEqSearcher.

// searchStepKind is the discriminant of pattern::SearchStep.
type searchStepKind uint8

const (
	// stepMatch expresses that a match of the pattern has been found at
	// haystack[a..b].
	stepMatch searchStepKind = iota
	// stepReject expresses that haystack[a..b] has been rejected as a
	// possible match of the pattern.
	stepReject
	// stepDone expresses that every byte of the haystack has been visited,
	// ending the iteration.
	stepDone
)

// searchStep is pattern::SearchStep.
type searchStep struct {
	kind searchStepKind
	a, b int
}

// searcher is pattern::Searcher together with pattern::ReverseSearcher: every
// pattern Mjolnir uses can be searched from both ends.
type searcher interface {
	next() searchStep
	nextMatch() (a, b int, ok bool)
	nextReject() (a, b int, ok bool)
	nextBack() searchStep
	nextMatchBack() (a, b int, ok bool)
	nextRejectBack() (a, b int, ok bool)
}

// The four functions below are the default methods of Searcher and
// ReverseSearcher.

func searcherNextMatch(next func() searchStep) (int, int, bool) {
	for {
		switch step := next(); step.kind {
		case stepMatch:
			return step.a, step.b, true
		case stepDone:
			return 0, 0, false
		}
	}
}

func searcherNextReject(next func() searchStep) (int, int, bool) {
	for {
		switch step := next(); step.kind {
		case stepReject:
			return step.a, step.b, true
		case stepDone:
			return 0, 0, false
		}
	}
}

/////////////////////////////////////////////////////////////////////////////
// Impl for char
/////////////////////////////////////////////////////////////////////////////

// charSearcher is pattern::CharSearcher.
type charSearcher struct {
	haystack string
	// `finger` is the current byte index of the forward search.
	// Imagine that it exists before the byte at its index, i.e.
	// `haystack[finger]` is the first byte of the slice we must inspect
	// during forward searching
	finger int
	// `fingerBack` is the current byte index of the reverse search.
	// Imagine that it exists after the byte at its index, i.e.
	// haystack[fingerBack - 1] is the last byte of the slice we must inspect
	// during forward searching (and thus the first byte to be inspected when
	// calling nextBack()).
	fingerBack int
	needle     rune
	// utf8Encoded is the needle encoded as UTF-8 (utf8_size bytes).
	utf8Encoded string
}

func newCharSearcher(haystack string, needle rune) *charSearcher {
	return &charSearcher{
		haystack:    haystack,
		finger:      0,
		fingerBack:  len(haystack),
		needle:      needle,
		utf8Encoded: CharEncodeUtf8(needle),
	}
}

func (s *charSearcher) next() searchStep {
	oldFinger := s.finger
	slice := s.haystack[oldFinger:s.fingerBack]
	if len(slice) > 0 {
		ch, size := nextCodePoint(slice)
		// add byte offset of current character
		// without re-encoding as utf-8
		s.finger += size
		if ch == s.needle {
			return searchStep{stepMatch, oldFinger, s.finger}
		}
		return searchStep{stepReject, oldFinger, s.finger}
	}
	return searchStep{kind: stepDone}
}

func (s *charSearcher) nextMatch() (int, int, bool) {
	utf8Size := len(s.utf8Encoded)
	for {
		// get the haystack after the last character found
		if s.finger > s.fingerBack {
			return 0, 0, false
		}
		bytes := s.haystack[s.finger:s.fingerBack]
		// the last byte of the utf8 encoded needle
		lastByte := s.utf8Encoded[utf8Size-1]
		index := strings.IndexByte(bytes, lastByte)
		if index < 0 {
			// found nothing, exit
			s.finger = s.fingerBack
			return 0, 0, false
		}
		// The new finger is the index of the byte we found,
		// plus one, since we memchr'd for the last byte of the character.
		//
		// Note that this doesn't always give us a finger on a UTF8 boundary.
		// If we *didn't* find our character
		// we may have indexed to the non-last byte of a 3-byte or 4-byte character.
		// We can't just skip to the next valid starting byte because a character like
		// ꁁ (U+A041 YI SYLLABLE PA), utf-8 `EA 81 81` will have us always find
		// the second byte when searching for the third.
		//
		// However, this is totally okay. While we have the invariant that
		// s.finger is on a UTF8 boundary, this invariant is not relied upon
		// within this method (it is relied upon in charSearcher.next()).
		//
		// We only exit this method when we reach the end of the string, or if we
		// find something. When we find something the `finger` will be set
		// to a UTF8 boundary.
		s.finger += index + 1
		if s.finger >= utf8Size {
			foundChar := s.finger - utf8Size
			if s.haystack[foundChar:s.finger] == s.utf8Encoded {
				return foundChar, s.finger, true
			}
		}
	}
}

// nextReject uses the default implementation from the Searcher trait.
func (s *charSearcher) nextReject() (int, int, bool) { return searcherNextReject(s.next) }

func (s *charSearcher) nextBack() searchStep {
	oldFinger := s.fingerBack
	slice := s.haystack[s.finger:oldFinger]
	if len(slice) > 0 {
		ch, size := nextCodePointReverse(slice)
		// subtract byte offset of current character
		// without re-encoding as utf-8
		s.fingerBack -= size
		if ch == s.needle {
			return searchStep{stepMatch, s.fingerBack, oldFinger}
		}
		return searchStep{stepReject, s.fingerBack, oldFinger}
	}
	return searchStep{kind: stepDone}
}

func (s *charSearcher) nextMatchBack() (int, int, bool) {
	haystack := s.haystack
	utf8Size := len(s.utf8Encoded)
	for {
		// get the haystack up to but not including the last character searched
		if s.finger > s.fingerBack {
			return 0, 0, false
		}
		bytes := haystack[s.finger:s.fingerBack]
		// the last byte of the utf8 encoded needle
		lastByte := s.utf8Encoded[utf8Size-1]
		index := strings.LastIndexByte(bytes, lastByte)
		if index < 0 {
			s.fingerBack = s.finger
			// found nothing, exit
			return 0, 0, false
		}
		// we searched a slice that was offset by s.finger,
		// add s.finger to recoup the original index
		index += s.finger
		// memrchr will return the index of the byte we wish to
		// find. In case of an ASCII character, this is indeed
		// were we wish our new finger to be ("after" the found
		// char in the paradigm of reverse iteration). For
		// multibyte chars we need to skip down by the number of more
		// bytes they have than ASCII
		shift := utf8Size - 1
		if index >= shift {
			foundChar := index - shift
			if foundChar+utf8Size <= len(haystack) && haystack[foundChar:foundChar+utf8Size] == s.utf8Encoded {
				// move finger to before the character found (i.e., at its start index)
				s.fingerBack = foundChar
				return s.fingerBack, s.fingerBack + utf8Size, true
			}
		}
		// We can't use fingerBack = index - size + 1 here. If we found the last char
		// of a different-sized character (or the middle byte of a different character)
		// we need to bump the fingerBack down to `index`. This similarly makes
		// `fingerBack` have the potential to no longer be on a boundary,
		// but this is OK since we only exit this function on a boundary
		// or when the haystack has been searched completely.
		//
		// Unlike nextMatch this does not
		// have the problem of repeated bytes in utf-8 because
		// we're searching for the last byte, and we can only have
		// found the last byte when searching in reverse.
		s.fingerBack = index
	}
}

// nextRejectBack uses the default implementation from the ReverseSearcher
// trait.
func (s *charSearcher) nextRejectBack() (int, int, bool) { return searcherNextReject(s.nextBack) }

// charIsContainedIn is `<char as Pattern>::is_contained_in`.
func charIsContainedIn(c rune, haystack string) bool {
	if uint32(c) < 128 {
		return strings.IndexByte(haystack, byte(c)) >= 0
	}
	return strIsContainedIn(CharEncodeUtf8(c), haystack)
}

/////////////////////////////////////////////////////////////////////////////
// Impl for a MultiCharEq wrapper
/////////////////////////////////////////////////////////////////////////////

// multiCharEqSearcher is pattern::MultiCharEqSearcher: the searcher of a
// predicate (`FnMut(char) -> bool`) and of a set of chars (`[char; N]`,
// `&[char]`). The predicate is called once for each char visited, in the
// order the search visits them.
type multiCharEqSearcher struct {
	charEq   func(rune) bool
	haystack string
	// front and back bound what the Rust CharIndices iterator has left.
	front, back int
}

func newFnSearcher(haystack string, charEq func(rune) bool) *multiCharEqSearcher {
	return &multiCharEqSearcher{charEq: charEq, haystack: haystack, front: 0, back: len(haystack)}
}

// newCharsSearcher is the searcher of `[char; N]` and `&[char]`: a char
// matches when the set contains it.
func newCharsSearcher(haystack string, set []rune) *multiCharEqSearcher {
	return newFnSearcher(haystack, func(c rune) bool { return slices.Contains(set, c) })
}

func (s *multiCharEqSearcher) next() searchStep {
	if s.front < s.back {
		c, charLen := nextCodePoint(s.haystack[s.front:s.back])
		i := s.front
		s.front += charLen
		if s.charEq(c) {
			return searchStep{stepMatch, i, i + charLen}
		}
		return searchStep{stepReject, i, i + charLen}
	}
	return searchStep{kind: stepDone}
}

func (s *multiCharEqSearcher) nextBack() searchStep {
	if s.front < s.back {
		c, charLen := nextCodePointReverse(s.haystack[s.front:s.back])
		s.back -= charLen
		i := s.back
		if s.charEq(c) {
			return searchStep{stepMatch, i, i + charLen}
		}
		return searchStep{stepReject, i, i + charLen}
	}
	return searchStep{kind: stepDone}
}

func (s *multiCharEqSearcher) nextMatch() (int, int, bool)  { return searcherNextMatch(s.next) }
func (s *multiCharEqSearcher) nextReject() (int, int, bool) { return searcherNextReject(s.next) }
func (s *multiCharEqSearcher) nextMatchBack() (int, int, bool) {
	return searcherNextMatch(s.nextBack)
}
func (s *multiCharEqSearcher) nextRejectBack() (int, int, bool) {
	return searcherNextReject(s.nextBack)
}

/////////////////////////////////////////////////////////////////////////////
// Impl for &str
/////////////////////////////////////////////////////////////////////////////

// strIsContainedIn is `<&str as Pattern>::is_contained_in`. (Rust picks among
// several search routines by length; they all answer whether the needle
// occurs.)
func strIsContainedIn(needle, haystack string) bool {
	if needle == "" {
		return true
	}
	if len(needle) < len(haystack) {
		if len(needle) == 1 {
			return strings.IndexByte(haystack, needle[0]) >= 0
		}
		_, _, found := newStrSearcher(haystack, needle).nextMatch()
		return found
	}
	return needle == haystack
}

/////////////////////////////////////////////////////////////////////////////
// Two Way substring searcher
/////////////////////////////////////////////////////////////////////////////

// strSearcher is pattern::StrSearcher.
type strSearcher struct {
	haystack string
	needle   string

	// Exactly one of the two is in use: empty for an empty needle.
	isEmpty bool
	empty   emptyNeedle
	twoWay  twoWaySearcher
}

// emptyNeedle is pattern::EmptyNeedle.
type emptyNeedle struct {
	position  int
	end       int
	isMatchFw bool
	isMatchBw bool
	// Needed in case of an empty haystack, see #85462
	isFinished bool
}

func newStrSearcher(haystack, needle string) *strSearcher {
	if needle == "" {
		return &strSearcher{
			haystack: haystack,
			needle:   needle,
			isEmpty:  true,
			empty: emptyNeedle{
				position:   0,
				end:        len(haystack),
				isMatchFw:  true,
				isMatchBw:  true,
				isFinished: false,
			},
		}
	}
	return &strSearcher{
		haystack: haystack,
		needle:   needle,
		twoWay:   newTwoWaySearcher(needle, len(haystack)),
	}
}

func (s *strSearcher) next() searchStep {
	if s.isEmpty {
		searcher := &s.empty
		if searcher.isFinished {
			return searchStep{kind: stepDone}
		}
		// empty needle rejects every char and matches every empty string between them
		isMatch := searcher.isMatchFw
		searcher.isMatchFw = !searcher.isMatchFw
		pos := searcher.position
		switch {
		case isMatch:
			return searchStep{stepMatch, pos, pos}
		case pos >= len(s.haystack):
			searcher.isFinished = true
			return searchStep{kind: stepDone}
		default:
			_, size := nextCodePoint(s.haystack[pos:])
			searcher.position += size
			return searchStep{stepReject, pos, searcher.position}
		}
	}

	searcher := &s.twoWay
	// twoWaySearcher produces valid *Match* indices that split at char boundaries
	// as long as it does correct matching and that haystack and needle are
	// valid UTF-8
	// *Rejects* from the algorithm can fall on any indices, but we will walk them
	// manually to the next character boundary, so that they are utf-8 safe.
	if searcher.position == len(s.haystack) {
		return searchStep{kind: stepDone}
	}
	step := searcher.next(s.haystack, s.needle, searcher.longPeriod, true)
	if step.kind == stepReject {
		// skip to next char boundary
		b := step.b
		for !IsCharBoundary(s.haystack, b) {
			b++
		}
		searcher.position = max(b, searcher.position)
		return searchStep{stepReject, step.a, b}
	}
	return step
}

func (s *strSearcher) nextMatch() (int, int, bool) {
	if s.isEmpty {
		return searcherNextMatch(s.next)
	}
	searcher := &s.twoWay
	step := searcher.next(s.haystack, s.needle, searcher.longPeriod, false)
	return step.a, step.b, step.kind == stepMatch
}

func (s *strSearcher) nextReject() (int, int, bool) { return searcherNextReject(s.next) }

func (s *strSearcher) nextBack() searchStep {
	if s.isEmpty {
		searcher := &s.empty
		if searcher.isFinished {
			return searchStep{kind: stepDone}
		}
		isMatch := searcher.isMatchBw
		searcher.isMatchBw = !searcher.isMatchBw
		end := searcher.end
		switch {
		case isMatch:
			return searchStep{stepMatch, end, end}
		case end <= 0:
			searcher.isFinished = true
			return searchStep{kind: stepDone}
		default:
			_, size := nextCodePointReverse(s.haystack[:end])
			searcher.end -= size
			return searchStep{stepReject, searcher.end, end}
		}
	}

	searcher := &s.twoWay
	if searcher.end == 0 {
		return searchStep{kind: stepDone}
	}
	step := searcher.nextBack(s.haystack, s.needle, searcher.longPeriod, true)
	if step.kind == stepReject {
		// skip to next char boundary
		a := step.a
		for !IsCharBoundary(s.haystack, a) {
			a--
		}
		searcher.end = min(a, searcher.end)
		return searchStep{stepReject, a, step.b}
	}
	return step
}

func (s *strSearcher) nextMatchBack() (int, int, bool) {
	if s.isEmpty {
		return searcherNextMatch(s.nextBack)
	}
	searcher := &s.twoWay
	step := searcher.nextBack(s.haystack, s.needle, searcher.longPeriod, false)
	return step.a, step.b, step.kind == stepMatch
}

func (s *strSearcher) nextRejectBack() (int, int, bool) { return searcherNextReject(s.nextBack) }

// twoWaySearcher is pattern::TwoWaySearcher: the internal state of the
// two-way substring search algorithm of Crochemore and Perrin (1991), which
// the Rust source explains at length.
type twoWaySearcher struct {
	// constants
	// critical factorization index
	critPos int
	// critical factorization index for reversed needle
	critPosBack int
	period      int
	// `byteset` is an extension (not part of the two way algorithm);
	// it's a 64-bit "fingerprint" where each set bit `j` corresponds
	// to a (byte & 63) == j present in the needle.
	byteset uint64

	// variables
	position int
	end      int
	// index into needle before which we have already matched
	memory int
	// index into needle after which we have already matched
	memoryBack int
	// longPeriod is Rust's `memory == usize::MAX`: the period is an
	// approximation and memory is not used.
	longPeriod bool
}

func newTwoWaySearcher(needle string, end int) twoWaySearcher {
	critPosFalse, periodFalse := twoWayMaximalSuffix(needle, false)
	critPosTrue, periodTrue := twoWayMaximalSuffix(needle, true)

	var critPos, period int
	if critPosFalse > critPosTrue {
		critPos, period = critPosFalse, periodFalse
	} else {
		critPos, period = critPosTrue, periodTrue
	}

	// A particularly readable explanation of what's going on here can be found
	// in Crochemore and Rytter's book "Text Algorithms", ch 13. Specifically
	// see the code for "Algorithm CP" on p. 323.
	//
	// What's going on is we have some critical factorization (u, v) of the
	// needle, and we want to determine whether u is a suffix of
	// &v[..period]. If it is, we use "Algorithm CP1". Otherwise we use
	// "Algorithm CP2", which is optimized for when the period of the needle
	// is large.
	if period+critPos <= len(needle) && needle[:critPos] == needle[period:period+critPos] {
		// short period case -- the period is exact
		// compute a separate critical factorization for the reversed needle
		// x = u' v' where |v'| < period(x).
		//
		// This is sped up by the period being known already.
		// Note that a case like x = "acba" may be factored exactly forwards
		// (crit_pos = 1, period = 3) while being factored with approximate
		// period in reverse (crit_pos = 2, period = 2). We use the given
		// reverse factorization but keep the exact period.
		critPosBack := len(needle) - max(
			twoWayReverseMaximalSuffix(needle, period, false),
			twoWayReverseMaximalSuffix(needle, period, true),
		)

		return twoWaySearcher{
			critPos:     critPos,
			critPosBack: critPosBack,
			period:      period,
			byteset:     twoWayBytesetCreate(needle[:period]),

			position:   0,
			end:        end,
			memory:     0,
			memoryBack: len(needle),
		}
	}
	// long period case -- we have an approximation to the actual period,
	// and don't use memorization.
	//
	// Approximate the period by lower bound max(|u|, |v|) + 1.
	// The critical factorization is efficient to use for both forward and
	// reverse search.
	return twoWaySearcher{
		critPos:     critPos,
		critPosBack: critPos,
		period:      max(critPos, len(needle)-critPos) + 1,
		byteset:     twoWayBytesetCreate(needle),

		position:   0,
		end:        end,
		longPeriod: true,
	}
}

func twoWayBytesetCreate(bytes string) uint64 {
	var set uint64
	for i := 0; i < len(bytes); i++ {
		set |= 1 << (bytes[i] & 0x3f)
	}
	return set
}

func (t *twoWaySearcher) bytesetContains(b byte) bool {
	return (t.byteset>>(b&0x3f))&1 != 0
}

// next is TwoWaySearcher::next. earlyReject selects the strategy:
// RejectAndMatch (true) reports each stretch without a match as a Reject,
// MatchOnly (false) skips to the next match and reports the end of the
// haystack as a Reject, which its caller reads as None.
//
// One of the main ideas of Two-Way is that we factorize the needle into
// two halves, (u, v), and begin trying to find v in the haystack by scanning
// left to right. If v matches, we try to match u by scanning right to left.
// How far we can jump when we encounter a mismatch is all based on the fact
// that (u, v) is a critical factorization for the needle.
func (t *twoWaySearcher) next(haystack, needle string, longPeriod bool, earlyReject bool) searchStep {
	// `next()` uses `t.position` as its cursor
	oldPos := t.position
	needleLast := len(needle) - 1
search:
	for {
		// Check that we have room to search in
		// position + needle_last can not overflow if we assume slices
		// are bounded by isize's range.
		if t.position+needleLast >= len(haystack) {
			t.position = len(haystack)
			return searchStep{stepReject, oldPos, t.position}
		}
		tailByte := haystack[t.position+needleLast]

		if earlyReject && oldPos != t.position {
			return searchStep{stepReject, oldPos, t.position}
		}

		// Quickly skip by large portions unrelated to our substring
		if !t.bytesetContains(tailByte) {
			t.position += len(needle)
			if !longPeriod {
				t.memory = 0
			}
			continue search
		}

		// See if the right part of the needle matches
		start := t.critPos
		if !longPeriod {
			start = max(t.critPos, t.memory)
		}
		for i := start; i < len(needle); i++ {
			if needle[i] != haystack[t.position+i] {
				t.position += i - t.critPos + 1
				if !longPeriod {
					t.memory = 0
				}
				continue search
			}
		}

		// See if the left part of the needle matches
		start = 0
		if !longPeriod {
			start = t.memory
		}
		for i := t.critPos - 1; i >= start; i-- {
			if needle[i] != haystack[t.position+i] {
				t.position += t.period
				if !longPeriod {
					t.memory = len(needle) - t.period
				}
				continue search
			}
		}

		// We have found a match!
		matchPos := t.position

		// Note: add t.period instead of len(needle) to have overlapping matches
		t.position += len(needle)
		if !longPeriod {
			t.memory = 0 // set to len(needle) - t.period for overlapping matches
		}

		return searchStep{stepMatch, matchPos, matchPos + len(needle)}
	}
}

// nextBack is TwoWaySearcher::next_back. It follows the ideas in `next()`.
//
// The definitions are symmetrical, with period(x) = period(reverse(x))
// and local_period(u, v) = local_period(reverse(v), reverse(u)), so if (u, v)
// is a critical factorization, so is (reverse(v), reverse(u)).
//
// For the reverse case we have computed a critical factorization x = u' v'
// (field `critPosBack`). We need |u| < period(x) for the forward case and
// thus |v'| < period(x) for the reverse.
//
// To search in reverse through the haystack, we search forward through
// a reversed haystack with a reversed needle, matching first u' and then v'.
func (t *twoWaySearcher) nextBack(haystack, needle string, longPeriod bool, earlyReject bool) searchStep {
	// `nextBack()` uses `t.end` as its cursor -- so that `next()` and `nextBack()`
	// are independent.
	oldEnd := t.end
search:
	for {
		// Check that we have room to search in
		// end - len(needle) will wrap around when there is no more room,
		// but due to slice length limits it can never wrap all the way back
		// into the length of haystack.
		if t.end < len(needle) || t.end-len(needle) >= len(haystack) {
			t.end = 0
			return searchStep{stepReject, 0, oldEnd}
		}
		frontByte := haystack[t.end-len(needle)]

		if earlyReject && oldEnd != t.end {
			return searchStep{stepReject, t.end, oldEnd}
		}

		// Quickly skip by large portions unrelated to our substring
		if !t.bytesetContains(frontByte) {
			t.end -= len(needle)
			if !longPeriod {
				t.memoryBack = len(needle)
			}
			continue search
		}

		// See if the left part of the needle matches
		crit := t.critPosBack
		if !longPeriod {
			crit = min(t.critPosBack, t.memoryBack)
		}
		for i := crit - 1; i >= 0; i-- {
			if needle[i] != haystack[t.end-len(needle)+i] {
				t.end -= t.critPosBack - i
				if !longPeriod {
					t.memoryBack = len(needle)
				}
				continue search
			}
		}

		// See if the right part of the needle matches
		needleEnd := len(needle)
		if !longPeriod {
			needleEnd = t.memoryBack
		}
		for i := t.critPosBack; i < needleEnd; i++ {
			if needle[i] != haystack[t.end-len(needle)+i] {
				t.end -= t.period
				if !longPeriod {
					t.memoryBack = t.period
				}
				continue search
			}
		}

		// We have found a match!
		matchPos := t.end - len(needle)
		// Note: sub t.period instead of len(needle) to have overlapping matches
		t.end -= len(needle)
		if !longPeriod {
			t.memoryBack = len(needle)
		}

		return searchStep{stepMatch, matchPos, matchPos + len(needle)}
	}
}

// twoWayMaximalSuffix is TwoWaySearcher::maximal_suffix: it computes the
// maximal suffix of `arr`.
//
// The maximal suffix is a possible critical factorization (u, v) of `arr`.
//
// Returns (`i`, `p`) where `i` is the starting index of v and `p` is the
// period of v.
//
// `orderGreater` determines if lexical order is `<` or `>`. Both
// orders must be computed -- the ordering with the largest `i` gives
// a critical factorization.
//
// For long period cases, the resulting period is not exact (it is too short).
func twoWayMaximalSuffix(arr string, orderGreater bool) (int, int) {
	left := 0   // Corresponds to i in the paper
	right := 1  // Corresponds to j in the paper
	offset := 0 // Corresponds to k in the paper, but starting at 0
	// to match 0-based indexing.
	period := 1 // Corresponds to p in the paper

	for right+offset < len(arr) {
		a := arr[right+offset]
		// `left` will be inbounds when `right` is.
		b := arr[left+offset]
		if (a < b && !orderGreater) || (a > b && orderGreater) {
			// Suffix is smaller, period is entire prefix so far.
			right += offset + 1
			offset = 0
			period = right - left
		} else if a == b {
			// Advance through repetition of the current period.
			if offset+1 == period {
				right += offset + 1
				offset = 0
			} else {
				offset++
			}
		} else {
			// Suffix is larger, start over from current location.
			left = right
			right++
			offset = 0
			period = 1
		}
	}
	return left, period
}

// twoWayReverseMaximalSuffix is TwoWaySearcher::reverse_maximal_suffix: it
// computes the maximal suffix of the reverse of `arr`.
//
// The maximal suffix is a possible critical factorization (u', v') of `arr`.
//
// Returns `i` where `i` is the starting index of v', from the back;
// returns immediately when a period of `knownPeriod` is reached.
//
// `orderGreater` determines if lexical order is `<` or `>`. Both
// orders must be computed -- the ordering with the largest `i` gives
// a critical factorization.
//
// For long period cases, the resulting period is not exact (it is too short).
func twoWayReverseMaximalSuffix(arr string, knownPeriod int, orderGreater bool) int {
	left := 0   // Corresponds to i in the paper
	right := 1  // Corresponds to j in the paper
	offset := 0 // Corresponds to k in the paper, but starting at 0
	// to match 0-based indexing.
	period := 1 // Corresponds to p in the paper
	n := len(arr)

	for right+offset < n {
		a := arr[n-(1+right+offset)]
		b := arr[n-(1+left+offset)]
		if (a < b && !orderGreater) || (a > b && orderGreater) {
			// Suffix is smaller, period is entire prefix so far.
			right += offset + 1
			offset = 0
			period = right - left
		} else if a == b {
			// Advance through repetition of the current period.
			if offset+1 == period {
				right += offset + 1
				offset = 0
			} else {
				offset++
			}
		} else {
			// Suffix is larger, start over from current location.
			left = right
			right++
			offset = 0
			period = 1
		}
		if period == knownPeriod {
			break
		}
	}
	return left
}
