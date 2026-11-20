// Port of bytesize 2.7.0 src/lib.rs

package bytesize

import (
	"testing"

	"github.com/BrokkAi/mjolnir/go/rs"
)

// The documentation examples of src/lib.rs, which Rust runs as tests.

// Construction using SI or IEC helpers.
func TestLib_doc_construction(t *testing.T) {
	assert(t, ByteSizeKib(4) > ByteSizeKb(4))
}

// Display as human-readable string.
func TestLib_doc_display(t *testing.T) {
	assertEq(t, "518.0 GiB", ByteSizeGib(518).Display().Iec().String())
	assertEq(t, "556.2 GB", ByteSizeGib(518).Display().Si().String())
	assertEq(t, "518.0G", ByteSizeGib(518).Display().IecShort().String())
	assertEq(t, "4.0 Kib", ByteSizeB(512).Display().IecBits().String())
	assertEq(t, "4.1 kb", ByteSizeB(512).Display().SiBits().String())
}

// Arithmetic operations are supported.
func TestLib_doc_arithmetic(t *testing.T) {
	plus := ByteSizeMb(1) + ByteSizeKb(100)
	// println!("{plus}")
	assertEq(t, plus.String(), "1.0 MiB")

	minus := ByteSizeTb(1) - ByteSizeGb(4)
	assertEq(t, ByteSizeGb(996), minus)
}

// mod core_tests

func TestLib_test_arithmetic_op(t *testing.T) {
	x := ByteSizeMb(1)
	y := ByteSizeKb(100)

	assertEq(t, (x + y).AsU64(), uint64(1_100_000))

	assertEq(t, (x - y).AsU64(), uint64(900_000))

	assertEq(t, (x + ByteSize(uint64(100*1000))).AsU64(), 1_100_000)

	assertEq(t, (x * ByteSize(uint64(2))).AsU64(), 2_000_000)

	x += y
	assertEq(t, x.AsU64(), 1_100_000)
	x *= ByteSize(uint64(2))
	assertEq(t, x.AsU64(), 2_200_000)
}

func TestLib_test_arithmetic_primitives(t *testing.T) {
	x := ByteSizeMb(1)

	assertEq(t, (x + ByteSize(uint64(MB))).AsU64(), 2_000_000)
	assertEq(t, (x + ByteSize(uint32(MB))).AsU64(), 2_000_000)
	assertEq(t, (x + ByteSize(uint16(KB))).AsU64(), 1_001_000)
	assertEq(t, (x - ByteSize(uint64(MB))).AsU64(), 0)
	assertEq(t, (x - ByteSize(uint32(MB))).AsU64(), 0)
	assertEq(t, (x - ByteSize(uint32(KB))).AsU64(), 999_000)

	x += ByteSize(uint64(MB))
	x += ByteSize(uint32(MB))
	x += ByteSize(uint16(10))
	x += ByteSize(uint8(1))
	assertEq(t, x.AsU64(), 3_000_011)
}

func TestLib_test_sum(t *testing.T) {
	// `iter::Sum` is a loop adding from `ByteSize::default()`.
	sum := func(sizes []ByteSize) ByteSize {
		total := rs.Default[ByteSize]()
		for _, size := range sizes {
			total += size
		}
		return total
	}
	sizes := []ByteSize{ByteSizeKb(1), ByteSizeMb(1), ByteSizeMib(1)}

	assertEq(t,
		sum(sizes),
		ByteSizeB(KB+MB+MIB),
	)
	assertEq(t, sum(sizes[:]), ByteSizeB(KB+MB+MIB))
	assertEq(t,
		sum(nil),
		ByteSizeB(0),
	)
}

func TestLib_test_comparison(t *testing.T) {
	assert(t, ByteSizeMb(1) == ByteSizeKb(1000))
	assert(t, ByteSizeMib(1) == ByteSizeKib(1024))
	assert(t, ByteSizeMb(1) != ByteSizeKib(1024))
	assert(t, ByteSizeMb(1) < ByteSizeKib(1024))
	assert(t, ByteSizeB(0) < ByteSizeTib(1))
	assert(t, ByteSizePib(1) < ByteSizeEb(1))
}

func TestLib_as_unit_conversions(t *testing.T) {
	assertEq(t, 41992187.5, ByteSizeGb(43).AsKib())
	assertEq(t, 0.028311552, ByteSizeMib(27).AsGb())
	assertEq(t, 0.0380859375, ByteSizeTib(39).AsPib())
	assertEq(t, 961.482752, ByteSizeKib(938948).AsMb())
	assertEq(t, 4.195428726649908, ByteSizePb(4837).AsEib())
	assertEq(t, 2.613772153284117, ByteSizeB(2873872874893).AsTib())
}

func TestLib_test_default(t *testing.T) {
	assertEq(t, ByteSizeB(0), rs.Default[ByteSize]())
}

// mod alloc_tests. The quickcheck properties draw their inputs as
// quickcheck 1.0 does (support_test.go), from a fixed seed.

func TestLib_parsing_never_panics(t *testing.T) {
	random := quickcheckRandom(1)
	for range quickcheckCases {
		size := quickcheckString(random)
		_, _ = ByteSizeFromStr(size)
	}
	// A Go string need not be UTF-8. Whatever its bytes, parsing must not
	// panic either.
	alphabet := []byte("0123456789.kKmMgGiIbB \t\x80\xbf\xc3\xa9\xe2\x80\xff\xf0")
	for range 20000 {
		raw := make([]byte, random.IntN(16))
		for i := range raw {
			raw[i] = alphabet[random.IntN(len(alphabet))]
		}
		_, _ = ByteSizeFromStr(string(raw))
	}
}

func TestLib_to_string_never_blank(t *testing.T) {
	random := quickcheckRandom(2)
	for range quickcheckCases {
		size := ByteSize(quickcheckU64(random))
		assert(t, size.String() != "")
	}
}

func TestLib_to_string_never_large(t *testing.T) {
	random := quickcheckRandom(3)
	for range quickcheckCases {
		size := ByteSize(quickcheckU64(random))
		if len(size.String()) >= 11 {
			t.Fatalf("%d displays as %q", uint64(size), size.String())
		}
	}
}

func TestLib_string_round_trip(t *testing.T) {
	random := quickcheckRandom(4)
	for range quickcheckCases {
		size := ByteSize(quickcheckU64(random))
		// currently fails on many inputs above the pebibyte level
		if size > ByteSizePib(1) {
			continue
		}

		assertEq(t, rs.Unwrap(ByteSizeFromStr(size.String())), size)
	}
}

func assertDisplay(t *testing.T, expected string, b ByteSize) {
	t.Helper()
	assertEq(t, expected, rs.Format("{b}", rs.Arg("b", b)))
}

func TestLib_test_display(t *testing.T) {
	requireFormatter(t)
	assertDisplay(t, "215 B", ByteSizeB(215))
	assertDisplay(t, "1.0 KiB", ByteSizeKib(1))
	assertDisplay(t, "301.0 KiB", ByteSizeKib(301))
	assertDisplay(t, "419.0 MiB", ByteSizeMib(419))
	assertDisplay(t, "518.0 GiB", ByteSizeGib(518))
	assertDisplay(t, "815.0 TiB", ByteSizeTib(815))
	assertDisplay(t, "609.0 PiB", ByteSizePib(609))
	assertDisplay(t, "15.0 EiB", ByteSizeEib(15))
}

func TestLib_test_display_alignment(t *testing.T) {
	requireFormatter(t)
	assertEq(t, "|357 B     |", rs.Format("|{:10}|", ByteSize(357)))
	assertEq(t, "|     357 B|", rs.Format("|{:>10}|", ByteSize(357)))
	assertEq(t, "|357 B     |", rs.Format("|{:<10}|", ByteSize(357)))
	assertEq(t, "|  357 B   |", rs.Format("|{:^10}|", ByteSize(357)))

	assertEq(t, "|-----357 B|", rs.Format("|{:->10}|", ByteSize(357)))
	assertEq(t, "|357 B-----|", rs.Format("|{:-<10}|", ByteSize(357)))
	assertEq(t, "|--357 B---|", rs.Format("|{:-^10}|", ByteSize(357)))
}

func TestLib_test_display_width_with_precision(t *testing.T) {
	requireFormatter(t)
	size := ByteSizeMib(1908)
	// Precision is honored as decimal places even when a width is given, and
	// the rendered value is never truncated to satisfy the precision.
	assertEq(t, "| 1.86328 GiB|", rs.Format("|{size:>12.5}|", rs.Arg("size", size)))
	assertEq(t, "|1.86328 GiB |", rs.Format("|{size:<12.5}|", rs.Arg("size", size)))
	assertEq(t, "|1.86328 GiB |", rs.Format("|{size:12.5}|", rs.Arg("size", size)))
	assertEq(t, "|--1.86328 GiB--|", rs.Format("|{size:-^15.5}|", rs.Arg("size", size)))
	assertEq(t, "|     2 GiB|", rs.Format("|{size:>10.0}|", rs.Arg("size", size)))
	// Width narrower than the value leaves it intact instead of truncating.
	assertEq(t, "1.86328 GiB", rs.Format("{size:3.5}", rs.Arg("size", size)))
}
