# The Rust formatter, inside package `go/rs`

Files: every `go/rs/fmt_*.go` and its `_test.go`, `go/rs/formatter_pending.go`
(deleted when the real `Formatter` replaces it), and the function
`formatToString` in `go/rs/pending.go` (removed once yours exists). Oracle
module: `go/port/oracle/src/std_fmt.rs`, feature `std_fmt`.

Other people add files to `go/rs` at the same time (clone/eq/cmp,
numbers/sort/slices). Do not touch their files, keep the package compiling at
every step, and do not change an existing exported signature in `fmt_api.go`,
`formatter_pending.go`, `registry.go` or `containers.go` (you may add). If you
develop outside the package (a copy with the go tool's `-overlay`), install
into `go/rs` often: work that exists only in `/tmp` is lost on reboot.

Reproduce Rust 1.99.1 `core::fmt` and `alloc::fmt` exactly (sources under
`~/.cache/mjolnir/build/go-port/rust-src/src/rust/library/core/src/fmt/`, and
`core/src/num/`, `core/src/char/`, `core/src/unicode/printable.rs`,
`core/src/time.rs`):

1. `rs.Format(format, args...)` taking a real Rust format string at run time:
   `{}`, `{0}`, `{name}` (named args passed as `rs.Arg("name", v)`), `{{`/`}}`,
   and the whole spec grammar `[[fill]align][sign]['#']['0'][width]['.'
   precision][type]` with width and precision from arguments (`{:width$}`,
   `{:1$}`, `{:.*}`, `{:.prec$}`), types `?`, `x`, `X`, `o`, `b`, `e`, `E`,
   `x?`, `X?`, `p`. Implicit positional counters advance exactly as rustc's
   `format_args!` does (including with `.*`). A malformed string or a missing
   argument panics with a clear message (in Rust it is a compile error). Parse
   results are cached per format string.
2. `Display` and `Debug` output, byte for byte, for: bool; every integer type
   (all flags, radix forms, `{:#x}`, sign-aware zero padding); f32 and f64
   (`{}` shortest round-trip without exponent, `{:?}` rules including when it
   switches to exponent form, precision, `{:e}`, NaN, infinities, negative
   zero); `string` (`{}` with padding and precision truncation by chars; `{:?}`
   with Rust's exact escaping, which depends on the Grapheme_Extend and
   printable tables); `rs.Char`; `[]byte` and other slices and arrays;
   `rs.Option`, `rs.Result`, `rs.Unit` (`()`), `rs.Tuple2..5`; pointers (print
   what they point to, as `&T` and `Box<T>` do); Go maps (entries in sorted key
   order) and collections implementing `rs.ReflectMap`/`rs.ReflectSeq`;
   `time.Duration` with Rust's `Duration` Debug algorithm; values implementing
   `rs.Display`/`rs.Debug`; Go `error` values (`{}` is `Error()` unless the
   value implements `rs.Display`; for `{:?}` use `rs.Debug` if implemented and
   otherwise a documented fallback modelled on what Rust prints for the common
   std error types).
3. Derived `Debug` by reflection from the registry (`registry.go`): structs
   (`Name { field: value }` with names from the `rs:"..."` tag, in order),
   tuple, unit and newtype structs, data-carrying enum variants (a pointer to a
   variant struct registered with `rs.Variant`), data-free enums (`rs.Const`),
   and the pretty form `{:#?}` (Rust's PadAdapter: 4-space indent, trailing
   commas). An unregistered struct falls back to its Go type name and
   snake_cased field names; document that.
4. The real `Formatter` with every method declared in `formatter_pending.go`
   (`WriteStr`, `WriteChar`, `WriteFmt`, `Pad`, `PadIntegral`, flag getters,
   the `DebugStruct`/`DebugTuple`/`DebugList`/`DebugSet`/`DebugMap` builders
   with pretty mode and `FinishNonExhaustive`), plus `rs.DisplayFmt` and
   `rs.DebugFmt`. Then delete `formatter_pending.go`.
5. Check `defaultPanicHook` in `panic.go` against
   `library/std/src/panicking.rs` of 1.99.1 (exact text, whether the thread id
   is printed) and fix it if it differs.

Tests: port the relevant tests from `coretests/tests/fmt/` and
`alloctests/tests/fmt.rs`, and differential tests against the oracle: specs
times values for integers, floats (thousands of random and edge f64 and f32 bit
patterns), strings (every escape class, wide and combining characters),
durations, nested containers, and struct and enum types defined identically on
both sides for derived Debug in plain and pretty forms.

This is a keystone: every error message and log line in the port goes through
it. Exactness matters more than speed.
