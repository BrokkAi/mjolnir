# `go/rs/strs`: Rust's `str`, `String` and `char` methods

Package `go/rs/strs`. Oracle module `go/port/oracle/src/std_str.rs`, feature
`std_str`.

A Rust `String` is a Go `string` in this port and a `char` is a `rune`.
Product code is translated call for call, so every `str`, `String` and `char`
method the Rust code uses whose result could differ from the obvious Go call
needs a function here with Rust 1.99.1's exact behaviour (`core/src/str/`,
`core/src/char/`, `core/src/unicode/`, `alloc/src/str.rs`,
`alloc/src/string.rs`; tests in `coretests/tests/str*.rs`,
`alloctests/tests/str.rs`, `alloctests/tests/string.rs`,
`coretests/tests/char.rs`).

First list what Mjolnir uses, with counts (grep `mj-*/`, `voice-worker/`).
Then cover all of it. At least:

- UTF-8: `FromUtf8` with Rust's `Utf8Error` (`valid_up_to`, `error_len`, exact
  `Display`), `FromUtf8Lossy` with Rust's substitution of maximal invalid
  subparts, `FromUtf16`/`FromUtf16Lossy`, `EncodeUtf16`, `IsCharBoundary`,
  `CharsCount`, `CharIndices`, `Chars`, `FloorCharBoundary`/
  `CeilCharBoundary`, and a checked `Slice(s, start, end)` that panics with
  Rust's message for an index off a char boundary or out of range.
- Case and classes from the toolchain's own Unicode tables (generated from
  `core/src/unicode/unicode_data.rs`; not Go's `unicode`, whose version and
  properties differ): `ToLowercase`, `ToUppercase` (full mappings, Greek final
  sigma, one-to-many), `CharToLowercase`/`CharToUppercase`, `IsAlphabetic`,
  `IsNumeric`, `IsAlphanumeric`, `IsWhitespace`, `IsLowercase`,
  `IsUppercase`, `IsControl`, the ASCII family, `CharIsDigit`, `CharToDigit`,
  `CharFromDigit`, `CharFromU32`, `CharLenUtf8`.
- Trimming, splitting and searching with Rust's rules, with char, string and
  predicate patterns (`Trim*`, `TrimMatches*`, `StripPrefix`/`StripSuffix` as
  options, `Lines`, `SplitWhitespace`, `SplitAsciiWhitespace`, `Split`,
  `Rsplit`, `Splitn`, `Rsplitn`, `SplitTerminator`, `SplitInclusive`,
  `SplitOnce`, `RsplitOnce`, `Matches`, `MatchIndices`, `Find`/`Rfind`,
  `Contains`, `Replace`, `Replacen`, `Repeat`, `EscapeDebug`, `EscapeDefault`,
  `EscapeUnicode`).
- `String` mutation over `*string`: `Truncate` (panics off a boundary), `Pop`,
  `Remove`, `Insert`, `InsertStr`, `Drain`, `Retain`, `SplitOff`,
  `ReplaceRange`.
- Parsing every integer type, `f32`/`f64`, `bool` and `char` with Rust's
  acceptance rules and exact error text (`invalid digit found in string`,
  `cannot parse integer from empty string`, `number too large to fit in target
  type`, `invalid float literal`, ...), `IntErrorKind`, and `from_str_radix`.

Name each function after the Rust method (`strs.<RustMethodInPascalCase>`);
where Rust overloads on the pattern type add `Char` or `Fn` (`strs.SplitChar`,
`strs.SplitFn`). Give each a doc comment naming the Rust method.

Tests: the std tests for what you cover, and differential tests against the
oracle over fixed edge cases and seeded random strings (ASCII, multi-byte,
combining marks, invalid UTF-8 for the lossy and error paths), and exhaustive
comparison over all of Unicode for the per-char class and case functions.
