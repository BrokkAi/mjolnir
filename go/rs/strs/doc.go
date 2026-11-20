// Package strs gives Go code the methods of Rust's `str`, `String` and `char`
// with the results Rust gives, so that Mjolnir's string handling can be
// translated call for call.
//
// A Rust `String` or `&str` is a Go string and a `char` is a rune. Wherever
// the obvious Go call could answer differently (another Unicode version,
// another rule for an empty separator, another replacement for invalid
// UTF-8, another set of accepted number spellings, another panic message)
// the function here is Rust's.
//
// Stands in for: Rust 1.99.1, Unicode 17.0.0:
//
//   - core::str (mod.rs, iter.rs, pattern.rs, traits.rs, validations.rs,
//     lossy.rs, converts.rs, error.rs) and alloc::str;
//   - alloc::string (the conversions, and the methods that change a String
//     in place);
//   - core::char (methods.rs, convert.rs, decode.rs) and the tables of
//     core::unicode;
//   - `FromStr` for the integer types, `f32`, `f64`, `bool` and `char`
//     (core::num, core::num::imp::dec2flt), and `from_str_radix`;
//   - the ASCII methods of `u8` and `[u8]`.
//
// # Names
//
// A method keeps its Rust name in PascalCase, with the string first:
// `s.trim_end()` is strs.TrimEnd(s). Where the name alone would be
// ambiguous, a prefix or a suffix says which Rust item it is:
//
//   - A method that takes a pattern has one spelling for each kind of
//     pattern: Split (a `&str`), SplitChar (a `char`), SplitChars (an array
//     or slice of chars, any of which matches) and SplitFn (a predicate).
//   - A `char` method whose name a `str` method also has, or that only makes
//     sense for a char, starts with Char: CharToLowercase, CharIsAscii,
//     CharLenUtf8, CharToDigit, CharFromU32. The class predicates do not:
//     IsWhitespace, IsAlphanumeric, IsAsciiDigit. The ASCII ones take a rune
//     or a byte, as Rust gives them to both `char` and `u8`.
//   - A `[u8]` method starts with Bytes: BytesTrimAscii.
//   - `s.parse::<T>()` is Parse and the Rust type: ParseU16, ParseUsize,
//     ParseF64, ParseBool, ParseChar. `T::from_str_radix` is U8FromStrRadix.
//   - A chain that has a cheaper answer than its parts keeps both names:
//     CharsCount, CharsRev, CharsNext, CharsNextBack, CharIndicesRev.
//   - `&s[a..b]` is Slice (SliceFrom, SliceTo, SliceInclusive,
//     SliceToInclusive) and `s.get(a..b)` is Get (GetFrom, ..).
//   - `String::from_utf8` is StringFromUtf8 and `str::from_utf8` is
//     FromUtf8: they fail with different error types.
//   - A method that changes a String in place takes a pointer to it:
//     strs.Truncate(&s, n).
//
// # Shapes
//
// An `Option` is an rs.Option, a `Result` is a value and an error, and a pair
// inside either is an rs.Tuple2. Where Rust returns an iterator the function
// returns a slice of everything the iterator yields; see iter.go for what
// that costs. An index or a count is a `usize` held in an int: a negative
// int stands for the huge usize that wraps to it, and is treated and printed
// as that.
//
// Strings must be valid UTF-8, as every Rust string is. On other input
// nothing here panics, and what is returned is unspecified (an invalid byte
// is read as U+FFFD of length one).
//
// # What is not here
//
//   - What Go already spells the same way: `len`, `is_empty`, `as_bytes`,
//     `bytes`, `push`, `push_str`, `clear`, `to_owned`, comparison (both
//     order strings by their bytes), `concat` and `join`.
//   - What is unstable in 1.99.1, which Mjolnir cannot call: title case and
//     case folding, `trim_prefix`, `strip_circumfix`, `remove_matches`,
//     `replace_first`, `from_utf16le`, `is_ascii_octdigit`, `substr_range`.
//   - `{}` and `{:?}` of strings and chars, which the formatter in go/rs
//     prints. Debug's escaping rule is charEscapeDebugExt here.
//   - The iterators as objects (`remainder`, `as_str`, stopping early).
//   - `u128` and `i128` as types: ParseU128Parts and ParseI128Parts return
//     the two halves, because rs.U128 did not exist when this was written.
//
// One difference is forced by the port holding a `usize` in an int:
// ParseUsize and UsizeFromStrRadix report a value above the largest int as
// too large, where Rust parses it.
//
// # Tables
//
// unicode_data.go and dec2flt_table.go are generated from the Rust sources
// of the pinned toolchain by gen_unicode_data.go and gen_dec2flt_table.go;
// Go's unicode package is not used, since it follows another Unicode
// version. Rerun both generators when rust-toolchain.toml moves.
//
// # Tests
//
// The standard library's own tests are ported in str_test.go,
// string_test.go, char_test.go, str_lossy_test.go, pattern_test.go and
// dec2flt_test.go. oracle_test.go and oracle_more_test.go compare the package
// with Rust itself (go/port/oracle/src/std_str.rs): every code point for the
// classes, the escapes and the case mappings, and fixed and random inputs for
// the rest, recorded under testdata/oracle.
package strs
