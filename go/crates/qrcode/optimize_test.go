// Port of qrcode 0.14.1 src/optimize.rs (tests)

package qrcode

import (
	"slices"
	"testing"

	"github.com/BrokkAi/mjolnir/go/rs"
)

func optimizeParseTestsParse(data []byte) []OptimizeSegment {
	return OptimizeParserNew(data).Collect()
}

func seg(mode TypesMode, begin int, end int) OptimizeSegment {
	return OptimizeSegment{Mode: mode, Begin: begin, End: end}
}

func TestOptimizeParseTests_test_parse_1(t *testing.T) {
	segs := optimizeParseTestsParse([]byte("01049123451234591597033130128%10ABC123"))
	qrAssertSliceEq(t,
		segs,
		[]OptimizeSegment{
			seg(TypesMode_Numeric, 0, 29),
			seg(TypesMode_Alphanumeric, 29, 30),
			seg(TypesMode_Numeric, 30, 32),
			seg(TypesMode_Alphanumeric, 32, 35),
			seg(TypesMode_Numeric, 35, 38),
		},
	)
}

func TestOptimizeParseTests_test_parse_shift_jis_example_1(t *testing.T) {
	segs := optimizeParseTestsParse([]byte("\x82\xa0\x81\x41\x41\xb1\x81\xf0")) // "あ、AｱÅ"
	qrAssertSliceEq(t,
		segs,
		[]OptimizeSegment{
			seg(TypesMode_Kanji, 0, 4),
			seg(TypesMode_Alphanumeric, 4, 5),
			seg(TypesMode_Byte, 5, 6),
			seg(TypesMode_Kanji, 6, 8),
		},
	)
}

func TestOptimizeParseTests_test_parse_utf_8(t *testing.T) {
	// Mojibake?
	segs := optimizeParseTestsParse([]byte("\xe3\x81\x82\xe3\x80\x81A\xef\xbd\xb1\xe2\x84\xab"))
	qrAssertSliceEq(t,
		segs,
		[]OptimizeSegment{
			seg(TypesMode_Kanji, 0, 4),
			seg(TypesMode_Byte, 4, 5),
			seg(TypesMode_Kanji, 5, 7),
			seg(TypesMode_Byte, 7, 10),
			seg(TypesMode_Kanji, 10, 12),
			seg(TypesMode_Byte, 12, 13),
		},
	)
}

func TestOptimizeParseTests_test_not_kanji_1(t *testing.T) {
	segs := optimizeParseTestsParse([]byte("\x81\x30"))
	qrAssertSliceEq(t,
		segs,
		[]OptimizeSegment{seg(TypesMode_Byte, 0, 1), seg(TypesMode_Numeric, 1, 2)},
	)
}

func TestOptimizeParseTests_test_not_kanji_2(t *testing.T) {
	// Note that it's implementation detail that the byte seq is split into
	// two. Perhaps adjust the test to check for this.
	segs := optimizeParseTestsParse([]byte("\xeb\xc0"))
	qrAssertSliceEq(t,
		segs,
		[]OptimizeSegment{seg(TypesMode_Byte, 0, 1), seg(TypesMode_Byte, 1, 2)},
	)
}

func TestOptimizeParseTests_test_not_kanji_3(t *testing.T) {
	segs := optimizeParseTestsParse([]byte("\x81\x7f"))
	qrAssertSliceEq(t,
		segs,
		[]OptimizeSegment{seg(TypesMode_Byte, 0, 1), seg(TypesMode_Byte, 1, 2)},
	)
}

func TestOptimizeParseTests_test_not_kanji_4(t *testing.T) {
	segs := optimizeParseTestsParse([]byte("\x81\x40\x81"))
	qrAssertSliceEq(t,
		segs,
		[]OptimizeSegment{seg(TypesMode_Kanji, 0, 2), seg(TypesMode_Byte, 2, 3)},
	)
}

func optimizeOptimizeTestsTestOptimizationResult(t *testing.T, given []OptimizeSegment, expected []OptimizeSegment, version Version) {
	t.Helper()
	prevLen := OptimizeTotalEncodedLen(given, version)
	optSegs := OptimizeOptimizerNew(&optimizeSliceIter{segments: given}, version).Collect()
	newLen := OptimizeTotalEncodedLen(optSegs, version)
	if !slices.Equal(given, optSegs) {
		if !(prevLen > newLen) {
			t.Fatal(rs.Format("{} > {}", prevLen, newLen))
		}
	}
	if !slices.Equal(optSegs, expected) {
		t.Fatal(rs.Format(
			"Optimization gave something better: {} < {} ({:?})",
			newLen,
			OptimizeTotalEncodedLen(expected, version),
			optSegs,
		))
	}
}

func TestOptimizeOptimizeTests_test_example_1(t *testing.T) {
	optimizeOptimizeTestsTestOptimizationResult(t,
		[]OptimizeSegment{
			seg(TypesMode_Alphanumeric, 0, 3),
			seg(TypesMode_Numeric, 3, 6),
			seg(TypesMode_Byte, 6, 10),
		},
		[]OptimizeSegment{
			seg(TypesMode_Alphanumeric, 0, 6),
			seg(TypesMode_Byte, 6, 10),
		},
		&Version_Normal{F0: 1},
	)
}

func TestOptimizeOptimizeTests_test_example_2(t *testing.T) {
	optimizeOptimizeTestsTestOptimizationResult(t,
		[]OptimizeSegment{
			seg(TypesMode_Numeric, 0, 29),
			seg(TypesMode_Alphanumeric, 29, 30),
			seg(TypesMode_Numeric, 30, 32),
			seg(TypesMode_Alphanumeric, 32, 35),
			seg(TypesMode_Numeric, 35, 38),
		},
		[]OptimizeSegment{
			seg(TypesMode_Numeric, 0, 29),
			seg(TypesMode_Alphanumeric, 29, 38),
		},
		&Version_Normal{F0: 9},
	)
}

func TestOptimizeOptimizeTests_test_example_3(t *testing.T) {
	optimizeOptimizeTestsTestOptimizationResult(t,
		[]OptimizeSegment{
			seg(TypesMode_Kanji, 0, 4),
			seg(TypesMode_Alphanumeric, 4, 5),
			seg(TypesMode_Byte, 5, 6),
			seg(TypesMode_Kanji, 6, 8),
		},
		[]OptimizeSegment{seg(TypesMode_Byte, 0, 8)},
		&Version_Normal{F0: 1},
	)
}

func TestOptimizeOptimizeTests_test_example_4(t *testing.T) {
	optimizeOptimizeTestsTestOptimizationResult(t,
		[]OptimizeSegment{seg(TypesMode_Kanji, 0, 10), seg(TypesMode_Byte, 10, 11)},
		[]OptimizeSegment{seg(TypesMode_Kanji, 0, 10), seg(TypesMode_Byte, 10, 11)},
		&Version_Normal{F0: 1},
	)
}

func TestOptimizeOptimizeTests_test_annex_j_guideline_1a(t *testing.T) {
	optimizeOptimizeTestsTestOptimizationResult(t,
		[]OptimizeSegment{
			seg(TypesMode_Numeric, 0, 3),
			seg(TypesMode_Alphanumeric, 3, 4),
		},
		[]OptimizeSegment{
			seg(TypesMode_Numeric, 0, 3),
			seg(TypesMode_Alphanumeric, 3, 4),
		},
		&Version_Micro{F0: 2},
	)
}

func TestOptimizeOptimizeTests_test_annex_j_guideline_1b(t *testing.T) {
	optimizeOptimizeTestsTestOptimizationResult(t,
		[]OptimizeSegment{
			seg(TypesMode_Numeric, 0, 2),
			seg(TypesMode_Alphanumeric, 2, 4),
		},
		[]OptimizeSegment{seg(TypesMode_Alphanumeric, 0, 4)},
		&Version_Micro{F0: 2},
	)
}

func TestOptimizeOptimizeTests_test_annex_j_guideline_1c(t *testing.T) {
	optimizeOptimizeTestsTestOptimizationResult(t,
		[]OptimizeSegment{
			seg(TypesMode_Numeric, 0, 3),
			seg(TypesMode_Alphanumeric, 3, 4),
		},
		[]OptimizeSegment{seg(TypesMode_Alphanumeric, 0, 4)},
		&Version_Micro{F0: 3},
	)
}
