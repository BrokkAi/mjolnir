// Port of base64 0.22.1 tests/encode.rs

package base64_test

import (
	"testing"

	"github.com/BrokkAi/mjolnir/go/crates/base64"
	"github.com/BrokkAi/mjolnir/go/rs"
)

func compareEncode(t *testing.T, expected string, target []byte) {
	t.Helper()
	if got := base64.EngineGeneralPurposeStandard.Encode(target); got != expected {
		t.Fatalf("assertion `left == right` failed\n  left: %q\n right: %q", expected, got)
	}
}

func TestItEncode_encode_all_ascii(t *testing.T) {
	ascii := []byte{}
	for b := range 128 {
		ascii = append(ascii, byte(b))
	}

	compareEncode(t,
		"AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8gISIjJCUmJygpKissLS4vMDEyMzQ1Njc4OTo7P"+
			"D0+P0BBQkNERUZHSElKS0xNTk9QUVJTVFVWV1hZWltcXV5fYGFiY2RlZmdoaWprbG1ub3BxcnN0dXZ3eHl6e3x9fn8"+
			"=",
		ascii,
	)
}

func TestItEncode_encode_all_bytes(t *testing.T) {
	bytes := []byte{}
	for b := range 256 {
		bytes = append(bytes, byte(b))
	}

	compareEncode(t,
		"AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8gISIjJCUmJygpKissLS4vMDEyMzQ1Njc4OTo7P"+
			"D0+P0BBQkNERUZHSElKS0xNTk9QUVJTVFVWV1hZWltcXV5fYGFiY2RlZmdoaWprbG1ub3BxcnN0dXZ3eHl6e3x9fn"+
			"+AgYKDhIWGh4iJiouMjY6PkJGSk5SVlpeYmZqbnJ2en6ChoqOkpaanqKmqq6ytrq+wsbKztLW2t7i5uru8vb6"+
			"/wMHCw8TFxsfIycrLzM3Oz9DR0tPU1dbX2Nna29zd3t/g4eLj5OXm5+jp6uvs7e7v8PHy8/T19vf4+fr7/P3+/w==",
		bytes,
	)
}

func TestItEncode_encode_all_bytes_url(t *testing.T) {
	bytes := []byte{}
	for b := range 256 {
		bytes = append(bytes, byte(b))
	}

	engine := base64.EngineGeneralPurposeNew(&base64.AlphabetUrlSafe, base64.EngineGeneralPurposePad)
	expected := "AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8gISIjJCUmJygpKissLS4vMDEyMzQ1Njc4OTo7PD0" +
		"-P0BBQkNERUZHSElKS0xNTk9QUVJTVFVWV1hZWltcXV5fYGFiY2RlZmdoaWprbG1ub3BxcnN0dXZ3eHl6e3x9fn" +
		"-AgYKDhIWGh4iJiouMjY6PkJGSk5SVlpeYmZqbnJ2en6ChoqOkpaanqKmqq6ytrq" +
		"-wsbKztLW2t7i5uru8vb6_wMHCw8TFxsfIycrLzM3Oz9DR0tPU1dbX2Nna29zd3t_g4eLj5OXm5-jp6uvs7e7v8PHy" +
		"8_T19vf4-fr7_P3-_w=="
	if got := engine.Encode(bytes); got != expected {
		t.Fatalf("assertion `left == right` failed\n  left: %q\n right: %q", expected, got)
	}
}

func TestItEncode_encoded_len_unpadded(t *testing.T) {
	for _, c := range [][2]int{{0, 0}, {2, 1}, {3, 2}, {4, 3}, {6, 4}, {7, 5}, {8, 6}, {10, 7}} {
		if got := base64.EncodedLen(c[1], false).Unwrap(); got != c[0] {
			t.Fatalf("encoded_len(%d, false) = %d, not %d", c[1], got, c[0])
		}
	}
}

func TestItEncode_encoded_len_padded(t *testing.T) {
	for _, c := range [][2]int{{0, 0}, {4, 1}, {4, 2}, {4, 3}, {8, 4}, {8, 5}, {8, 6}, {12, 7}} {
		if got := base64.EncodedLen(c[1], true).Unwrap(); got != c[0] {
			t.Fatalf("encoded_len(%d, true) = %d, not %d", c[1], got, c[0])
		}
	}
}

func TestItEncode_encoded_len_overflow(t *testing.T) {
	// usize::MAX is rs.UsizeMax, the largest Go int: the bound is that of
	// the type the length is held in, and the test's arithmetic is relative
	// to it.
	maxSize := rs.UsizeMax/4*3 + 2
	if maxSize%3 != 2 {
		t.Fatal("assertion `left == right` failed: 2 == max_size % 3")
	}
	if got := base64.EncodedLen(maxSize, false); got != rs.Some(rs.UsizeMax) {
		t.Fatalf("encoded_len(max_size, false) = %v", got)
	}
	if got := base64.EncodedLen(maxSize+1, false); got != rs.None[int]() {
		t.Fatalf("encoded_len(max_size + 1, false) = %v", got)
	}
}
