// Port of coretests 1.99.1 tests/fmt/mod.rs

package rs

import (
	"strconv"
	"strings"
	"testing"
	"unicode/utf8"
	"unsafe"
)

// Not ported: test_maybe_uninit_short (no MaybeUninit), formatting_options_ctor
// and formatting_options_flags (the unstable FormattingOptions API, which
// the port keeps private), and the parts of test_fmt_debug_of_raw_pointers
// about raw slice and trait-object pointers, which Go does not have: a Go
// slice or interface prints what it holds, as a Rust reference does.

func TestFmt_test_lifetime(t *testing.T) {
	// Trigger all different forms of expansion,
	// and check that each of them can be stored as a variable.
	a := FormatArgs("hello")
	a = FormatArgs("hello {a}", Arg("a", a))
	a = FormatArgs("hello {a:1}", Arg("a", a))
	a = FormatArgs("hello {a} {a:?}", Arg("a", a))
	fmtExpectEq(t, a.String(), "hello hello hello hello hello hello hello")

	// Check that temporaries as arguments are extended.
	b := FormatArgs("{}", "")
	c := FormatArgs("{}{}", "", "")
	fmtExpectEq(t, b.String(), "")
	fmtExpectEq(t, c.String(), "")

	// Without arguments, it should also work in consts.
	A := FormatArgs("hello")
	fmtExpectEq(t, A.String(), "hello")
}

func TestFmt_test_format_flags(t *testing.T) {
	// No residual flags left by pointer formatting
	p := unsafe.StringData("x")
	fmtExpectEq(t, Format("{:p} {:x}", p, 16), Format("{p:p} 10", Arg("p", p)))

	fmtExpectEq(t, Format("{: >3}", Char('a')), "  a")

	// Regression test for <https://github.com/rust-lang/rust/issues/50280#issuecomment-626035934>.
	show := func(a func() float32, b func(*[]int8) float32) {
		Println("the two pointers: {:p} {:p}", a, b)
	}
	show(func() float32 { return 1.0 }, func(*[]int8) float32 { return 2.0 })
}

func TestFmt_test_pointer_formats_data_pointer(t *testing.T) {
	b := []byte{}
	s := ""
	fmtExpectEq(t, Format("{s:p}", Arg("s", s)), Format("{:p}", s))
	fmtExpectEq(t, Format("{b:p}", Arg("b", b)), Format("{:p}", b))

	// &str and &[u8] are wide pointers: their address and their length.
	text := "héllo"
	fmtExpectEq(t, Format("{:p}", text), Format("Pointer {{ addr: {:p}, metadata: {} }}", unsafe.StringData(text), len(text)))
	bytes := []byte(text)
	fmtExpectEq(t, Format("{:p}", bytes), Format("Pointer {{ addr: {:p}, metadata: {} }}", &bytes[0], len(bytes)))
}

func TestFmt_test_fmt_debug_of_raw_pointers(t *testing.T) {
	value := int32(500)
	address := uintptr(unsafe.Pointer(&value))
	raw := unsafe.Pointer(&value)
	fmtExpectEq(t, Format("{:?}", raw), "0x"+strconv.FormatUint(uint64(address), 16))
}

func TestFmt_test_fmt_debug_of_mut_reference(t *testing.T) {
	x := uint32(0)

	fmtExpectEq(t, Format("{:?}", &x), "0")
}

func TestFmt_test_fmt_pointer(t *testing.T) {
	var p *uint8
	rc := new(int)
	arc := new(int)
	b := new(string)
	*b = "hi"

	_ = Format("{rc:p}{arc:p}{b:p}", Arg("rc", rc), Arg("arc", arc), Arg("b", b))

	if strconv.IntSize == 32 {
		fmtExpectEq(t, Format("{:#p}", p), "0x00000000")
	} else {
		fmtExpectEq(t, Format("{:#p}", p), "0x0000000000000000")
	}
	fmtExpectEq(t, Format("{:p}", p), "0x0")
}

// fmtTestBuf is the Buf of test_default_write_impls: a fmt::Write that
// implements only write_str. Rust gives it write_char and write_fmt by
// default; here writeChar is written out as that default.
type fmtTestBuf struct{ s strings.Builder }

func (b *fmtTestBuf) writeStr(s string) error {
	b.s.WriteString(s)
	return nil
}

func (b *fmtTestBuf) writeChar(c rune) error {
	return b.writeStr(string(utf8.AppendRune(nil, c)))
}

func TestFmt_test_default_write_impls(t *testing.T) {
	buf := &fmtTestBuf{}
	if err := buf.writeChar('a'); err != nil {
		t.Fatal(err)
	}

	fmtExpectEq(t, buf.s.String(), "a")

	buf = &fmtTestBuf{}
	if err := fmtWriteTemplate(buf, fmtParse("a"), nil); err != nil {
		t.Fatal(err)
	}

	fmtExpectEq(t, buf.s.String(), "a")
}

func TestFmt_test_estimated_capacity(t *testing.T) {
	capacity := func(format string) int { return fmtParse(format).estimatedCapacity() }
	if got := capacity(""); got != 0 {
		t.Errorf(`format_args!("").estimated_capacity() = %d, want 0`, got)
	}
	if got := capacity("{}"); got != 0 {
		t.Errorf(`format_args!("{}", "").estimated_capacity() = %d, want 0`, got)
	}
	if got := capacity("Hello"); got != 5 {
		t.Errorf(`format_args!("Hello").estimated_capacity() = %d, want 5`, got)
	}
	if got := capacity("Hello, {}!"); got != 16 {
		t.Errorf(`format_args!("Hello, {}!", "").estimated_capacity() = %d, want 16`, got)
	}
	if got := capacity("{}, hello!"); got != 0 {
		t.Errorf(`format_args!("{}, hello!", "World").estimated_capacity() = %d, want 0`, got)
	}
	if got := capacity("{}. 16-bytes piece"); got != 32 {
		t.Errorf(`format_args!("{}. 16-bytes piece", "World").estimated_capacity() = %d, want 32`, got)
	}
}

func TestFmt_pad_integral_resets(t *testing.T) {
	bar := FmtFromFnOf(func(f *Formatter) error {
		if err := DisplayFmt("1", f); err != nil {
			return err
		}
		if err := f.PadIntegral(true, "", "5"); err != nil {
			return err
		}
		return DisplayFmt("1", f)
	})

	fmtExpectEq(t, Format("{Bar:<03}", Arg("Bar", bar)), "1  0051  ")
}
