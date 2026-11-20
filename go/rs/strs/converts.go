// Port of core 1.99.1 src/str/converts.rs

package strs

import "unicode/utf8"

// FromUtf8 is str::from_utf8 (`std::str::from_utf8(v)`): v as a string when
// it is valid UTF-8, and otherwise a Utf8Error saying where it stops being
// valid.
//
// `String::from_utf8(v)` reports the same failure with a different error
// type, which also holds the bytes: that one is StringFromUtf8.
func FromUtf8(v []byte) (string, error) {
	if err, ok := validateUtf8(v); !ok {
		return "", err
	}
	return string(v), nil
}

// validateUtf8 is run_utf8_validation behind a fast path: Go's utf8.Valid
// accepts exactly the strings Rust's validation does (the well-formed
// sequences of the Unicode standard), so only input it rejects is walked
// again to find the position and length of the error.
func validateUtf8(v []byte) (Utf8Error, bool) {
	if utf8.Valid(v) {
		return Utf8Error{}, true
	}
	return runUtf8Validation(v)
}
