// Port of alloctests 1.99.1 tests/fmt.rs

package rs

import (
	"math"
	"strconv"
	"strings"
	"testing"
	"unsafe"
)

// Not ported, having no Go counterpart: the checks that format! does not
// move out of its arguments or warn inside an unsafe block, that trailing
// commas are accepted, and the `RefCell { value: <borrowed> }` of a RefCell
// that is borrowed (rs.RefCell does not track borrows).

func TestAllocFmt_test_format(t *testing.T) {
	s := FormatArgs("Hello, {}!", "world").String()
	fmtExpectEq(t, s, "Hello, world!")
}

type fmtTestA struct{}
type fmtTestB struct{}
type fmtTestC struct{}
type fmtTestD struct{}

func (fmtTestA) FmtLowerHex(f *Formatter) error {
	return f.WriteStr("aloha")
}

func (fmtTestB) FmtUpperHex(f *Formatter) error {
	return f.WriteStr("adios")
}

func (fmtTestC) FmtDisplay(f *Formatter) error {
	return f.PadIntegral(true, "☃", "123")
}

func (fmtTestD) FmtBinary(f *Formatter) error {
	if err := f.WriteStr("aa"); err != nil {
		return err
	}
	if err := f.WriteChar('☃'); err != nil {
		return err
	}
	return f.WriteStr("bb")
}

func TestAllocFmt_test_format_macro_interface(t *testing.T) {
	A, B, C, D := fmtTestA{}, fmtTestB{}, fmtTestC{}, fmtTestD{}
	tt := func(a, b string) {
		t.Helper()
		fmtExpectEq(t, b, a)
	}

	// Various edge cases without formats
	tt(Format(""), "")
	tt(Format("hello"), "hello")
	tt(Format("hello {{"), "hello {")

	// default formatters should work
	tt(Format("{}", float32(1.0)), "1")
	tt(Format("{}", 1.0), "1")
	tt(Format("{}", "a"), "a")
	tt(Format("{}", strings.Clone("a")), "a")
	tt(Format("{}", false), "false")
	tt(Format("{}", Char('a')), "a")

	// At least exercise all the formats
	tt(Format("{}", true), "true")
	tt(Format("{}", Char('☃')), "☃")
	tt(Format("{}", 10), "10")
	tt(Format("{}", uint(10)), "10")
	tt(Format("{:?}", Char('☃')), "'☃'")
	tt(Format("{:?}", 10), "10")
	tt(Format("{:?}", uint(10)), "10")
	tt(Format("{:?}", "true"), "\"true\"")
	tt(Format("{:?}", "foo\nbar"), "\"foo\\nbar\"")
	tt(Format("{:?}", "foo\n\"bar\"\r\n'baz'\t\\qux\\"), `"foo\n\"bar\"\r\n'baz'\t\\qux\\"`)
	tt(Format("{:?}", "foo\x00bar\x01baz\u007fqux"), `"foo\0bar\u{1}baz\u{7f}qux"`)
	tt(Format("{:o}", uint(10)), "12")
	tt(Format("{:x}", uint(10)), "a")
	tt(Format("{:X}", uint(10)), "A")
	tt(Format("{}", "foo"), "foo")
	tt(Format("{}", strings.Clone("foo")), "foo")
	// Rust prints made-up addresses; these are real ones.
	pointee := new(int)
	address := strconv.FormatUint(uint64(uintptr(unsafe.Pointer(pointee))), 16)
	if strconv.IntSize == 32 {
		tt(Format("{:#p}", pointee), "0x"+strings.Repeat("0", 8-len(address))+address)
	} else {
		tt(Format("{:#p}", pointee), "0x"+strings.Repeat("0", 16-len(address))+address)
	}
	tt(Format("{:p}", pointee), "0x"+address)
	tt(Format("{A:x}", Arg("A", A)), "aloha")
	tt(Format("{B:X}", Arg("B", B)), "adios")
	tt(Format("foo {} ☃☃☃☃☃☃", "bar"), "foo bar ☃☃☃☃☃☃")
	tt(Format("{1} {0}", 0, 1), "1 0")
	tt(Format("{foo} {bar}", Arg("foo", 0), Arg("bar", 1)), "0 1")
	tt(Format("{foo} {1} {bar} {0}", 0, 1, Arg("foo", 2), Arg("bar", 3)), "2 1 3 0")
	tt(Format("{} {0}", "a"), "a a")
	tt(Format("{_foo}", Arg("_foo", uint(6))), "6")
	tt(Format("{foo_bar}", Arg("foo_bar", 1)), "1")
	tt(Format("{}", 5+5), "10")
	tt(Format("{C:#4}", Arg("C", C)), "☃123")
	tt(Format("{D:b}", Arg("D", D)), "aa☃bb")

	var a Debug = FmtFromFnOf(func(f *Formatter) error { return DebugFmt(1, f) })
	tt(Format("{a:?}", Arg("a", a)), "1")

	// Formatting strings and their arguments
	tt(Format("{}", "a"), "a")
	tt(Format("{:4}", "a"), "a   ")
	tt(Format("{:4}", "☃"), "☃   ")
	tt(Format("{:>4}", "a"), "   a")
	tt(Format("{:<4}", "a"), "a   ")
	tt(Format("{:^5}", "a"), "  a  ")
	tt(Format("{:^5}", "aa"), " aa  ")
	tt(Format("{:^4}", "a"), " a  ")
	tt(Format("{:^4}", "aa"), " aa ")
	tt(Format("{:.4}", "a"), "a")
	tt(Format("{:4.4}", "a"), "a   ")
	tt(Format("{:4.4}", "aaaaaaaaaaaaaaaaaa"), "aaaa")
	tt(Format("{:<4.4}", "aaaaaaaaaaaaaaaaaa"), "aaaa")
	tt(Format("{:>4.4}", "aaaaaaaaaaaaaaaaaa"), "aaaa")
	tt(Format("{:^4.4}", "aaaaaaaaaaaaaaaaaa"), "aaaa")
	tt(Format("{:>10.4}", "aaaaaaaaaaaaaaaaaa"), "      aaaa")
	tt(Format("{:2.4}", "aaaaa"), "aaaa")
	tt(Format("{:2.4}", "aaaa"), "aaaa")
	tt(Format("{:2.4}", "aaa"), "aaa")
	tt(Format("{:2.4}", "aa"), "aa")
	tt(Format("{:2.4}", "a"), "a ")
	tt(Format("{:0>2}", "a"), "0a")
	tt(Format("{:.*}", 4, "aaaaaaaaaaaaaaaaaa"), "aaaa")
	tt(Format("{:.1$}", "aaaaaaaaaaaaaaaaaa", 4), "aaaa")
	tt(Format("{:.a$}", "aaaaaaaaaaaaaaaaaa", Arg("a", 4)), "aaaa")
	tt(Format("{:._a$}", "aaaaaaaaaaaaaaaaaa", Arg("_a", 4)), "aaaa")
	tt(Format("{:1$}", "a", 4), "a   ")
	tt(Format("{1:0$}", 4, "a"), "a   ")
	tt(Format("{:a$}", "a", Arg("a", 4)), "a   ")
	tt(Format("{:-#}", "a"), "a")
	tt(Format("{:+#}", "a"), "a")
	tt(Format("{:/^10.8}", "1234567890"), "/12345678/")

	// Some float stuff
	tt(Format("{:}", float32(1.0)), "1")
	tt(Format("{:}", 1.0), "1")
	tt(Format("{:.3}", 1.0), "1.000")
	tt(Format("{:10.3}", 1.0), "     1.000")
	tt(Format("{:+10.3}", 1.0), "    +1.000")
	tt(Format("{:+10.3}", -1.0), "    -1.000")

	tt(Format("{:e}", float32(1.2345e6)), "1.2345e6")
	tt(Format("{:e}", 1.2345e6), "1.2345e6")
	tt(Format("{:E}", 1.2345e6), "1.2345E6")
	tt(Format("{:.3e}", 1.2345e6), "1.234e6")
	tt(Format("{:10.3e}", 1.2345e6), "   1.234e6")
	tt(Format("{:+10.3e}", 1.2345e6), "  +1.234e6")
	tt(Format("{:+10.3e}", -1.2345e6), "  -1.234e6")

	// Float edge cases
	negativeZero := math.Copysign(0, -1) // -0.0: Go's constant -0.0 is 0
	tt(Format("{}", negativeZero), "-0")
	tt(Format("{:?}", 0.0), "0.0")

	// sign aware zero padding
	tt(Format("{:<3}", 1), "1  ")
	tt(Format("{:>3}", 1), "  1")
	tt(Format("{:^3}", 1), " 1 ")
	tt(Format("{:03}", 1), "001")
	tt(Format("{:<03}", 1), "001")
	tt(Format("{:>03}", 1), "001")
	tt(Format("{:^03}", 1), "001")
	tt(Format("{:+03}", 1), "+01")
	tt(Format("{:<+03}", 1), "+01")
	tt(Format("{:>+03}", 1), "+01")
	tt(Format("{:^+03}", 1), "+01")
	tt(Format("{:#05x}", 1), "0x001")
	tt(Format("{:<#05x}", 1), "0x001")
	tt(Format("{:>#05x}", 1), "0x001")
	tt(Format("{:^#05x}", 1), "0x001")
	tt(Format("{:05}", 1.2), "001.2")
	tt(Format("{:<05}", 1.2), "001.2")
	tt(Format("{:>05}", 1.2), "001.2")
	tt(Format("{:^05}", 1.2), "001.2")
	tt(Format("{:05}", -1.2), "-01.2")
	tt(Format("{:<05}", -1.2), "-01.2")
	tt(Format("{:>05}", -1.2), "-01.2")
	tt(Format("{:^05}", -1.2), "-01.2")
	tt(Format("{:+05}", 1.2), "+01.2")
	tt(Format("{:<+05}", 1.2), "+01.2")
	tt(Format("{:>+05}", 1.2), "+01.2")
	tt(Format("{:^+05}", 1.2), "+01.2")

	// Ergonomic format_args!
	tt(Format("{0:x} {0:X}", 15), "f F")
	tt(Format("{0:x} {0:X} {}", 15), "f F 15")
	tt(Format("{:x}{0:X}{a:x}{:X}{1:x}{a:X}", 13, 14, Arg("a", 15)), "dDfEeF")
	tt(Format("{a:x} {a:X}", Arg("a", 15)), "f F")

	// And its edge cases
	tt(
		Format(
			"{a:.0$} {b:.0$} {0:.0$}\n{a:.c$} {b:.c$} {c:.c$}",
			4,
			Arg("a", "abcdefg"),
			Arg("b", "hijklmn"),
			Arg("c", 3),
		),
		"abcd hijk 4\nabc hij 3",
	)
	tt(Format("{a:.*} {0} {:.*}", 4, 3, "efgh", Arg("a", "abcdef")), "abcd 4 efg")
	tt(Format("{:.a$} {a} {a:#x}", "aaaaaa", Arg("a", 2)), "aa 2 0x2")

	// Test that pointers don't get truncated.
	{
		val := ^uintptr(0)
		exp := Format("{val:#x}", Arg("val", val))
		maxPointer := FmtFromFnOf(func(f *Formatter) error { return fmtPointerFmtInner(val, f) })
		tt(Format("{}", maxPointer), exp)
	}

	// Escaping
	tt(Format("{{"), "{")
	tt(Format("}}"), "}")

	// Display and Debug go through a Box, as a reference.
	boxed := new(int)
	*boxed = 3
	tt(Format("{a}", Arg("a", boxed)), "3")
	tt(Format("{a}", Arg("a", boxed)), "3")
}

// Basic test to make sure that we can invoke the `write!` macro with an
// fmt::Write instance.
func TestAllocFmt_test_write(t *testing.T) {
	var buf strings.Builder
	_ = Write(&buf, "{}", 3)
	{
		w := &buf
		_ = Write(w, "{foo}", Arg("foo", 4))
		_ = Write(w, "{}", "hello")
		_ = Writeln(w, "{}", "line")
		_ = Writeln(w, "{foo}", Arg("foo", "bar"))
		_, _ = w.WriteRune('☃')
		_, _ = w.WriteString("str")
	}

	fmtExpectEq(t, buf.String(), "34helloline\nbar\n☃str")
}

// Just make sure that the macros are defined, there's not really a lot that we
// can do with them just yet (to test the output)
func TestAllocFmt_test_print(t *testing.T) {
	Print("hi")
	Print("{:?}", []uint8{0})
	Println("hello")
	Println("this is a {}", "test")
	Println("{foo}", Arg("foo", "bar"))
}

// Just make sure that the macros are defined, there's not really a lot that we
// can do with them just yet (to test the output)
func TestAllocFmt_test_format_args(t *testing.T) {
	var buf strings.Builder
	{
		w := &buf
		_ = Write(w, "{}", FormatArgs("{}", 1))
		_ = Write(w, "{}", FormatArgs("test"))
		_ = Write(w, "{}", FormatArgs("{test}", Arg("test", 3)))
	}
	s := buf.String()
	fmtExpectEq(t, s, "1test3")

	s = FormatArgs("hello {}", "world").String()
	fmtExpectEq(t, s, "hello world")
	s = Format("{}: {}", "args were", FormatArgs("hello {}", "world"))
	fmtExpectEq(t, s, "args were: hello world")
}

// fmtTestCounter is counter_fn!: a function that returns 1, 2, 3, ... on
// successive calls.
func fmtTestCounter() func() uint32 {
	var counter uint32
	return func() uint32 {
		counter++
		return counter
	}
}

func TestAllocFmt_test_order(t *testing.T) {
	// Make sure format!() arguments are always evaluated in a left-to-right ordering
	count := fmtTestCounter()

	fmtExpectEq(t,
		Format(
			"{} {} {a} {b} {} {c}",
			count(),
			count(),
			count(),
			Arg("a", count()),
			Arg("b", count()),
			Arg("c", count()),
		),
		"1 2 4 5 3 6",
	)
}

func TestAllocFmt_test_once(t *testing.T) {
	// Make sure each argument are evaluated only once even though it may be
	// formatted multiple times
	count := fmtTestCounter()

	fmtExpectEq(t, Format("{0} {0} {0} {a} {a} {a}", count(), Arg("a", count())), "1 1 1 2 2 2")
}

func TestAllocFmt_test_refcell(t *testing.T) {
	refcell := RefCellNew(5)
	fmtExpectEq(t, Format("{refcell:?}", Arg("refcell", refcell)), "RefCell { value: 5 }")
	*refcell.BorrowMut() = 6
	fmtExpectEq(t, Format("{refcell:?}", Arg("refcell", &refcell)), "RefCell { value: 6 }")
}
