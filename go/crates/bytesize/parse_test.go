// Port of bytesize 2.7.0 src/parse.rs

package bytesize

import (
	"strconv"
	"testing"

	"github.com/BrokkAi/mjolnir/go/rs"
)

// The documentation example of Unit.
func TestParse_doc_unit(t *testing.T) {
	assertEq(t,
		rs.Unwrap(UnitFromStr("GiB")),
		Unit_GibiByte,
	)

	_ = rs.UnwrapErr(UnitFromStr("gibibyte"))
}

func TestParse_truncating_error_strings(t *testing.T) {
	assertEq(t, "", parseToStringTruncate(""))
	assertEq(t, "b", parseToStringTruncate("b"))
	assertEq(t, "ob", parseToStringTruncate("ob"))
	assertEq(t, "foo", parseToStringTruncate("foo"))

	assertEq(t, "foo...", parseToStringTruncate("foob"))
	assertEq(t, "foo...", parseToStringTruncate("foobar"))
}

func TestParse_when_ok(t *testing.T) {
	// shortcut for writing test cases
	parse := func(s string) uint64 {
		return uint64(rs.Unwrap(ByteSizeFromStr(s)))
	}

	assertEq(t, uint64(rs.Unwrap(ByteSizeFromStr("0"))), 0)
	assertEq(t, parse("0"), 0)
	assertEq(t, parse("500"), 500)
	assertEq(t, parse("1K"), Unit_KiloByte.Mul(1))
	assertEq(t, parse("1Ki"), Unit_KibiByte.Mul(1))
	assertEq(t, parse("1.5Ki"), parseCastF64ToU64(Unit_KibiByte.MulF64(1.5)))
	assertEq(t, parse("1KiB"), Unit_KibiByte.Mul(1))
	assertEq(t, parse("1.5KiB"), parseCastF64ToU64(Unit_KibiByte.MulF64(1.5)))
	assertEq(t, parse("3 MB"), Unit_MegaByte.Mul(3))
	assertEq(t, parse("4 MiB"), Unit_MebiByte.Mul(4))
	assertEq(t, parse("6 GB"), Unit_GigaByte.Mul(6))
	assertEq(t, parse("4 GiB"), Unit_GibiByte.Mul(4))
	assertEq(t, parse("88TB"), Unit_TeraByte.Mul(88))
	assertEq(t, parse("521TiB"), Unit_TebiByte.Mul(521))
	assertEq(t, parse("8 PB"), Unit_PetaByte.Mul(8))
	assertEq(t, parse("8P"), Unit_PetaByte.Mul(8))
	assertEq(t, parse("12 PiB"), Unit_PebiByte.Mul(12))
}

func TestParse_large_integer_byte_counts_are_exact(t *testing.T) {
	// shortcut for writing test cases
	parse := func(s string) uint64 {
		return uint64(rs.Unwrap(ByteSizeFromStr(s)))
	}

	// 2^53 + 1 is not exactly representable as an `f64`, so the unit-suffix
	// parsing path must not round it down. Regression test for the
	// `f64`-based parsing path.
	assertEq(t, parse("9007199254740993B"), 9_007_199_254_740_993)
	assertEq(t, parse("9007199254740993"), parse("9007199254740993B"))
}

func TestParse_when_err(t *testing.T) {
	// shortcut for writing test cases
	parse := func(s string) error {
		_, err := ByteSizeFromStr(s)
		return err
	}

	assert(t, parse("") != nil)
	assert(t, parse("a124GB") != nil)
	assert(t, parse("1.3 42.0 B") != nil)
	assert(t, parse("1.3 ... B") != nil)
	// The original implementation did not account for the possibility that users may
	// use whitespace to visually separate digits, thus treat it as an error
	assert(t, parse("1 000 B") != nil)
}

func TestParse_to_and_from_str(t *testing.T) {
	// shortcut for writing test cases
	parse := func(s string) uint64 {
		return uint64(rs.Unwrap(ByteSizeFromStr(s)))
	}

	// `parse(..)` is a u64 here, so this is the integer's to_string.
	assertEq(t, parse(strconv.FormatUint(parse("128GB"), 10)), Unit_GigaByte.Mul(128))
	assertEq(t,
		parse(ByteSize(parse("128.000 GiB")).String()),
		Unit_GibiByte.Mul(128),
	)
}
