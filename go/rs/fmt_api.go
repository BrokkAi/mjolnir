// Port of core 1.99.1 src/fmt (the contract; the implementation is in fmt_*.go).

package rs

import (
	"io"
	"os"
)

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
	return formatToString("{}", []any{value})
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
func Write(w io.Writer, format string, args ...any) error {
	_, err := io.WriteString(w, formatToString(format, args))
	return err
}

// Writeln is `writeln!(w, ...)`.
func Writeln(w io.Writer, format string, args ...any) error {
	_, err := io.WriteString(w, formatToString(format, args)+"\n")
	return err
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
