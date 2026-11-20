// Port of core 1.99.1 src/fmt (the contract; the implementation is in fmt_*.go).

package rs

import (
	"io"
	"os"
	"strings"
)

// The formatter: what it stands in for, and how Go values print.
//
// It is core::fmt and alloc::fmt of Rust 1.99.1: format!, format_args!,
// write!, print! and their kin (Format, FormatArgs, Write, Print, ...), the
// Formatter with its padding and flag methods, the Debug builders and their
// `{:#?}` layout, and the Display, Debug, LowerHex, UpperHex, Octal, Binary,
// LowerExp, UpperExp and Pointer traits of the standard types: integers
// (fmt/num.rs), floats (fmt/float.rs over num/flt2dec, Grisu with the Dragon
// fallback), char and str with the escapes of the Unicode 17.0.0 tables of
// 1.99.1, Duration (time.rs) and SystemTime (sys/time). The panic hook's
// text is std::panicking::default_hook's (panic.go).
//
// Format strings are read at run time with rustc's grammar
// (rustc_parse_format) and checked as rustc checks a format! call: a
// malformed string, a missing argument, an unused one, a width or precision
// argument that is not an integer, or a value lacking the trait its
// placeholder names panics, where Rust refuses to compile. Each string is
// parsed once and kept, which is bounded because Rust takes format strings
// only as literals.
//
// A Go value prints as the Rust type it stands for:
//
//   - bool, the integer types (int is isize, uint and uintptr are usize, a
//     bare rune is i32), float32 and float64, and string (also for a Path or
//     an OsStr: a byte that is not UTF-8 prints as `\xNN` under `{:?}`, as a
//     Path does). Char is a char. A type implementing fmtInt128 (U128,
//     I128) is a u128 or an i128.
//   - A slice or an array is a slice or an array; []byte is a Vec<u8>.
//   - A pointer is a reference or a Box: it prints what it points to. `{:p}`
//     prints the address of a pointer, a map, a func or a channel, and the
//     address and the length of a slice or a string, as a wide pointer.
//   - Option, Result, Unit and Tuple2 to Tuple5 are Option, Result, () and
//     tuples.
//   - A Go map prints its entries in key order: the Rust map it stands for
//     may not be iterated in a stable order, and output must be stable. A
//     collection implementing ReflectMap or ReflectSeq prints in its own
//     order, or in key order when it reports that its order is not sorted.
//   - time.Duration is a Duration; a negative one, which Rust cannot hold,
//     prints its magnitude after `-`. time.Time is a SystemTime.
//   - A value with FmtDisplay, FmtDebug, FmtLowerHex, ... uses it, through
//     a pointer when the method has a pointer receiver.
//   - A type registered with RegisterStruct, RegisterEnum and their kin
//     prints the Debug that `#[derive(Debug)]` gives the Rust type: `Name {
//     field: value }` with the names of the `rs` tags, `Name(value)`,
//     `Name`, the variant of an enum (a pointer to its variant struct, or a
//     constant). A constant that is no declared variant prints as
//     `Enum(n)`. A registered type has Display, and the radix and exponent
//     traits, only through the methods; without them it panics, as rustc
//     refuses. So do Char and time.Duration under `{:x}` and `{:e}`.
//   - A struct that is not registered prints under its Go type name, with
//     its Go field names in snake_case (or as a tuple struct when the fields
//     are F0, F1, ...). Register the type to print the Rust names.
//   - An error that has no FmtDisplay prints its Error() text with `{}`.
//     With `{:?}`, a Go error that is neither registered nor has FmtDebug
//     prints as a tuple struct of its type's name around its message,
//     `PathError("open x: no such file or directory")`, as Rust's message
//     wrapping errors print; the runtime packages give the errors of the
//     Rust libraries they stand for their own FmtDebug. A fmt.Stringer
//     prints its String() text with `{}`.
//   - A func, a channel or an unsafe.Pointer prints its address with `{:?}`,
//     as a Rust fn pointer or raw pointer does.
//   - nil, which no Rust value is, prints as `<nil>` rather than panic inside
//     a log line: it marks a porting error.
//
// Not provided: f16 and f128, and the unstable FormattingOptions API (the
// options are kept privately, as Rust's Formatter keeps them).

// Display is Rust's fmt::Display: how a value prints with `{}`.
type Display interface {
	FmtDisplay(f *Formatter) error
}

// Debug is Rust's fmt::Debug for a type that wrote the impl by hand. A type
// whose Debug is derived needs no method; the formatter prints it from its
// registration.
type Debug interface {
	FmtDebug(f *Formatter) error
}

// NamedArg is an argument a format string refers to by name: `{error}`,
// `{count:>4}`, `{:width$}`.
type NamedArg struct {
	Name  string
	Value any
}

// Arg names an argument of Format. Rust captures `{name}` from the
// surrounding scope; Go passes it:
//
//	rs.Format("could not read {}: {error:#}", path, rs.Arg("error", err))
func Arg(name string, value any) NamedArg {
	return NamedArg{Name: name, Value: value}
}

// Char is a Rust `char` for formatting. A Go rune is an int32, so the
// formatter cannot otherwise tell 'a' from 97.
type Char rune

// Format is Rust's `format!`: format is a Rust format string, used exactly as
// it is written in the Rust source, positional arguments follow in order and
// named ones are passed with Arg.
//
// A mistake in the string or the arguments (an unknown name, too few
// arguments, a `{:x}` on a string) is a compile error in Rust; here it
// panics, and the rsfmt check in CI finds it without running the code.
func Format(format string, args ...any) string {
	return formatToString(format, args)
}

// ToString is `value.to_string()`: the value's Display output.
func ToString(value any) string {
	return fmtToString(value)
}

// DebugString is `format!("{:?}", value)`.
func DebugString(value any) string {
	return formatToString("{:?}", []any{value})
}

// PrettyDebugString is `format!("{:#?}", value)`.
func PrettyDebugString(value any) string {
	return formatToString("{:#?}", []any{value})
}

// Write is `write!(w, ...)`.
//
// A *strings.Builder stands for a Rust String, which is a fmt::Write: a
// formatting method that fails makes Write return its error (FmtError), with
// what was formatted before it already written. Any other w stands for an
// io::Write: the text goes to w in one write, where Rust writes it piece by
// piece (so a writer that fails part way through has received less of it
// than in Rust), and a formatting method that fails while w did not panics,
// as io::Write::write_fmt does.
func Write(w io.Writer, format string, args ...any) error {
	if builder, ok := w.(*strings.Builder); ok {
		return fmtWriteTemplate(fmtBuilderWrite{builder}, fmtParse(format), args)
	}
	_, err := io.WriteString(w, formatToStringForIo(format, args))
	return err
}

// Writeln is `writeln!(w, ...)`, which writes as Write does.
func Writeln(w io.Writer, format string, args ...any) error {
	if builder, ok := w.(*strings.Builder); ok {
		if err := fmtWriteTemplate(fmtBuilderWrite{builder}, fmtParse(format), args); err != nil {
			return err
		}
		builder.WriteByte('\n')
		return nil
	}
	_, err := io.WriteString(w, formatToStringForIo(format, args)+"\n")
	return err
}

// fmtBuilderWrite is `impl fmt::Write for String` over a strings.Builder.
type fmtBuilderWrite struct{ builder *strings.Builder }

func (w fmtBuilderWrite) writeStr(s string) error {
	w.builder.WriteString(s)
	return nil
}

func (w fmtBuilderWrite) writeChar(c rune) error {
	w.builder.WriteRune(c)
	return nil
}

// Print is `print!`. Like Rust's, it panics when standard output cannot be
// written.
func Print(format string, args ...any) {
	if _, err := io.WriteString(os.Stdout, formatToString(format, args)); err != nil {
		PanicAt(1, "failed printing to stdout: "+ToString(err))
	}
}

// Println is `println!`.
func Println(format string, args ...any) {
	if _, err := io.WriteString(os.Stdout, formatToString(format, args)+"\n"); err != nil {
		PanicAt(1, "failed printing to stdout: "+ToString(err))
	}
}

// Eprint is `eprint!`.
func Eprint(format string, args ...any) {
	if _, err := io.WriteString(os.Stderr, formatToString(format, args)); err != nil {
		PanicAt(1, "failed printing to stderr: "+ToString(err))
	}
}

// Eprintln is `eprintln!`.
func Eprintln(format string, args ...any) {
	if _, err := io.WriteString(os.Stderr, formatToString(format, args)+"\n"); err != nil {
		PanicAt(1, "failed printing to stderr: "+ToString(err))
	}
}
