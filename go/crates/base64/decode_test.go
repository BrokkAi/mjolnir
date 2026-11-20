// Port of base64 0.22.1 src/decode.rs

package base64

import (
	"errors"
	"strings"
	"testing"

	"github.com/BrokkAi/mjolnir/go/rs"
)

func TestDecode_decode_into_nonempty_vec_doesnt_clobber_existing_prefix(t *testing.T) {
	origData := []byte{}
	encodedData := ""
	decodedWithPrefix := []byte{}
	decodedWithoutPrefix := []byte{}
	prefix := []byte{}

	rng := testRng(21)

	for range 10_000 {
		origData = origData[:0]
		encodedData = ""
		decodedWithPrefix = decodedWithPrefix[:0]
		decodedWithoutPrefix = decodedWithoutPrefix[:0]
		prefix = prefix[:0]

		inputLen := testUniform(rng, 0, 1000)

		origData = testFillRandom(origData, rng, inputLen)

		engine := randomEngine(rng)
		engine.EncodeString(origData, &encodedData)
		assertEncodeSanity(t, encodedData, engine.Config().EncodePadding(), inputLen)

		prefixLen := testUniform(rng, 0, 1000)

		// fill the buf with a prefix
		prefix = testFillRandom(prefix, rng, prefixLen)

		decodedWithPrefix = append(decodedWithPrefix[:0], prefix...)

		// decode into the non-empty buf
		assertOk(t, engine.DecodeVec([]byte(encodedData), &decodedWithPrefix))
		// also decode into the empty buf
		assertOk(t, engine.DecodeVec([]byte(encodedData), &decodedWithoutPrefix))

		assertEq(t,
			prefixLen+len(decodedWithoutPrefix),
			len(decodedWithPrefix),
		)
		assertBytes(t, origData, decodedWithoutPrefix)

		// append plain decode onto prefix
		prefix = append(prefix, decodedWithoutPrefix...)
		decodedWithoutPrefix = decodedWithoutPrefix[:0]

		assertBytes(t, prefix, decodedWithPrefix)
	}
}

func TestDecode_decode_slice_doesnt_clobber_existing_prefix_or_suffix(t *testing.T) {
	doDecodeSliceDoesntClobberExistingPrefixOrSuffix(t, 22, func(e *EngineGeneralPurpose, input []byte, output []byte) int {
		written, err := e.DecodeSlice(input, output)
		assertOk(t, err)
		return written
	})
}

func TestDecode_decode_slice_unchecked_doesnt_clobber_existing_prefix_or_suffix(t *testing.T) {
	doDecodeSliceDoesntClobberExistingPrefixOrSuffix(t, 23, func(e *EngineGeneralPurpose, input []byte, output []byte) int {
		written, err := e.DecodeSliceUnchecked(input, output)
		assertOk(t, err)
		return written
	})
}

func TestDecode_decode_engine_estimation_works_for_various_lengths(t *testing.T) {
	engine := EngineGeneralPurposeNew(&AlphabetStandard, EngineGeneralPurposeNoPad)
	for numPrefixQuads := range 100 {
		for _, suffix := range []string{"AA", "AAA", "AAAA"} {
			prefix := strings.Repeat("AAAA", numPrefixQuads)
			prefix += suffix
			// make sure no overflow (and thus a panic) occurs
			_, err := engine.Decode([]byte(prefix))
			assertOk(t, err)
		}
	}
}

func TestDecode_decode_slice_output_length_errors(t *testing.T) {
	for numQuads := 1; numQuads < 100; numQuads++ {
		input := strings.Repeat("AAAA", numQuads)
		vec := make([]byte, (numQuads-1)*3)
		_, err := EngineGeneralPurposeStandard.DecodeSlice([]byte(input), vec)
		assertEq[error](t, &DecodeSliceError_OutputSliceTooSmall{}, err)
		vec = append(vec, 0)
		_, err = EngineGeneralPurposeStandard.DecodeSlice([]byte(input), vec)
		assertEq[error](t, &DecodeSliceError_OutputSliceTooSmall{}, err)
		vec = append(vec, 0)
		_, err = EngineGeneralPurposeStandard.DecodeSlice([]byte(input), vec)
		assertEq[error](t, &DecodeSliceError_OutputSliceTooSmall{}, err)
		vec = append(vec, 0)
		// now it works
		written, err := EngineGeneralPurposeStandard.DecodeSlice([]byte(input), vec)
		assertOk(t, err)
		assertEq(t, numQuads*3, written)
	}
}

func doDecodeSliceDoesntClobberExistingPrefixOrSuffix(
	t *testing.T,
	seed uint64,
	callDecode func(*EngineGeneralPurpose, []byte, []byte) int,
) {
	t.Helper()
	origData := []byte{}
	encodedData := ""
	decodeBuf := []byte{}
	decodeBufCopy := []byte{}

	rng := testRng(seed)

	for range 10_000 {
		origData = origData[:0]
		encodedData = ""
		decodeBuf = decodeBuf[:0]
		decodeBufCopy = decodeBufCopy[:0]

		inputLen := testUniform(rng, 0, 1000)

		origData = testFillRandom(origData, rng, inputLen)

		engine := randomEngine(rng)
		engine.EncodeString(origData, &encodedData)
		assertEncodeSanity(t, encodedData, engine.Config().EncodePadding(), inputLen)

		// fill the buffer with random garbage, long enough to have some room before and after
		decodeBuf = testFillRandom(decodeBuf, rng, 5000)

		// keep a copy for later comparison
		decodeBufCopy = append(decodeBufCopy, decodeBuf...)

		offset := 1000

		// decode into the non-empty buf
		decodeBytesWritten :=
			callDecode(engine, []byte(encodedData), decodeBuf[offset:])

		assertEq(t, len(origData), decodeBytesWritten)
		assertBytes(t,
			origData,
			decodeBuf[offset:(offset+decodeBytesWritten)],
		)
		assertBytes(t, decodeBufCopy[0:offset], decodeBuf[0:offset])
		assertBytes(t,
			decodeBufCopy[offset+decodeBytesWritten:],
			decodeBuf[offset+decodeBytesWritten:],
		)
	}
}

// mod coverage_gaming. Its test of the deprecated free functions (decode,
// decode_engine, decode_engine_vec, decode_engine_slice) is not ported: the
// package leaves them out (doc.go).

func TestDecode_decode_error(t *testing.T) {
	_ = rs.Format("{:?}", rs.Clone[DecodeError](&DecodeError_InvalidPadding{}))
	_ = rs.Format(
		"{} {} {} {}",
		&DecodeError_InvalidByte{F0: 0, F1: 0},
		&DecodeError_InvalidLength{F0: 0},
		&DecodeError_InvalidLastSymbol{F0: 0, F1: 0},
		&DecodeError_InvalidPadding{},
	)
}

func TestDecode_decode_slice_error(t *testing.T) {
	_ = rs.Format("{:?}", rs.Clone[DecodeSliceError](&DecodeSliceError_OutputSliceTooSmall{}))
	_ = rs.Format(
		"{} {}",
		&DecodeSliceError_OutputSliceTooSmall{},
		&DecodeSliceError_DecodeError{F0: &DecodeError_InvalidPadding{}},
	)
	// `.source()` is errors.Unwrap.
	_ = errors.Unwrap(&DecodeSliceError_OutputSliceTooSmall{})
	_ = errors.Unwrap(&DecodeSliceError_DecodeError{F0: &DecodeError_InvalidPadding{}})
}

func TestDecode_decoded_len_est(t *testing.T) {
	assertEq(t, 3, DecodedLenEstimate(4))
}
