// Port of coretests 1.99.1 tests/str_lossy.rs

package strs

import "testing"

func TestStrLossy_chunks(t *testing.T) {
	assertChunks := func(input string, want ...[2]string) {
		t.Helper()
		chunks := Utf8Chunks([]byte(input))
		if len(chunks) != len(want) {
			t.Errorf("%q: %d chunks, want %d", input, len(chunks), len(want))
			return
		}
		for i, chunk := range chunks {
			eq(t, chunk.Valid(), want[i][0])
			eq(t, string(chunk.Invalid()), want[i][1])
		}
	}

	assertChunks("hello", [2]string{"hello", ""})
	assertChunks("ศไทย中华Việt Nam", [2]string{"ศไทย中华Việt Nam", ""})
	assertChunks(
		"Hello\xC2 There\xFF Goodbye",
		[2]string{"Hello", "\xC2"},
		[2]string{" There", "\xFF"},
		[2]string{" Goodbye", ""},
	)
	assertChunks(
		"Hello\xC0\x80 There\xE6\x83 Goodbye",
		[2]string{"Hello", "\xC0"},
		[2]string{"", "\x80"},
		[2]string{" There", "\xE6\x83"},
		[2]string{" Goodbye", ""},
	)
	assertChunks(
		"\xF5foo\xF5\x80bar",
		[2]string{"", "\xF5"},
		[2]string{"foo", "\xF5"},
		[2]string{"", "\x80"},
		[2]string{"bar", ""},
	)
	assertChunks(
		"\xF1foo\xF1\x80bar\xF1\x80\x80baz",
		[2]string{"", "\xF1"},
		[2]string{"foo", "\xF1\x80"},
		[2]string{"bar", "\xF1\x80\x80"},
		[2]string{"baz", ""},
	)
	assertChunks(
		"\xF4foo\xF4\x80bar\xF4\xBFbaz",
		[2]string{"", "\xF4"},
		[2]string{"foo", "\xF4\x80"},
		[2]string{"bar", "\xF4"},
		[2]string{"", "\xBF"},
		[2]string{"baz", ""},
	)
	assertChunks(
		"\xF0\x80\x80\x80foo\xF0\x90\x80\x80bar",
		[2]string{"", "\xF0"},
		[2]string{"", "\x80"},
		[2]string{"", "\x80"},
		[2]string{"", "\x80"},
		[2]string{"foo\U00010000bar", ""},
	)
	assertChunks(
		"\xED\xA0\x80foo\xED\xBF\xBFbar",
		[2]string{"", "\xED"},
		[2]string{"", "\xA0"},
		[2]string{"", "\x80"},
		[2]string{"foo", "\xED"},
		[2]string{"", "\xBF"},
		[2]string{"", "\xBF"},
		[2]string{"bar", ""},
	)
}
