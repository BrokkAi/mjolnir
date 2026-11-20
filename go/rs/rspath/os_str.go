// Port of std 1.99.1 src/ffi/os_str.rs, src/sys/os_str/bytes.rs,
// src/sys/os_str/wtf8.rs, src/os/unix/ffi/os_str.rs and
// src/os/windows/ffi.rs

package rspath

import (
	"unicode/utf8"

	"github.com/BrokkAi/mjolnir/go/rs"
)

// An OsString and an OsStr are a Go string holding the encoded bytes: any
// bytes on Unix, WTF-8 on Windows. The conversions that only change the Rust
// type (OsStr::new, OsString::from, as_os_str, to_os_string, into_os_string,
// Path::new, PathBuf::from, to_path_buf, as_path) are the identity and most
// have no function here. `==` and ordering between two OsStr values compare
// the bytes, which is Go's == and < on the strings.

// OsStrToStr is OsStr::to_str: Some(s) if it is valid Unicode, None
// otherwise.
func (f Flavor) OsStrToStr(s string) rs.Option[string] {
	// Both encodings hold Unicode as UTF-8, and a WTF-8 surrogate is not
	// UTF-8: str::from_utf8 decides on either platform.
	return rs.OptionOf(s, utf8.ValidString(s))
}

// OsStrToStringLossy is OsStr::to_string_lossy: the text, with any
// non-Unicode sequences replaced by U+FFFD REPLACEMENT CHARACTER.
//
// On Unix every maximal broken UTF-8 sequence becomes one replacement
// character, as String::from_utf8_lossy makes it. On Windows every unpaired
// surrogate does.
func (f Flavor) OsStrToStringLossy(s string) string {
	if f.resolve() == Windows {
		return wtf8ToStringLossy(s)
	}
	return fromUtf8Lossy(s)
}

// OsStrDisplay is OsStr::display made into text. It is OsStrToStringLossy.
func (f Flavor) OsStrDisplay(s string) string {
	return f.OsStrToStringLossy(s)
}

// IntoStringError is the Err of OsString::into_string and
// PathBuf::into_string. Rust hands the OsString back; here the error holds
// it.
//
// Its Debug form is the OsString's, so `into_string().unwrap()` panics with
// the text Rust panics with.
type IntoStringError struct {
	// OsString is the string that is not valid Unicode.
	OsString string

	flavor Flavor
}

// Error returns the Debug form of the OsString. An OsString has no Display
// form in Rust.
func (e *IntoStringError) Error() string {
	return e.flavor.DebugString(e.OsString)
}

// FmtDebug is `impl Debug for OsString`.
func (e *IntoStringError) FmtDebug(f *rs.Formatter) error {
	return f.WriteStr(e.flavor.DebugString(e.OsString))
}

// OsStringIntoString is OsString::into_string: the String if the OsString
// contains valid Unicode data. On failure the error is an *IntoStringError
// holding the original OsString.
func (f Flavor) OsStringIntoString(s string) (string, error) {
	if utf8.ValidString(s) {
		return s, nil
	}
	return "", &IntoStringError{OsString: s, flavor: f.resolve()}
}

// OsStringPush is OsString::push: it extends buf with s.
//
// On Unix that is concatenation. On Windows a lead surrogate at the end of
// buf and a trail surrogate at the start of s become the one code point the
// pair encodes.
func (f Flavor) OsStringPush(buf *string, s string) {
	if f.resolve() == Windows {
		*buf = wtf8PushWtf8(*buf, s)
		return
	}
	*buf += s
}

// OsStrJoin is `[S]::join(&OsStr)` for a slice of OsStr: the parts pushed
// one after another with sep pushed between them.
func (f Flavor) OsStrJoin(parts []string, sep string) string {
	if len(parts) == 0 {
		return ""
	}
	joined := parts[0]
	for _, part := range parts[1:] {
		f.OsStringPush(&joined, sep)
		f.OsStringPush(&joined, part)
	}
	return joined
}

// OsStrToOsString is OsStr::to_os_string: an owned copy. A Go string is
// already one.
func OsStrToOsString(s string) string { return s }

// OsStrIsEmpty is OsStr::is_empty.
func OsStrIsEmpty(s string) bool { return len(s) == 0 }

// OsStrLen is OsStr::len: the length of the encoded bytes.
//
// On Windows this is the length of the WTF-8 form, not a count of UTF-16
// code units, as in Rust.
func OsStrLen(s string) int { return len(s) }

// OsStrAsEncodedBytes is OsStr::as_encoded_bytes: the bytes the OsStr is
// held in.
func OsStrAsEncodedBytes(s string) []byte { return []byte(s) }

// OsStringIntoEncodedBytes is OsString::into_encoded_bytes.
func OsStringIntoEncodedBytes(s string) []byte { return []byte(s) }

// OsStrFromEncodedBytesUnchecked is OsStr::from_encoded_bytes_unchecked: the
// OsStr held in bytes.
func OsStrFromEncodedBytesUnchecked(bytes []byte) string { return string(bytes) }

// OsStringFromEncodedBytesUnchecked is
// OsString::from_encoded_bytes_unchecked.
func OsStringFromEncodedBytesUnchecked(bytes []byte) string { return string(bytes) }

// OsStrAsBytes is std::os::unix::ffi::OsStrExt::as_bytes.
func OsStrAsBytes(s string) []byte { return []byte(s) }

// OsStrFromBytes is std::os::unix::ffi::OsStrExt::from_bytes.
func OsStrFromBytes(bytes []byte) string { return string(bytes) }

// OsStringFromVec is std::os::unix::ffi::OsStringExt::from_vec.
func OsStringFromVec(bytes []byte) string { return string(bytes) }

// OsStringIntoVec is std::os::unix::ffi::OsStringExt::into_vec.
func OsStringIntoVec(s string) []byte { return []byte(s) }

// OsStrEncodeWide is std::os::windows::ffi::OsStrExt::encode_wide collected:
// the OsStr as potentially ill-formed UTF-16, with no terminating zero. An
// unpaired surrogate comes back as the code unit it was.
func OsStrEncodeWide(s string) []uint16 { return wtf8EncodeWide(s) }

// OsStringFromWide is std::os::windows::ffi::OsStringExt::from_wide: an
// OsString from potentially ill-formed UTF-16. Unpaired surrogates are kept,
// so OsStrEncodeWide gives the same code units back.
func OsStringFromWide(wide []uint16) string { return wtf8FromWide(wide) }

// OsStrEqIgnoreAsciiCase is OsStr::eq_ignore_ascii_case: whether two strings
// are an ASCII case-insensitive match. Only A-Z and a-z fold; every other
// byte must be equal.
func OsStrEqIgnoreAsciiCase(a, b string) bool {
	if len(a) != len(b) {
		return false
	}
	for i := 0; i < len(a); i++ {
		if asciiLower(a[i]) != asciiLower(b[i]) {
			return false
		}
	}
	return true
}

// OsStrToAsciiLowercase is OsStr::to_ascii_lowercase: a copy with A-Z made
// a-z and every other byte unchanged.
func OsStrToAsciiLowercase(s string) string {
	bytes := []byte(s)
	for i, b := range bytes {
		bytes[i] = asciiLower(b)
	}
	return string(bytes)
}

// OsStrToAsciiUppercase is OsStr::to_ascii_uppercase: a copy with a-z made
// A-Z and every other byte unchanged.
func OsStrToAsciiUppercase(s string) string {
	bytes := []byte(s)
	for i, b := range bytes {
		if b >= 'a' && b <= 'z' {
			bytes[i] = b - ('a' - 'A')
		}
	}
	return string(bytes)
}

// OsStrMakeAsciiLowercase is OsStr::make_ascii_lowercase.
func OsStrMakeAsciiLowercase(s *string) { *s = OsStrToAsciiLowercase(*s) }

// OsStrMakeAsciiUppercase is OsStr::make_ascii_uppercase.
func OsStrMakeAsciiUppercase(s *string) { *s = OsStrToAsciiUppercase(*s) }

// OsStrIsAscii is OsStr::is_ascii: whether all bytes are in the ASCII range.
func OsStrIsAscii(s string) bool {
	for i := 0; i < len(s); i++ {
		if s[i] >= 0x80 {
			return false
		}
	}
	return true
}

func asciiLower(b byte) byte {
	if b >= 'A' && b <= 'Z' {
		return b + ('a' - 'A')
	}
	return b
}
