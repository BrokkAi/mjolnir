// Port of alloc 1.99.1 src/wtf8/tests.rs
//
// A Wtf8Buf is a Go string here, read as WTF-8 by the Windows flavour. The
// tests of what this package gives a Windows OsString are ported:
// conversion from and to UTF-16, pushing, into_string, the lossy
// conversion, Display, Debug and the ASCII case functions. Not ported are
// the tests of CodePoint (code_point_*), of the is_known_utf8 flag (which is
// a cache, and is where wtf8buf_new, wtf8buf_from_str, wtf8buf_from_string,
// wtf8buf_push_str, wtf8buf_push_char, wtf8_to_owned, wtf8_clone_into,
// wobbled_wtf8_plus_bytes_isnt_utf8, wobbled_wtf8_plus_str_isnt_utf8 and
// unwobbly_wtf8_plus_utf8_is_utf8 have their only assertions that are not
// about plain bytes), and of slicing, truncation and code point boundaries
// (wtf8buf_truncate*, wtf8_slice*, wtf8_len, wtf8_ascii_byte_at,
// wtf8_code_points, wtf8buf_from_iterator, wtf8buf_extend,
// wtf8buf_as_slice, wtf8_from_str, wtf8_encode_wide_size_hint,
// wtf8_encode_wide_debug, wtf8_valid_utf8_boundaries,
// wtf8_utf8_boundary_*), which have no function here.

package rspath

import (
	"slices"
	"testing"

	"github.com/BrokkAi/mjolnir/go/rs"
)

// codePoint is the WTF-8 form of one code point, surrogates included: what
// Wtf8Buf::push(CodePoint::from_u32(value)) appends when nothing joins.
func codePoint(value uint32) string {
	if value >= 0xD800 && value <= 0xDFFF {
		return string([]byte{byte(value>>12&0x0F) | 0xE0, byte(value>>6&0x3F) | 0x80, byte(value&0x3F) | 0x80})
	}
	return string(rune(value))
}

func TestWtf8_wtf8buf_from_wide(t *testing.T) {
	check := func(wide []uint16, expected string) {
		t.Helper()
		if got := OsStringFromWide(wide); got != expected {
			t.Errorf("from_wide(%x) = %q, expected %q", wide, got, expected)
		}
	}
	check([]uint16{}, "")
	check([]uint16{0x61, 0xE9, 0x20, 0xD83D, 0xDCA9}, "a\xC3\xA9 \xF0\x9F\x92\xA9")
	check([]uint16{0x61, 0xE9, 0x20, 0xD83D, 0xD83D, 0xDCA9}, "a\xC3\xA9 \xED\xA0\xBD\xF0\x9F\x92\xA9")
	check([]uint16{0xD800}, "\xED\xA0\x80")
	check([]uint16{0xDBFF}, "\xED\xAF\xBF")
	check([]uint16{0xDC00}, "\xED\xB0\x80")
	check([]uint16{0xDFFF}, "\xED\xBF\xBF")
}

func TestWtf8_wtf8buf_push(t *testing.T) {
	push := func(parts ...uint32) string {
		str := ""
		for _, part := range parts {
			Windows.OsStringPush(&str, codePoint(part))
		}
		return str
	}
	check := func(got, expected string) {
		t.Helper()
		if got != expected {
			t.Errorf("got %q, expected %q", got, expected)
		}
	}

	str := "aé "
	Windows.OsStringPush(&str, "💩")
	check(str, "a\xC3\xA9 \xF0\x9F\x92\xA9")

	check(push(0xD83D, 0xDCA9), "\xF0\x9F\x92\xA9")                // lead, trail: Magic!
	check(push(0xD83D, 0x20, 0xDCA9), "\xED\xA0\xBD \xED\xB2\xA9") // lead, not surrogate, trail
	check(push(0xD800, 0xDBFF), "\xED\xA0\x80\xED\xAF\xBF")        // lead, lead
	check(push(0xD800, 0xE000), "\xED\xA0\x80\xEE\x80\x80")        // lead, not surrogate
	check(push(0xD7FF, 0xDC00), "\xED\x9F\xBF\xED\xB0\x80")        // not surrogate, trail
	check(push(0x61, 0xDC00), "\x61\xED\xB0\x80")                  // not surrogate, < 3 bytes, trail
	check(push(0xDC00), "\xED\xB0\x80")                            // trail
}

func TestWtf8_wtf8buf_push_wtf8(t *testing.T) {
	push := func(parts ...string) string {
		str := ""
		for _, part := range parts {
			Windows.OsStringPush(&str, part)
		}
		return str
	}
	check := func(got, expected string) {
		t.Helper()
		if got != expected {
			t.Errorf("got %q, expected %q", got, expected)
		}
	}

	check(push("aé", " 💩"), "a\xC3\xA9 \xF0\x9F\x92\xA9")

	check(push("\xED\xA0\xBD", "\xED\xB2\xA9"), "\xF0\x9F\x92\xA9")               // lead, trail: Magic!
	check(push("\xED\xA0\xBD", " ", "\xED\xB2\xA9"), "\xED\xA0\xBD \xED\xB2\xA9") // lead, not surrogate, trail
	check(push("\xED\xA0\x80", "\xED\xAF\xBF"), "\xED\xA0\x80\xED\xAF\xBF")       // lead, lead
	check(push("\xED\xA0\x80", "\xEE\x80\x80"), "\xED\xA0\x80\xEE\x80\x80")       // lead, not surrogate
	check(push("\xED\x9F\xBF", "\xED\xB0\x80"), "\xED\x9F\xBF\xED\xB0\x80")       // not surrogate, trail
	check(push("a", "\xED\xB0\x80"), "\x61\xED\xB0\x80")                          // not surrogate, < 3 bytes, trail
	check(push("\xED\xB0\x80"), "\xED\xB0\x80")                                   // trail

	// On Unix pushing is concatenation, surrogate bytes or not.
	unix := "\xED\xA0\xBD"
	Unix.OsStringPush(&unix, "\xED\xB2\xA9")
	check(unix, "\xED\xA0\xBD\xED\xB2\xA9")
}

func TestWtf8_wtf8buf_into_string(t *testing.T) {
	str := "aé 💩"
	if got, err := Windows.OsStringIntoString(str); err != nil || got != "aé 💩" {
		t.Errorf("into_string(%q) = %q, %v", str, got, err)
	}
	Windows.OsStringPush(&str, codePoint(0xD800))
	_, err := Windows.OsStringIntoString(str)
	if err == nil {
		t.Fatalf("into_string(%q) is Ok", str)
	}
	if failed := err.(*IntoStringError); failed.OsString != str {
		t.Errorf("the error holds %q, expected %q", failed.OsString, str)
	}
}

func TestWtf8_wtf8buf_into_string_lossy(t *testing.T) {
	str := "aé 💩"
	if got := Windows.ToStringLossy(str); got != "aé 💩" {
		t.Errorf("%q", got)
	}
	Windows.OsStringPush(&str, codePoint(0xD800))
	if got := Windows.ToStringLossy(str); got != "aé 💩\uFFFD" {
		t.Errorf("%q", got)
	}
}

func TestWtf8_wtf8buf_show(t *testing.T) {
	str := "a\té \u007f💩\r"
	Windows.OsStringPush(&str, codePoint(0xD800))
	if got := Windows.DebugString(str); got != "\"a\\té \\u{7f}\U0001f4a9\\r\\u{d800}\"" {
		t.Errorf("%s", got)
	}
}

func TestWtf8_wtf8buf_show_str(t *testing.T) {
	// assert_eq!(format!("{text:?}"), format!("{string:?}"));
	text := "a\té 💩\r"
	if got := Windows.DebugString(text); got != `"a\té 💩\r"` {
		t.Errorf("%s", got)
	}
	if formatterReady() {
		if got, want := Windows.DebugString(text), rs.Format("{text:?}", rs.Arg("text", text)); got != want {
			t.Errorf("%s, and as a str %s", got, want)
		}
	}
}

func TestWtf8_wtf8_as_str(t *testing.T) {
	if Windows.ToStr("") != rs.Some("") {
		t.Errorf("as_str(\"\") is not Ok")
	}
	if Windows.ToStr("aé 💩") != rs.Some("aé 💩") {
		t.Errorf("as_str is not Ok")
	}
	if Windows.ToStr(codePoint(0xD800)).IsSome() {
		t.Errorf("as_str of a surrogate is Ok")
	}
}

func TestWtf8_wtf8_to_string_lossy(t *testing.T) {
	if got := Windows.ToStringLossy(""); got != "" {
		t.Errorf("%q", got)
	}
	if got := Windows.ToStringLossy("aé 💩"); got != "aé 💩" {
		t.Errorf("%q", got)
	}
	str := "aé 💩" + codePoint(0xD800)
	if got := Windows.ToStringLossy(str); got != "aé 💩\uFFFD" {
		t.Errorf("%q", got)
	}
}

func TestWtf8_wtf8_display(t *testing.T) {
	d := func(b string) string {
		return Windows.Display(b)
	}

	if got := d(""); got != "" {
		t.Errorf("%q", got)
	}
	if got := d("aé 💩"); got != "aé 💩" {
		t.Errorf("%q", got)
	}

	str := "aé 💩" + codePoint(0xD800)
	if got := d(str); got != "aé 💩\uFFFD" {
		t.Errorf("%q", got)
	}
}

func TestWtf8_wtf8_encode_wide(t *testing.T) {
	str := "aé "
	Windows.OsStringPush(&str, codePoint(0xD83D))
	Windows.OsStringPush(&str, "💩")
	if got := OsStrEncodeWide(str); !slices.Equal(got, []uint16{0x61, 0xE9, 0x20, 0xD83D, 0xD83D, 0xDCA9}) {
		t.Errorf("%x", got)
	}
}

func TestWtf8_wtf8_make_ascii_lowercase(t *testing.T) {
	lowercase := ""
	OsStrMakeAsciiLowercase(&lowercase)
	if lowercase != "" {
		t.Errorf("%q", lowercase)
	}

	lowercase = "GrEeN gRaPeS! 🍇"
	OsStrMakeAsciiLowercase(&lowercase)
	if lowercase != "green grapes! \xf0\x9f\x8d\x87" {
		t.Errorf("%q", lowercase)
	}

	lowercase = "\xED\xA0\x80"
	OsStrMakeAsciiLowercase(&lowercase)
	if lowercase != "\xED\xA0\x80" {
		t.Errorf("%q", lowercase)
	}
}

func TestWtf8_wtf8_make_ascii_uppercase(t *testing.T) {
	uppercase := ""
	OsStrMakeAsciiUppercase(&uppercase)
	if uppercase != "" {
		t.Errorf("%q", uppercase)
	}

	uppercase = "GrEeN gRaPeS! 🍇"
	OsStrMakeAsciiUppercase(&uppercase)
	if uppercase != "GREEN GRAPES! \xf0\x9f\x8d\x87" {
		t.Errorf("%q", uppercase)
	}

	uppercase = "\xED\xA0\x80"
	OsStrMakeAsciiUppercase(&uppercase)
	if uppercase != "\xED\xA0\x80" {
		t.Errorf("%q", uppercase)
	}
}
