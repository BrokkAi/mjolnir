// Port of percent-encoding 2.3.2 src/lib.rs and src/ascii_set.rs (tests)

package url

import (
	"bytes"
	"testing"

	"github.com/BrokkAi/mjolnir/go/rs"
	"github.com/BrokkAi/mjolnir/go/rs/strs"
)

func TestPercentEncoding_percent_encode_byte(t *testing.T) {
	for i := 0; i <= 0xFF; i++ {
		encoded := percentEncodingPercentEncodeByte(byte(i))
		itUnitAssertEq(t, encoded, rs.Format("%{:02X}", i))
	}
}

func TestPercentEncoding_percent_encode_accepts_ascii_set_ref(t *testing.T) {
	encoded := percentEncodingPercentEncodeNew([]byte("foo bar?"), &percentEncodingAsciiSetEmpty)
	itUnitAssertEq(t, percentEncodingCollect(encoded), "foo bar?")
}

// percentEncodingCollect is `.collect::<String>()`: the pieces Next yields,
// concatenated.
func percentEncodingCollect(encoded percentEncodingPercentEncode) string {
	var out []byte
	for {
		piece, ok := encoded.next()
		if !ok {
			return string(out)
		}
		out = append(out, piece...)
	}
}

func TestPercentEncoding_percent_encode_collect(t *testing.T) {
	encoded := percentEncodingPercentEncodeNew([]byte("foo bar?"), percentEncodingNonAlphanumeric)
	itUnitAssertEq(t, percentEncodingCollect(encoded), "foo%20bar%3F")

	encoded = percentEncodingPercentEncodeNew([]byte("\x00\x01\x02\x03"), percentEncodingControls)
	itUnitAssertEq(t, percentEncodingCollect(encoded), "%00%01%02%03")
}

func TestPercentEncoding_percent_encode_display(t *testing.T) {
	encoded := percentEncodingPercentEncodeNew([]byte("foo bar?"), percentEncodingNonAlphanumeric)
	itUnitAssertEq(t, encoded.String(), "foo%20bar%3F")
}

func TestPercentEncoding_percent_encode_cow(t *testing.T) {
	encoded := percentEncodingPercentEncodeNew([]byte("foo bar?"), percentEncodingNonAlphanumeric)
	itUnitAssertEq(t, encoded.String(), "foo%20bar%3F")
}

func TestPercentEncoding_utf8_percent_encode_accepts_ascii_set_ref(t *testing.T) {
	encoded := percentEncodingUtf8PercentEncode("foo bar?", &percentEncodingAsciiSetEmpty)
	itUnitAssertEq(t, percentEncodingCollect(encoded), "foo bar?")
}

func TestPercentEncoding_utf8_percent_encode(t *testing.T) {
	// The derived PartialEq of PercentEncode compares the bytes left and the
	// sets.
	left := percentEncodingUtf8PercentEncode("foo bar?", percentEncodingNonAlphanumeric)
	right := percentEncodingPercentEncodeNew([]byte("foo bar?"), percentEncodingNonAlphanumeric)
	itUnitAssert(t, bytes.Equal(left.bytes, right.bytes) && *left.asciiSet == *right.asciiSet)
}

func TestPercentEncoding_percent_decode(t *testing.T) {
	decoded, err := percentEncodingPercentDecodeNew([]byte("foo%20bar%3f")).decodeUtf8()
	itUnitOk(t, err)
	itUnitAssertEq(t, decoded, "foo bar?")
}

func TestPercentEncoding_percent_decode_str(t *testing.T) {
	decoded, err := percentEncodingPercentDecodeStr("foo%20bar%3f").decodeUtf8()
	itUnitOk(t, err)
	itUnitAssertEq(t, decoded, "foo bar?")
}

func TestPercentEncoding_percent_decode_collect(t *testing.T) {
	decoded := percentEncodingPercentDecodeNew([]byte("foo%20bar%3f"))
	var collected []byte
	for {
		b, ok := decoded.next()
		if !ok {
			break
		}
		collected = append(collected, b)
	}
	itUnitAssertEq(t, string(collected), "foo bar?")
}

func TestPercentEncoding_percent_decode_cow(t *testing.T) {
	decoded, owned := percentEncodingPercentDecodeNew([]byte("foo%20bar%3f")).collect()
	itUnitAssert(t, owned && string(decoded) == "foo bar?")

	decoded, owned = percentEncodingPercentDecodeNew([]byte("foo bar?")).collect()
	itUnitAssert(t, !owned && string(decoded) == "foo bar?")
}

func TestPercentEncoding_percent_decode_invalid_utf8(t *testing.T) {
	// Invalid UTF-8 sequence
	_, err := percentEncodingPercentDecodeNew([]byte("%00%9F%92%96")).decodeUtf8()
	decoded := err.(strs.Utf8Error)
	itUnitAssertEq(t, decoded.ValidUpTo(), 1)
	itUnitAssertEq(t, decoded.ErrorLen(), rs.Some(1))
}

func TestPercentEncoding_percent_decode_utf8_lossy(t *testing.T) {
	itUnitAssertEq(t,
		percentEncodingPercentDecodeNew([]byte("%F0%9F%92%96")).decodeUtf8Lossy(),
		"💖",
	)
}

func TestPercentEncoding_percent_decode_utf8_lossy_invalid_utf8(t *testing.T) {
	itUnitAssertEq(t,
		percentEncodingPercentDecodeNew([]byte("%00%9F%92%96")).decodeUtf8Lossy(),
		"\u0000���",
	)
}

func TestPercentEncodingAsciiSet_add_op(t *testing.T) {
	left := percentEncodingAsciiSetEmpty.add('A')
	right := percentEncodingAsciiSetEmpty.add('B')
	expected := percentEncodingAsciiSetEmpty.add('A').add('B')
	itUnitAssertEq(t, *left.union(right), *expected)
}

func TestPercentEncodingAsciiSet_not_op(t *testing.T) {
	set := percentEncodingAsciiSetEmpty.add('A').add('B')
	notSet := set.complement()
	itUnitAssert(t, !notSet.contains('A'))
	itUnitAssert(t, notSet.contains('C'))
}

// This test ensures that we can get the union of two sets as a constant value, which is
// useful for defining sets in a modular way.
func TestPercentEncodingAsciiSet_union(t *testing.T) {
	a := percentEncodingAsciiSetEmpty.add('A')
	b := percentEncodingAsciiSetEmpty.add('B')
	union := a.union(b)
	expected := percentEncodingAsciiSetEmpty.add('A').add('B')
	itUnitAssertEq(t, *union, *expected)
}

// This test ensures that we can get the complement of a set as a constant value, which is
// useful for defining sets in a modular way.
func TestPercentEncodingAsciiSet_complement(t *testing.T) {
	both := percentEncodingAsciiSetEmpty.add('A').add('B')
	complement := both.complement()
	itUnitAssert(t, !complement.contains('A'))
	itUnitAssert(t, !complement.contains('B'))
	itUnitAssert(t, complement.contains('C'))
}
