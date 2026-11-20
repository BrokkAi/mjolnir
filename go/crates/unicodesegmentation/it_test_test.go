// Port of unicode-segmentation 1.13.3 tests/test.rs

// Copyright 2012-2015 The Rust Project Developers. See the COPYRIGHT
// file at the top-level directory of this distribution and at
// http://rust-lang.org/COPYRIGHT.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

// `mod testdata;` is it_testdata_test.go, which gen.go writes from the
// crate's tests/testdata/mod.rs.

package unicodesegmentation

import (
	"slices"
	"strings"
	"testing"

	"github.com/BrokkAi/mjolnir/go/rs"
)

func TestItTest_test_graphemes(t *testing.T) {
	// use crate::testdata::{TEST_DIFF, TEST_SAME};

	extraDiff := []rs.Tuple3[string, []string, []string]{
		// Official test suite doesn't include two Prepend chars between two other chars.
		{
			F0: "\x20\u0600\u0600\x20",
			F1: []string{"\x20", "\u0600\u0600\x20"},
			F2: []string{"\x20", "\u0600", "\u0600", "\x20"},
		},
		// Test for Prepend followed by two Any chars
		{
			F0: "\u0600\x20\x20",
			F1: []string{"\u0600\x20", "\x20"},
			F2: []string{"\u0600", "\x20", "\x20"},
		},
	}

	extraSame := []rs.Tuple2[string, []string]{
		// family emoji (more than two emoji joined by ZWJ)
		{
			F0: "\U0001f468\u200D\U0001f467\u200D\U0001f466",
			F1: []string{"\U0001f468\u200D\U0001f467\u200D\U0001f466"},
		},
		// cartwheel emoji followed by two fitzpatrick skin tone modifiers
		// (test case from issue #19)
		{
			F0: "\U0001F938\U0001F3FE\U0001F3FE",
			F1: []string{"\U0001F938\U0001F3FE\U0001F3FE"},
		},
	}

	for _, c := range slices.Concat(testdataTestSame, extraSame) {
		s, g := c.F0, c.F1
		// test forward iterator
		ourExtended := testCollect(UnicodeSegmentationGraphemes(s, true).Next)
		ourLegacy := testCollect(UnicodeSegmentationGraphemes(s, false).Next)
		testAssertEq(t, ourExtended, g, "%+q extended", s)
		testAssertEq(t, ourLegacy, g, "%+q legacy", s)

		// test reverse iterator
		itTestAssert(t, slices.Equal(testCollect(UnicodeSegmentationGraphemes(s, true).NextBack), testReversed(g)),
			"UnicodeSegmentation::graphemes(s, true)\n            .rev()\n            .eq(g.iter().rev().cloned())", s)
		itTestAssert(t, slices.Equal(testCollect(UnicodeSegmentationGraphemes(s, false).NextBack), testReversed(g)),
			"UnicodeSegmentation::graphemes(s, false)\n            .rev()\n            .eq(g.iter().rev().cloned())", s)
	}

	for _, c := range slices.Concat(testdataTestDiff, extraDiff) {
		s, gt, gf := c.F0, c.F1, c.F2
		// test forward iterator
		itTestAssert(t, slices.Equal(testCollect(UnicodeSegmentationGraphemes(s, true).Next), gt),
			"UnicodeSegmentation::graphemes(s, true).eq(gt.iter().cloned())", s)
		itTestAssert(t, slices.Equal(testCollect(UnicodeSegmentationGraphemes(s, false).Next), gf),
			"UnicodeSegmentation::graphemes(s, false).eq(gf.iter().cloned())", s)

		// test reverse iterator
		itTestAssert(t, slices.Equal(testCollect(UnicodeSegmentationGraphemes(s, true).NextBack), testReversed(gt)),
			"UnicodeSegmentation::graphemes(s, true)\n            .rev()\n            .eq(gt.iter().rev().cloned())", s)
		itTestAssert(t, slices.Equal(testCollect(UnicodeSegmentationGraphemes(s, false).NextBack), testReversed(gf)),
			"UnicodeSegmentation::graphemes(s, false)\n            .rev()\n            .eq(gf.iter().rev().cloned())", s)
	}

	// test the indices iterators
	s := "a\u0310e\u0301o\u0308\u0332\r\n"
	grInds := testCollect(UnicodeSegmentationGraphemeIndices(s, true).Next)
	b := []rs.Tuple2[int, string]{rs.T2(0, "a\u0310"), rs.T2(3, "e\u0301"), rs.T2(6, "o\u0308\u0332"), rs.T2(11, "\r\n")}
	testAssertEq(t, grInds, b)
	grInds = testCollect(UnicodeSegmentationGraphemeIndices(s, true).NextBack)
	b = []rs.Tuple2[int, string]{rs.T2(11, "\r\n"), rs.T2(6, "o\u0308\u0332"), rs.T2(3, "e\u0301"), rs.T2(0, "a\u0310")}
	testAssertEq(t, grInds, b)
	grIndsIter := UnicodeSegmentationGraphemeIndices(s, true)
	{
		grInds := grIndsIter // .by_ref()
		e1Lower, e1Upper := grInds.SizeHint()
		testAssertEq(t, []rs.Tuple2[int, rs.Option[int]]{rs.T2(e1Lower, e1Upper)},
			[]rs.Tuple2[int, rs.Option[int]]{rs.T2(1, rs.Some(13))})
		c := len(testCollect(grInds.Next)) // .count()
		testAssertEq(t, []int{c}, []int{4})
	}
	e2Lower, e2Upper := grIndsIter.SizeHint()
	testAssertEq(t, []rs.Tuple2[int, rs.Option[int]]{rs.T2(e2Lower, e2Upper)},
		[]rs.Tuple2[int, rs.Option[int]]{rs.T2(0, rs.Some(0))})

	// make sure the reverse iterator does the right thing with "\n" at beginning of string
	s = "\n\r\n\r"
	gr := testCollect(UnicodeSegmentationGraphemes(s, true).NextBack)
	b2 := []string{"\r", "\r\n", "\n"}
	testAssertEq(t, gr, b2)
}

func TestItTest_test_words(t *testing.T) {
	// use crate::testdata::TEST_WORD;

	// Unicode's official tests don't really test longer chains of flag emoji
	// TODO This could be improved with more tests like flag emoji with interspersed Extend chars and ZWJ
	extraTests := []rs.Tuple2[string, []string]{
		{
			F0: "\U0001F1E6\U0001F1EB\U0001F1E6\U0001F1FD\U0001F1E6\U0001F1F1\U0001F1E9\U0001F1FF\U0001F1E6\U0001F1F8\U0001F1E6\U0001F1E9\U0001F1E6\U0001F1F4",
			F1: []string{"\U0001F1E6\U0001F1EB", "\U0001F1E6\U0001F1FD", "\U0001F1E6\U0001F1F1", "\U0001F1E9\U0001F1FF", "\U0001F1E6\U0001F1F8", "\U0001F1E6\U0001F1E9", "\U0001F1E6\U0001F1F4"},
		},
		{
			F0: "\U0001F1E6\U0001F1EB\U0001F1E6\U0001F1FD\U0001F1E6\U0001F1F1\U0001F1E9\U0001F1FF\U0001F1E6\U0001F1F8\U0001F1E6\U0001F1E9\U0001F1E6",
			F1: []string{"\U0001F1E6\U0001F1EB", "\U0001F1E6\U0001F1FD", "\U0001F1E6\U0001F1F1", "\U0001F1E9\U0001F1FF", "\U0001F1E6\U0001F1F8", "\U0001F1E6\U0001F1E9", "\U0001F1E6"},
		},
		{
			F0: "\U0001F1E6a\U0001F1EB\U0001F1E6\U0001F1FDa\U0001F1E6\U0001F1F1\U0001F1E9\U0001F1FF\U0001F1E6\U0001F1F8\U0001F1E6\U0001F1E9\U0001F1E6",
			F1: []string{"\U0001F1E6", "a", "\U0001F1EB\U0001F1E6", "\U0001F1FD", "a", "\U0001F1E6\U0001F1F1", "\U0001F1E9\U0001F1FF", "\U0001F1E6\U0001F1F8", "\U0001F1E6\U0001F1E9", "\U0001F1E6"},
		},
		{
			F0: "\U0001f468\u200D\U0001f468\u200D\U0001f466",
			F1: []string{"\U0001f468\u200D\U0001f468\u200D\U0001f466"},
		},
		{F0: "\U0001F60C\U0001F44E\U0001F3FC", F1: []string{"\U0001F60C", "\U0001F44E\U0001F3FC"}},
		// perhaps wrong, spaces should not be included?
		{F0: "hello world", F1: []string{"hello", " ", "world"}},
		{
			F0: "\U0001F1E8\U0001F1E6\U0001F1E8\U0001F1ED\U0001F1FF\U0001F1F2\U0001F1FF hi",
			F1: []string{"\U0001F1E8\U0001F1E6", "\U0001F1E8\U0001F1ED", "\U0001F1FF\U0001F1F2", "\U0001F1FF", " ", "hi"},
		},
	}
	for _, c := range slices.Concat(testdataTestWord, extraTests) {
		s, w := c.F0, c.F1
		// assert_!($test, $exp, $name): the vectors, and the message
		// "{} test for testcase ({:?}, {:?}) failed."

		// test forward iterator
		testAssertEq(t,
			testCollect(SplitWordBounds(s).Next),
			w,
			"%s test for testcase (%+q, %s) failed.", "Forward word boundaries", s, testDebug(w),
		)

		// test reverse iterator
		testAssertEq(t,
			testCollect(SplitWordBounds(s).NextBack),
			testReversed(w),
			"%s test for testcase (%+q, %s) failed.", "Reverse word boundaries", s, testDebug(w),
		)

		// generate offsets from word string lengths
		indices := []int{0}
		total := 0
		for _, piece := range w {
			total += len(piece)
			indices = append(indices, total)
		}
		indices = indices[:len(indices)-1] // indices.pop()

		// test forward indices iterator
		testAssertEq(t,
			itTestOffsets(testCollect(SplitWordBoundIndices(s).Next)),
			indices,
			"%s test for testcase (%+q, %s) failed.", "Forward word indices", s, testDebug(w),
		)

		// test backward indices iterator
		testAssertEq(t,
			itTestOffsets(testCollect(SplitWordBoundIndices(s).NextBack)),
			testReversed(indices),
			"%s test for testcase (%+q, %s) failed.", "Reverse word indices", s, testDebug(w),
		)
	}
}

// test_sentences is not ported: sentence boundaries are not (see doc.go).

// The quickcheck! properties. Each is a test that checks the property on
// generated strings; quickcheck 1.0.3 draws 100 of them, from a new random
// seed on every run. Here each property checks itTestQuickcheckCases strings
// drawn from a fixed seed with quickcheck's own distribution
// (itTestArbitraryString), so every run checks the same strings.

const itTestQuickcheckCases = 2000

// itTestQuickcheck checks property on the generated strings, and reports
// the first string it does not hold for.
func itTestQuickcheck(t *testing.T, property func(s string) bool) {
	t.Helper()
	r := &corpusRand{state: 0x717569636B636865}
	for range itTestQuickcheckCases {
		s := itTestArbitraryString(r)
		if !property(s) {
			t.Fatalf("[quickcheck] TEST FAILED. Arguments: (%+q)", s)
		}
	}
}

func TestItTest_quickcheck_forward_reverse_graphemes_extended(t *testing.T) {
	itTestQuickcheck(t, func(s string) bool {
		a := testCollect(Graphemes(s, true).Next)
		b := testCollect(Graphemes(s, true).NextBack)
		slices.Reverse(b)
		return slices.Equal(a, b)
	})
}

func TestItTest_quickcheck_forward_reverse_graphemes_legacy(t *testing.T) {
	itTestQuickcheck(t, func(s string) bool {
		a := testCollect(Graphemes(s, false).Next)
		b := testCollect(Graphemes(s, false).NextBack)
		slices.Reverse(b)
		return slices.Equal(a, b)
	})
}

func TestItTest_quickcheck_join_graphemes(t *testing.T) {
	itTestQuickcheck(t, func(s string) bool {
		a := strings.Join(testCollect(Graphemes(s, true).Next), "")
		b := strings.Join(testCollect(Graphemes(s, false).Next), "")
		return a == s && b == s
	})
}

func TestItTest_quickcheck_forward_reverse_words(t *testing.T) {
	itTestQuickcheck(t, func(s string) bool {
		a := testCollect(SplitWordBounds(s).Next)
		b := testCollect(SplitWordBounds(s).NextBack)
		slices.Reverse(b)
		return slices.Equal(a, b)
	})
}

func TestItTest_quickcheck_join_words(t *testing.T) {
	itTestQuickcheck(t, func(s string) bool {
		a := strings.Join(testCollect(SplitWordBounds(s).Next), "")
		return a == s
	})
}

// itTestArbitraryString is quickcheck 1.0.3's `Arbitrary for String` with
// the default `Gen` size of 100: up to 99 characters, each drawn by
// itTestArbitraryChar.
func itTestArbitraryString(r *corpusRand) string {
	size := r.intn(100)
	chars := make([]rune, size)
	for index := range chars {
		chars[index] = itTestArbitraryChar(r)
	}
	return string(chars)
}

// itTestArbitraryChar is quickcheck 1.0.3's `Arbitrary for char`
// (src/arbitrary.rs).
func itTestArbitraryChar(r *corpusRand) rune {
	mode := r.intn(100)
	switch {
	case mode <= 49:
		// ASCII + some control characters
		return rune(r.intn(0xB0))
	case mode <= 59:
		// Unicode BMP characters
		for {
			if c := rune(r.intn(0x10000)); c < 0xD800 || c > 0xDFFF {
				return c
			}
			// ignore surrogate pairs
		}
	case mode <= 84:
		// Characters often used in programming languages
		return r.pick([]rune{
			' ', ' ', ' ', '\t', '\n', '~', '`', '!', '@', '#', '$',
			'%', '^', '&', '*', '(', ')', '_', '-', '=', '+', '[',
			']', '{', '}', ':', ';', '\'', '"', '\\', '|', ',', '<',
			'>', '.', '/', '?', '0', '1', '2', '3', '4', '5', '6',
			'7', '8', '9',
		})
	case mode <= 89:
		// Tricky Unicode, part 1
		return r.pick([]rune{
			0x0149, // a deprecated character
			0xfff0, // some of "Other, format" category:
			0xfff1,
			0xfff2,
			0xfff3,
			0xfff4,
			0xfff5,
			0xfff6,
			0xfff7,
			0xfff8,
			0xfff9,
			0xfffA,
			0xfffB,
			0xfffC,
			0xfffD,
			0xfffE,
			0xfffF,
			0x0600,
			0x0601,
			0x0602,
			0x0603,
			0x0604,
			0x0605,
			0x061C,
			0x06DD,
			0x070F,
			0x180E,
			0x110BD,
			0x1D173,
			0xe0001, // tag
			0xe0020, //  tag space
			0xe000,
			0xe001,
			0xef8ff, // private use
			0xf0000,
			0xffffd,
			0xffffe,
			0xfffff,
			0x100000,
			0x10FFFD,
			0x10FFFE,
			0x10FFFF,
			// "Other, surrogate" characters are so that very special
			// that they are not even allowed in safe Rust,
			//so omitted here
			0x3000, // ideographic space
			0x1680,
			// other space characters are already covered by two next
			// branches
		})
	case mode <= 94:
		// Tricky unicode, part 2
		return 0x2000 + rune(r.intn(0x70))
	default:
		// Completely arbitrary characters: rand's `Standard` for char,
		// uniform over the scalar values.
		for {
			if c := rune(r.intn(0x110000)); c < 0xD800 || c > 0xDFFF {
				return c
			}
		}
	}
}

// itTestAssert is `assert!(condition)`, whose message is the condition's
// source text; the string under test follows it.
func itTestAssert(t *testing.T, condition bool, source string, s string) {
	t.Helper()
	if !condition {
		t.Fatalf("assertion failed: %s\n  for %+q", source, s)
	}
}

// itTestOffsets is `.map(|(l, _)| l)`: the offsets of the pieces.
func itTestOffsets(pieces []rs.Tuple2[int, string]) []int {
	offsets := make([]int, len(pieces))
	for index, piece := range pieces {
		offsets[index] = piece.F0
	}
	return offsets
}
