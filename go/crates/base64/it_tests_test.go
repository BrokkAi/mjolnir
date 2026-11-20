// Port of base64 0.22.1 tests/tests.rs

package base64_test

import (
	"bytes"
	"math/rand/v2"
	"strings"
	"testing"

	"github.com/BrokkAi/mjolnir/go/crates/base64"
	"github.com/BrokkAi/mjolnir/go/rs"
)

// generate random contents of the specified length and test encode/decode roundtrip
func roundtripRandom(
	t *testing.T,
	byteBuf *[]byte,
	strBuf *string,
	engine base64.Engine,
	byteLen int,
	approxValuesPerByte uint8,
	maxRounds uint64,
) {
	t.Helper()
	// let the short ones be short but don't let it get too crazy large
	numRounds := calculateNumberOfRounds(byteLen, approxValuesPerByte, maxRounds)
	// rand's SmallRng seeded from entropy in the crate; a seeded PCG here.
	r := rand.New(rand.NewPCG(uint64(byteLen), 0x7473))
	decodeBuf := []byte{}

	for range numRounds {
		*byteBuf = (*byteBuf)[:0]
		*strBuf = ""
		decodeBuf = decodeBuf[:0]
		for len(*byteBuf) < byteLen {
			*byteBuf = append(*byteBuf, byte(r.Uint32()))
		}

		engine.EncodeString(*byteBuf, strBuf)
		if err := engine.DecodeVec([]byte(*strBuf), &decodeBuf); err != nil {
			t.Fatalf("called `Result::unwrap()` on an `Err` value: %s", err)
		}

		if !bytes.Equal(*byteBuf, decodeBuf) {
			t.Fatalf("%q decodes to %q, not %q", *strBuf, decodeBuf, *byteBuf)
		}
	}
}

func calculateNumberOfRounds(byteLen int, approxValuesPerByte uint8, max uint64) uint64 {
	// don't overflow
	prod := uint64(approxValuesPerByte)

	for range byteLen {
		if prod > max {
			return max
		}

		// prod.saturating_mul(prod)
		if prod != 0 && prod > ^uint64(0)/prod {
			prod = ^uint64(0)
		} else {
			prod = prod * prod
		}
	}

	return prod
}

func TestItTests_roundtrip_random_short_standard(t *testing.T) {
	byteBuf := []byte{}
	strBuf := ""

	for inputLen := range 40 {
		roundtripRandom(t, &byteBuf, &strBuf, &base64.EngineGeneralPurposeStandard, inputLen, 4, 10000)
	}
}

func TestItTests_roundtrip_random_with_fast_loop_standard(t *testing.T) {
	byteBuf := []byte{}
	strBuf := ""

	for inputLen := 40; inputLen < 100; inputLen++ {
		roundtripRandom(t, &byteBuf, &strBuf, &base64.EngineGeneralPurposeStandard, inputLen, 4, 1000)
	}
}

func TestItTests_roundtrip_random_short_no_padding(t *testing.T) {
	byteBuf := []byte{}
	strBuf := ""

	engine := base64.EngineGeneralPurposeNew(&base64.AlphabetStandard, base64.EngineGeneralPurposeNoPad)
	for inputLen := range 40 {
		roundtripRandom(t, &byteBuf, &strBuf, &engine, inputLen, 4, 10000)
	}
}

func TestItTests_roundtrip_random_no_padding(t *testing.T) {
	byteBuf := []byte{}
	strBuf := ""

	engine := base64.EngineGeneralPurposeNew(&base64.AlphabetStandard, base64.EngineGeneralPurposeNoPad)

	for inputLen := 40; inputLen < 100; inputLen++ {
		roundtripRandom(t, &byteBuf, &strBuf, &engine, inputLen, 4, 1000)
	}
}

func TestItTests_roundtrip_decode_trailing_10_bytes(t *testing.T) {
	// This is a special case because we decode 8 byte blocks of input at a time as much as we can,
	// ideally unrolled to 32 bytes at a time, in stages 1 and 2. Since we also write a u64's worth
	// of bytes (8) to the output, we always write 2 garbage bytes that then will be overwritten by
	// the NEXT block. However, if the next block only contains 2 bytes, it will decode to 1 byte,
	// and therefore be too short to cover up the trailing 2 garbage bytes. Thus, we have stage 3
	// to handle that case.

	for numQuads := range 25 {
		s := strings.Repeat("ABCD", numQuads)
		s += "EFGHIJKLZg"

		engine := base64.EngineGeneralPurposeNew(&base64.AlphabetStandard, base64.EngineGeneralPurposeNoPad)
		decoded := rs.Unwrap(engine.Decode([]byte(s)))
		if len(decoded) != numQuads*3+7 {
			t.Fatalf("%q decodes to %d bytes", s, len(decoded))
		}

		if engine.Encode(decoded) != s {
			t.Fatalf("%q does not encode back", s)
		}
	}
}

// display_wrapper_matches_normal_encode is not ported: it tests
// display::Base64Display, which this package leaves out (doc.go).

func TestItTests_encode_engine_slice_error_when_buffer_too_small(t *testing.T) {
	for numTriples := 1; numTriples < 100; numTriples++ {
		input := strings.Repeat("AAA", numTriples)
		vec := make([]byte, (numTriples-1)*4)
		for range 4 {
			_, err := base64.EngineGeneralPurposeStandard.EncodeSlice([]byte(input), vec)
			if err != base64.EncodeSliceError_OutputSliceTooSmall {
				t.Fatalf("%d triples into %d bytes: %v", numTriples, len(vec), err)
			}
			vec = append(vec, 0)
		}
		written := rs.Unwrap(base64.EngineGeneralPurposeStandard.EncodeSlice([]byte(input), vec))
		if written != numTriples*4 {
			t.Fatalf("%d triples: wrote %d", numTriples, written)
		}
	}
}
