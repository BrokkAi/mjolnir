// Port of base64 0.22.1 src/encode.rs

package base64

import (
	"strings"
	"testing"
	"unicode/utf8"

	"github.com/BrokkAi/mjolnir/go/rs"
)

var urlSafeNoPadEngine = EngineGeneralPurposeNew(&AlphabetUrlSafe, EngineGeneralPurposeNoPad)

func TestEncode_encoded_size_correct_standard(t *testing.T) {
	assertEncodedLength(t, 0, 0, &EngineGeneralPurposeStandard, true)

	assertEncodedLength(t, 1, 4, &EngineGeneralPurposeStandard, true)
	assertEncodedLength(t, 2, 4, &EngineGeneralPurposeStandard, true)
	assertEncodedLength(t, 3, 4, &EngineGeneralPurposeStandard, true)

	assertEncodedLength(t, 4, 8, &EngineGeneralPurposeStandard, true)
	assertEncodedLength(t, 5, 8, &EngineGeneralPurposeStandard, true)
	assertEncodedLength(t, 6, 8, &EngineGeneralPurposeStandard, true)

	assertEncodedLength(t, 7, 12, &EngineGeneralPurposeStandard, true)
	assertEncodedLength(t, 8, 12, &EngineGeneralPurposeStandard, true)
	assertEncodedLength(t, 9, 12, &EngineGeneralPurposeStandard, true)

	assertEncodedLength(t, 54, 72, &EngineGeneralPurposeStandard, true)

	assertEncodedLength(t, 55, 76, &EngineGeneralPurposeStandard, true)
	assertEncodedLength(t, 56, 76, &EngineGeneralPurposeStandard, true)
	assertEncodedLength(t, 57, 76, &EngineGeneralPurposeStandard, true)

	assertEncodedLength(t, 58, 80, &EngineGeneralPurposeStandard, true)
}

func TestEncode_encoded_size_correct_no_pad(t *testing.T) {
	assertEncodedLength(t, 0, 0, &urlSafeNoPadEngine, false)

	assertEncodedLength(t, 1, 2, &urlSafeNoPadEngine, false)
	assertEncodedLength(t, 2, 3, &urlSafeNoPadEngine, false)
	assertEncodedLength(t, 3, 4, &urlSafeNoPadEngine, false)

	assertEncodedLength(t, 4, 6, &urlSafeNoPadEngine, false)
	assertEncodedLength(t, 5, 7, &urlSafeNoPadEngine, false)
	assertEncodedLength(t, 6, 8, &urlSafeNoPadEngine, false)

	assertEncodedLength(t, 7, 10, &urlSafeNoPadEngine, false)
	assertEncodedLength(t, 8, 11, &urlSafeNoPadEngine, false)
	assertEncodedLength(t, 9, 12, &urlSafeNoPadEngine, false)

	assertEncodedLength(t, 54, 72, &urlSafeNoPadEngine, false)

	assertEncodedLength(t, 55, 74, &urlSafeNoPadEngine, false)
	assertEncodedLength(t, 56, 75, &urlSafeNoPadEngine, false)
	assertEncodedLength(t, 57, 76, &urlSafeNoPadEngine, false)

	assertEncodedLength(t, 58, 78, &urlSafeNoPadEngine, false)
}

func TestEncode_encoded_size_overflow(t *testing.T) {
	assertEq(t, rs.None[int](), EncodedLen(rs.UsizeMax, true))
}

func TestEncode_encode_engine_string_into_nonempty_buffer_doesnt_clobber_prefix(t *testing.T) {
	origData := []byte{}
	prefix := ""
	encodedDataNoPrefix := ""
	encodedDataWithPrefix := ""
	decoded := []byte{}

	rng := testRng(11)

	for range 10_000 {
		origData = origData[:0]
		prefix = ""
		encodedDataNoPrefix = ""
		encodedDataWithPrefix = ""
		decoded = decoded[:0]

		inputLen := testUniform(rng, 0, 1000)

		origData = testFillRandom(origData, rng, inputLen)

		prefixLen := testUniform(rng, 0, 1000)
		// getting convenient random single-byte printable chars that aren't base64 is
		// annoying
		prefix += strings.Repeat("#", prefixLen)
		encodedDataWithPrefix += prefix

		engine := randomEngine(rng)
		engine.EncodeString(origData, &encodedDataNoPrefix)
		engine.EncodeString(origData, &encodedDataWithPrefix)

		assertEq(t,
			len(encodedDataNoPrefix)+prefixLen,
			len(encodedDataWithPrefix),
		)
		assertEncodeSanity(t,
			encodedDataNoPrefix,
			engine.Config().EncodePadding(),
			inputLen,
		)
		assertEncodeSanity(t,
			encodedDataWithPrefix[prefixLen:],
			engine.Config().EncodePadding(),
			inputLen,
		)

		// append plain encode onto prefix
		prefix += encodedDataNoPrefix

		assertEq(t, prefix, encodedDataWithPrefix)

		assertOk(t, engine.DecodeVec([]byte(encodedDataNoPrefix), &decoded))
		assertBytes(t, origData, decoded)
	}
}

func TestEncode_encode_engine_slice_into_nonempty_buffer_doesnt_clobber_suffix(t *testing.T) {
	origData := []byte{}
	encodedData := []byte{}
	encodedDataOriginalState := []byte{}
	decoded := []byte{}

	rng := testRng(12)

	for range 10_000 {
		origData = origData[:0]
		encodedData = encodedData[:0]
		encodedDataOriginalState = encodedDataOriginalState[:0]
		decoded = decoded[:0]

		inputLen := testUniform(rng, 0, 1000)

		origData = testFillRandom(origData, rng, inputLen)

		// plenty of existing garbage in the encoded buffer
		encodedData = testFillRandom(encodedData, rng, 10*inputLen)

		encodedDataOriginalState = append(encodedDataOriginalState, encodedData...)

		engine := randomEngine(rng)

		encodedSize := EncodedLen(inputLen, engine.Config().EncodePadding()).Unwrap()

		written, err := engine.EncodeSlice(origData, encodedData)
		assertOk(t, err)
		assertEq(t, encodedSize, written)

		assertEncodeSanity(t,
			string(encodedData[0:encodedSize]),
			engine.Config().EncodePadding(),
			inputLen,
		)

		assertBytes(t,
			encodedData[encodedSize:],
			encodedDataOriginalState[encodedSize:],
		)

		assertOk(t, engine.DecodeVec(encodedData[0:encodedSize], &decoded))
		assertBytes(t, origData, decoded)
	}
}

func TestEncode_encode_to_slice_random_valid_utf8(t *testing.T) {
	input := []byte{}
	output := []byte{}

	rng := testRng(13)

	for range 10_000 {
		input = input[:0]
		output = output[:0]

		inputLen := testUniform(rng, 0, 1000)

		input = testFillRandom(input, rng, inputLen)

		config := randomConfig(rng)
		engine := randomEngine(rng)

		// fill up the output buffer with garbage
		encodedSize := EncodedLen(inputLen, config.EncodePadding()).Unwrap()
		output = testFillRandom(output, rng, encodedSize)

		origOutputBuf := append([]byte(nil), output...)

		bytesWritten := engine.InternalEncode(input, output)

		// make sure the part beyond bytes_written is the same garbage it was before
		assertBytes(t, origOutputBuf[bytesWritten:], output[bytesWritten:])

		// make sure the encoded bytes are UTF-8
		if !utf8.Valid(output[0:bytesWritten]) {
			t.Fatal("encoded bytes are not UTF-8")
		}
	}
}

func TestEncode_encode_with_padding_random_valid_utf8(t *testing.T) {
	input := []byte{}
	output := []byte{}

	rng := testRng(14)

	for range 10_000 {
		input = input[:0]
		output = output[:0]

		inputLen := testUniform(rng, 0, 1000)

		input = testFillRandom(input, rng, inputLen)

		engine := randomEngine(rng)

		// fill up the output buffer with garbage
		encodedSize := EncodedLen(inputLen, engine.Config().EncodePadding()).Unwrap()
		output = testFillRandom(output, rng, encodedSize+1000)

		origOutputBuf := append([]byte(nil), output...)

		encodeEncodeWithPadding(input, output[0:encodedSize], engine, encodedSize)

		// make sure the part beyond b64 is the same garbage it was before
		assertBytes(t, origOutputBuf[encodedSize:], output[encodedSize:])

		// make sure the encoded bytes are UTF-8
		if !utf8.Valid(output[0:encodedSize]) {
			t.Fatal("encoded bytes are not UTF-8")
		}
	}
}

func TestEncode_add_padding_random_valid_utf8(t *testing.T) {
	output := []byte{}

	rng := testRng(15)

	// cover our bases for length % 4
	for unpaddedOutputLen := range 20 {
		output = output[:0]

		// fill output with random
		output = testFillRandom(output, rng, 100)

		origOutputBuf := append([]byte(nil), output...)

		bytesWritten := encodeAddPadding(unpaddedOutputLen, output)

		// make sure the part beyond bytes_written is the same garbage it was before
		assertBytes(t, origOutputBuf[bytesWritten:], output[bytesWritten:])

		// make sure the encoded bytes are UTF-8
		if !utf8.Valid(output[0:bytesWritten]) {
			t.Fatal("encoded bytes are not UTF-8")
		}
	}
}

func assertEncodedLength(
	t *testing.T,
	inputLen int,
	encLen int,
	engine Engine,
	padded bool,
) {
	t.Helper()
	assertEq(t, encLen, EncodedLen(inputLen, padded).Unwrap())

	rng := testRng(uint64(inputLen))
	bytes := testFillRandom(nil, rng, inputLen)

	encoded := engine.Encode(bytes)
	assertEncodeSanity(t, encoded, padded, inputLen)

	assertEq(t, encLen, len(encoded))
}

func TestEncode_encode_imap(t *testing.T) {
	imap := EngineGeneralPurposeNew(&AlphabetImapMutf7, EngineGeneralPurposeNoPad)
	standard := EngineGeneralPurposeNew(&AlphabetStandard, EngineGeneralPurposeNoPad)
	assertEq(t,
		imap.Encode([]byte("\xFB\xFF")),
		strings.ReplaceAll(standard.Encode([]byte("\xFB\xFF")), "/", ","),
	)
}
