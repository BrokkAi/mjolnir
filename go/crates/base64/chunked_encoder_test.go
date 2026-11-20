// Port of base64 0.22.1 src/chunked_encoder.rs

package base64

import (
	"strconv"
	"testing"
)

func TestChunkedEncoder_chunked_encode_empty(t *testing.T) {
	assertEq(t, "", chunkedEncodeStr([]byte{}, EngineGeneralPurposePad))
}

func TestChunkedEncoder_chunked_encode_intermediate_fast_loop(t *testing.T) {
	// > 8 bytes input, will enter the pretty fast loop
	assertEq(t, "Zm9vYmFyYmF6cXV4", chunkedEncodeStr([]byte("foobarbazqux"), EngineGeneralPurposePad))
}

func TestChunkedEncoder_chunked_encode_fast_loop(t *testing.T) {
	// > 32 bytes input, will enter the uber fast loop
	assertEq(t,
		"Zm9vYmFyYmF6cXV4cXV1eGNvcmdlZ3JhdWx0Z2FycGx5eg==",
		chunkedEncodeStr([]byte("foobarbazquxquuxcorgegraultgarplyz"), EngineGeneralPurposePad),
	)
}

func TestChunkedEncoder_chunked_encode_slow_loop_only(t *testing.T) {
	// < 8 bytes input, slow loop only
	assertEq(t, "Zm9vYmFy", chunkedEncodeStr([]byte("foobar"), EngineGeneralPurposePad))
}

func TestChunkedEncoder_chunked_encode_matches_normal_encode_random_string_sink(t *testing.T) {
	chunkedEncodeMatchesNormalEncodeRandom(t)
}

// chunkedEncodeMatchesNormalEncodeRandom is the test over a SinkTestHelper,
// with the one helper the crate has: the StringSink one.
func chunkedEncodeMatchesNormalEncodeRandom(t *testing.T) {
	t.Helper()
	inputBuf := []byte{}
	outputBuf := ""
	rng := testRng(31)

	for range 20_000 {
		inputBuf = inputBuf[:0]
		outputBuf = ""

		bufLen := testUniform(rng, 1, 10_000)
		inputBuf = testFillRandom(inputBuf, rng, bufLen)

		engine := randomEngine(rng)

		chunkEncodedString := stringSinkEncodeToString(engine, inputBuf)
		engine.EncodeString(inputBuf, &outputBuf)

		assertEq(t, outputBuf, chunkEncodedString, "input len="+strconv.Itoa(bufLen))
	}
}

func chunkedEncodeStr(bytes []byte, config EngineGeneralPurposeConfig) string {
	s := ""

	sink := chunkedEncoderStringSinkNew(&s)
	engine := EngineGeneralPurposeNew(&AlphabetStandard, config)
	encoder := chunkedEncoderChunkedEncoderNew(&engine)
	if err := encoder.encode(bytes, sink); err != nil {
		panic(err)
	}

	return s
}

// stringSinkEncodeToString is StringSinkTestHelper::encode_to_string.
func stringSinkEncodeToString(engine Engine, bytes []byte) string {
	encoder := chunkedEncoderChunkedEncoderNew(engine)
	s := ""
	sink := chunkedEncoderStringSinkNew(&s)
	if err := encoder.encode(bytes, sink); err != nil {
		panic(err)
	}

	return s
}
