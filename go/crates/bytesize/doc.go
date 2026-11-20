// Package bytesize stands in for the Rust crate `bytesize` 2.7.0, a byte
// count with human-readable display and parsing, built as Mjolnir builds it:
// with the default `std` feature and without `serde` and `arbitrary`.
//
// Mjolnir parses with it. mj-core/src/config/targets.rs accepts a machine's
// build cache scheduler memory when `value.parse::<ByteSize>().is_ok()`
// (or the value is "none"), so which strings parse decides which
// configurations load. The port also covers the crate's display, which is
// the type's other everyday use.
//
// # Names
//
//	value.parse::<ByteSize>()              bytesize.ByteSizeFromStr(value)
//	ByteSize(n), size.0, size.as_u64()     bytesize.ByteSize(n), uint64(size), size.AsU64()
//	ByteSize::kib(4), ByteSize::default()  bytesize.ByteSizeKib(4), bytesize.ByteSize(0)
//	size.as_gib()                          size.AsGib()
//	size.to_string()                       size.String()
//	size.display().si().to_string()        size.Display().Si().String()
//	bytesize::KIB, bytesize::kib(4)        bytesize.KIB, bytesize.Kib(4)
//	"GiB".parse::<Unit>(), Unit::GibiByte  bytesize.UnitFromStr("GiB"), bytesize.Unit_GibiByte
//	Unit::KibiByte * 2, 1.5 * Unit::KibiByte  bytesize.Unit_KibiByte.Mul(2), .MulF64(1.5)
//	bytesize::UnitParseError               bytesize.UnitParseError
//
// The constants keep their Rust spelling (KB, KIB, ...), because the
// PascalCase of KB would be the name of the function kb.
//
// ByteSize is `struct ByteSize(pub u64)`, a newtype, so it is a defined type
// over uint64 with value receivers. Go's own operators are the crate's
// arithmetic and comparisons: `a + b`, `a - ByteSize(n)`, `a * 2`, `a += b`,
// `a < b`, and `iter::Sum` is a loop adding from ByteSize(0). Where Rust's
// arithmetic would overflow (`ByteSize::eb(20)`, `ByteSize(u64::MAX) +
// ByteSize(1)`) it panics in a debug build and wraps in a release build; Go
// wraps. The display wrapper's style methods change it in place and return
// it, so a chain reads as in Rust.
//
// ByteSizeFromStr's error is the crate's `String`: its Error text is the
// message, and it prints with `{}` and `{:?}` as a String does.
// UnitFromStr's error is a UnitParseError.
//
// # Covered
//
// The whole crate as Mjolnir builds it:
//
//   - src/lib.rs: the unit constants and conversion functions, ByteSize with
//     its constructors, `as_*` conversions, `display()`, Display (with width,
//     fill, alignment and precision) and Debug; the operator impls and Sum
//     are Go's operators, as said above.
//   - src/display.rs: the display wrapper and its six styles (IEC, SI, their
//     short forms, and bits in either), the choice of unit prefix, rounding
//     to the formatter's precision.
//   - src/parse.rs: FromStr for ByteSize and Unit, the operator impls of
//     Unit, UnitParseError and the truncated unit it reports.
//
// # Not covered
//
//   - src/serde.rs and src/arbitrary.rs: Mjolnir enables neither feature.
//   - The crate's tests of src/serde.rs, for the same reason. The quickcheck
//     properties are ported with inputs drawn as quickcheck 1.0 draws them,
//     from a fixed seed.
//
// A Rust `&str` is always UTF-8 and a Go string need not be. A byte that is
// not part of a valid encoding is taken as one character, so parsing never
// panics: such input is an unparsable number or an unknown unit.
//
// # Behaviour to know about
//
// Parsing:
//
//   - A whole number of bytes is read by u64's parser, which takes a leading
//     `+`: "+5" is 5 bytes, but "+5KB", "-5", " 8GiB" and "8GiB " are
//     errors. A number with a fraction needs a unit: "1.5" is an error.
//   - Units are matched in ASCII letter case only ("gib" is GiB, but the
//     Kelvin sign is not K), and a unit must be the rest of the string.
//   - Between the number and the unit any Unicode White_Space is skipped
//     (a no-break space, an ideographic space, a newline), but not a
//     zero-width space or a byte order mark.
//   - A whole number with a unit is multiplied exactly and saturates at
//     u64::MAX ("20EB", "16EiB", "18446744073709551616B"). A fraction is
//     multiplied as an f64 and truncated toward zero ("0.5B" is 0).
//
// Display:
//
//   - One decimal by default; a precision sets the decimals, and halves
//     round to even: 1280 bytes is "1.2 KiB", 1.25 KiB. A value just under
//     a unit prints in it: 1048575 bytes is "1024.0 KiB".
//   - Only ByteSize's own Display pads to a width, and it never truncates.
//     The `display()` wrapper and Debug ignore width, fill and alignment,
//     and `{:012}` pads with spaces on the right.
//
// # Tests
//
// The crate's tests for what is covered are ported (lib_test.go,
// display_test.go, parse_test.go), with the documentation examples.
// oracle_test.go compares the package with the real crate over two recorded
// corpora: every display style and conversion at the boundaries the display
// branches on, parsing of written-out, Unicode and random size strings, and
// unit parsing; and, through rs.Formatter, every display with width, fill,
// alignment and precision, the Debug forms and the parse error.
package bytesize
