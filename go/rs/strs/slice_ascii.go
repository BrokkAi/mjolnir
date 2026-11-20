// Port of core 1.99.1 src/slice/ascii.rs (the methods Mjolnir calls on byte
// strings)

package strs

// Rust gives `[u8]` the ASCII methods it gives `str`. They live here, beside
// the string ones, under the prefix Bytes.

// BytesIsAscii is `<[u8]>::is_ascii`: whether every byte is ASCII.
func BytesIsAscii(b []byte) bool {
	for _, c := range b {
		if c >= 0x80 {
			return false
		}
	}
	return true
}

// BytesEqIgnoreAsciiCase is `<[u8]>::eq_ignore_ascii_case`: whether a and b
// are equal once the ASCII letters of both are lowercased.
func BytesEqIgnoreAsciiCase(a, b []byte) bool {
	if len(a) != len(b) {
		return false
	}
	for i := range a {
		if CharToAsciiLowercase(a[i]) != CharToAsciiLowercase(b[i]) {
			return false
		}
	}
	return true
}

// BytesToAsciiUppercase is `<[u8]>::to_ascii_uppercase`: a copy of b with its
// ASCII letters uppercased.
func BytesToAsciiUppercase(b []byte) []byte {
	out := make([]byte, len(b))
	for i, c := range b {
		out[i] = CharToAsciiUppercase(c)
	}
	return out
}

// BytesToAsciiLowercase is `<[u8]>::to_ascii_lowercase`: a copy of b with its
// ASCII letters lowercased.
func BytesToAsciiLowercase(b []byte) []byte {
	out := make([]byte, len(b))
	for i, c := range b {
		out[i] = CharToAsciiLowercase(c)
	}
	return out
}

// BytesTrimAsciiStart is `<[u8]>::trim_ascii_start`: b without leading ASCII
// whitespace (u8::is_ascii_whitespace). The result aliases b.
func BytesTrimAsciiStart(b []byte) []byte {
	for len(b) > 0 && IsAsciiWhitespace(b[0]) {
		b = b[1:]
	}
	return b
}

// BytesTrimAsciiEnd is `<[u8]>::trim_ascii_end`. The result aliases b.
func BytesTrimAsciiEnd(b []byte) []byte {
	for len(b) > 0 && IsAsciiWhitespace(b[len(b)-1]) {
		b = b[:len(b)-1]
	}
	return b
}

// BytesTrimAscii is `<[u8]>::trim_ascii`. The result aliases b.
func BytesTrimAscii(b []byte) []byte {
	return BytesTrimAsciiEnd(BytesTrimAsciiStart(b))
}
