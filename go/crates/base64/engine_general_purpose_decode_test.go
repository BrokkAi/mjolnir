// Port of base64 0.22.1 src/engine/general_purpose/decode.rs

package base64

import (
	"testing"
)

func TestEngineGeneralPurposeDecode_decode_chunk_8_writes_only_6_bytes(t *testing.T) {
	input := []byte("Zm9vYmFy") // "foobar"
	output := []byte{0, 1, 2, 3, 4, 5, 6, 7}

	assertOk(t, generalPurposeDecodeChunk8(input[:], 0, &EngineGeneralPurposeStandard.decodeTable, output[:]))
	assertBytes(t, []byte{'f', 'o', 'o', 'b', 'a', 'r', 6, 7}, output)
}

func TestEngineGeneralPurposeDecode_decode_chunk_4_writes_only_3_bytes(t *testing.T) {
	input := []byte("Zm9v") // "foobar"
	output := []byte{0, 1, 2, 3}

	assertOk(t, generalPurposeDecodeChunk4(input[:], 0, &EngineGeneralPurposeStandard.decodeTable, output[:]))
	assertBytes(t, []byte{'f', 'o', 'o', 3}, output)
}

func TestEngineGeneralPurposeDecode_estimate_short_lengths(t *testing.T) {
	for _, c := range []struct {
		low, high          int
		decodedLenEstimate int
	}{
		{0, 0, 0},
		{1, 4, 3},
		{5, 8, 6},
		{9, 12, 9},
		{13, 16, 12},
		{17, 20, 15},
	} {
		for encodedLen := c.low; encodedLen <= c.high; encodedLen++ {
			estimate := generalPurposeEstimateNew(encodedLen)
			assertEq(t, c.decodedLenEstimate, estimate.DecodedLenEstimate())
		}
	}
}

func TestEngineGeneralPurposeDecode_estimate_via_u128_inflation(t *testing.T) {
	// cover both ends of usize, here a Go int (see engine_test.go)
	lengths := []int{}
	for encodedLen := range 1000 {
		lengths = append(lengths, encodedLen)
	}
	for encodedLen := int64(1<<63 - 1 - 1000); ; encodedLen++ {
		lengths = append(lengths, int(encodedLen))
		if encodedLen == 1<<63-1 {
			break
		}
	}
	for _, encodedLen := range lengths {
		// inflate to 128 bit type to be able to safely use the easy formulas
		len128 := uint64(encodedLen)

		estimate := generalPurposeEstimateNew(encodedLen)
		if (len128+3)/4*3 != uint64(estimate.conservativeDecodedLen) {
			t.Fatalf("enc len %d: estimate %d", encodedLen, estimate.conservativeDecodedLen)
		}
	}
}
