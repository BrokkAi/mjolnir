// Port of core 1.99.1 src/wtf8.rs and alloc 1.99.1 src/wtf8/mod.rs
//
// On Windows an OsStr is WTF-8: UTF-8 that may also hold unpaired surrogates,
// each as the three bytes generalized UTF-8 gives a surrogate code point. Go
// holds a Windows file name the same way (syscall.UTF16ToString has written
// unpaired surrogates as WTF-8 since Go 1.21), so a Go string read from
// Windows is the bytes a Rust OsString holds.
//
// Rust can only build a Wtf8 from UTF-16 or from a str, so its functions
// never see bytes that are not WTF-8. A Go string can hold anything. Each
// function here gives Rust's result for WTF-8 and says what it does with
// other bytes.

package rspath

import (
	"strings"
	"unicode/utf16"
	"unicode/utf8"
)

// wtf8Chunk is one step through a string taken as WTF-8: a run of valid
// UTF-8, then what ended it. That is an unpaired surrogate (three bytes), a
// sequence that is neither UTF-8 nor a surrogate (one to three bytes, as
// core::str::Utf8Chunk::invalid has them), or nothing at the end of the
// input.
type wtf8Chunk struct {
	valid       string
	surrogate   uint16
	isSurrogate bool
	invalid     string
}

// nextWtf8Chunk splits the next chunk off source. ok is false when source is
// empty.
func nextWtf8Chunk(source string) (chunk wtf8Chunk, rest string, ok bool) {
	utf8Part, remaining, ok := nextUtf8Chunk(source)
	if !ok {
		return wtf8Chunk{}, source, false
	}
	// UTF-8 stops at the first byte of a surrogate: 0xED may only be followed
	// by 0x80..=0x9F there.
	if utf8Part.invalid == "\xED" && len(remaining) >= 2 &&
		remaining[0] >= 0xA0 && remaining[0] <= 0xBF && remaining[1]&0xC0 == 0x80 {
		return wtf8Chunk{
			valid:       utf8Part.valid,
			surrogate:   decodeSurrogate(remaining[0], remaining[1]),
			isSurrogate: true,
		}, remaining[2:], true
	}
	return wtf8Chunk{valid: utf8Part.valid, invalid: utf8Part.invalid}, remaining, true
}

// decodeSurrogate is wtf8::decode_surrogate.
func decodeSurrogate(secondByte, thirdByte byte) uint16 {
	// The first byte is assumed to be 0xED
	return 0xD800 | (uint16(secondByte)&0x3F)<<6 | uint16(thirdByte)&0x3F
}

// decodeSurrogatePair is alloc::wtf8::decode_surrogate_pair.
func decodeSurrogatePair(lead, trail uint16) rune {
	return 0x10000 + ((rune(lead-0xD800) << 10) | rune(trail-0xDC00))
}

// wtf8FinalLeadSurrogate is Wtf8::final_lead_surrogate.
func wtf8FinalLeadSurrogate(bytes string) (uint16, bool) {
	n := len(bytes)
	if n >= 3 && bytes[n-3] == 0xED && bytes[n-2] >= 0xA0 && bytes[n-2] <= 0xAF {
		return decodeSurrogate(bytes[n-2], bytes[n-1]), true
	}
	return 0, false
}

// wtf8InitialTrailSurrogate is Wtf8::initial_trail_surrogate.
func wtf8InitialTrailSurrogate(bytes string) (uint16, bool) {
	if len(bytes) >= 3 && bytes[0] == 0xED && bytes[1] >= 0xB0 && bytes[1] <= 0xBF {
		return decodeSurrogate(bytes[1], bytes[2]), true
	}
	return 0, false
}

// wtf8PushWtf8 is Wtf8Buf::push_wtf8: it appends other to buf. A lead
// surrogate at the end of buf and a trail surrogate at the start of other
// become the supplementary code point they encode, which is why appending on
// Windows is not always concatenation.
func wtf8PushWtf8(buf, other string) string {
	lead, hasLead := wtf8FinalLeadSurrogate(buf)
	trail, hasTrail := wtf8InitialTrailSurrogate(other)
	if hasLead && hasTrail {
		// Replace newly paired surrogates by a supplementary code point.
		lenWithoutLeadSurrogate := len(buf) - 3
		otherWithoutTrailSurrogate := other[3:]
		var b strings.Builder
		// 4 bytes for the supplementary code point
		b.Grow(lenWithoutLeadSurrogate + 4 + len(otherWithoutTrailSurrogate))
		b.WriteString(buf[:lenWithoutLeadSurrogate])
		b.WriteRune(decodeSurrogatePair(lead, trail))
		b.WriteString(otherWithoutTrailSurrogate)
		return b.String()
	}
	return buf + other
}

// wtf8FromWide is Wtf8Buf::from_wide: potentially ill-formed UTF-16 to
// WTF-8. Surrogate pairs become one code point; an unpaired surrogate keeps
// its value as three bytes.
func wtf8FromWide(v []uint16) string {
	var b strings.Builder
	b.Grow(len(v))
	for i := 0; i < len(v); i++ {
		u := v[i]
		switch {
		case u < 0xD800 || u > 0xDFFF:
			// not a surrogate
			b.WriteRune(rune(u))
		case u >= 0xDC00:
			// a trailing surrogate
			writeCodePointUnchecked(&b, uint32(u))
		case i+1 < len(v) && v[i+1] >= 0xDC00 && v[i+1] <= 0xDFFF:
			// all ok, so lets decode it.
			u2 := v[i+1]
			i++
			b.WriteRune(rune((uint32(u&0x3ff)<<10 | uint32(u2&0x3ff)) + 0x10000))
		default:
			// eof, or not a trailing surrogate so we're not a valid
			// surrogate pair.
			writeCodePointUnchecked(&b, uint32(u))
		}
	}
	return b.String()
}

// writeCodePointUnchecked is Wtf8Buf::push_code_point_unchecked for a
// surrogate: core::char::encode_utf8_raw, which encodes a surrogate like any
// other three-byte code point where Go would write U+FFFD.
func writeCodePointUnchecked(b *strings.Builder, code uint32) {
	b.WriteByte(byte(code>>12&0x0F) | 0xE0)
	b.WriteByte(byte(code>>6&0x3F) | 0x80)
	b.WriteByte(byte(code&0x3F) | 0x80)
}

// wtf8EncodeWide is Wtf8::encode_wide collected: WTF-8 to potentially
// ill-formed UTF-16. A sequence that is not WTF-8, which Rust cannot hold,
// becomes U+FFFD.
func wtf8EncodeWide(s string) []uint16 {
	out := make([]uint16, 0, len(s))
	rest := s
	for {
		chunk, remaining, ok := nextWtf8Chunk(rest)
		if !ok {
			return out
		}
		rest = remaining
		for _, c := range chunk.valid {
			out = utf16.AppendRune(out, c)
		}
		switch {
		case chunk.isSurrogate:
			out = append(out, chunk.surrogate)
		case len(chunk.invalid) != 0:
			out = append(out, utf8.RuneError)
		}
	}
}

// wtf8ToStringLossy is Wtf8::to_string_lossy and the Display form of a Wtf8:
// every unpaired surrogate becomes one U+FFFD. A sequence that is not WTF-8
// becomes U+FFFD as String::from_utf8_lossy would make it.
func wtf8ToStringLossy(s string) string {
	if utf8.ValidString(s) {
		return s
	}
	var b strings.Builder
	b.Grow(len(s))
	rest := s
	for {
		chunk, remaining, ok := nextWtf8Chunk(rest)
		if !ok {
			return b.String()
		}
		rest = remaining
		b.WriteString(chunk.valid)
		if chunk.isSurrogate || len(chunk.invalid) != 0 {
			b.WriteString("�")
		}
	}
}

// wtf8Debug is the Debug form of a Wtf8, which is how a Windows OsStr prints
// with `{:?}`: escaped as a str is, with each unpaired surrogate as
// \u{d800}. A byte that is not WTF-8 is written as \xNN, as the Unix form
// writes it.
func wtf8Debug(s string) string {
	var f strings.Builder
	f.WriteByte('"')
	rest := s
	for {
		chunk, remaining, ok := nextWtf8Chunk(rest)
		if !ok {
			break
		}
		rest = remaining
		writeStrEscaped(&f, chunk.valid)
		if chunk.isSurrogate {
			// write!(formatter, "\\u{{{:x}}}", surrogate)
			writeEscapeUnicode(&f, rune(chunk.surrogate))
		}
		for i := 0; i < len(chunk.invalid); i++ {
			const hexDigits = "0123456789ABCDEF"
			b := chunk.invalid[i]
			f.WriteString(`\x`)
			f.WriteByte(hexDigits[b>>4])
			f.WriteByte(hexDigits[b&15])
		}
	}
	f.WriteByte('"')
	return f.String()
}
