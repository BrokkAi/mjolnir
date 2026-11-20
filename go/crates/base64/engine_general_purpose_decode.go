// Port of base64 0.22.1 src/engine/general_purpose/decode.rs

package base64

import (
	"encoding/binary"

	"github.com/BrokkAi/mjolnir/go/rs"
)

// generalPurposeEstimate is `GeneralPurposeEstimate`, the decode estimate of
// the GeneralPurpose engine (doc-hidden in the crate).
type generalPurposeEstimate struct {
	// input len % 4
	rem                    int `rs:"rem"`
	conservativeDecodedLen int `rs:"conservative_decoded_len"`
}

var _ = rs.RegisterStruct[generalPurposeEstimate]("GeneralPurposeEstimate")

func generalPurposeEstimateNew(encodedLen int) generalPurposeEstimate {
	rem := encodedLen % 4
	remAsUsize := 0
	if rem > 0 {
		remAsUsize = 1
	}
	return generalPurposeEstimate{
		rem:                    rem,
		conservativeDecodedLen: (encodedLen/4 + remAsUsize) * 3,
	}
}

// DecodedLenEstimate is `impl DecodeEstimate for GeneralPurposeEstimate`.
func (self generalPurposeEstimate) DecodedLenEstimate() int {
	return self.conservativeDecodedLen
}

// Helper to avoid duplicating num_chunks calculation, which is costly on short inputs.
// Returns the decode metadata, or an error.
// We're on the fragile edge of compiler heuristics here. If this is not inlined, slow. If this is
// inlined(always), a different slow. plain ol' inline makes the benchmarks happiest at the moment,
// but this is fragile and the best setting changes with only minor code modifications.
func generalPurposeDecodeHelper(
	input []byte,
	estimate generalPurposeEstimate,
	output []byte,
	decodeTable *[256]byte,
	decodeAllowTrailingBits bool,
	paddingMode EngineDecodePaddingMode,
) (EngineDecodeMetadata, DecodeSliceError) {
	inputCompleteNonterminalQuadsLen, err :=
		generalPurposeCompleteQuadsLen(input, estimate.rem, len(output), decodeTable)
	if err != nil {
		return EngineDecodeMetadata{}, err
	}

	const unrolledInputChunkSize = 32
	const unrolledOutputChunkSize = unrolledInputChunkSize / 4 * 3

	inputCompleteQuadsAfterUnrolledChunksLen :=
		inputCompleteNonterminalQuadsLen % unrolledInputChunkSize

	inputUnrolledLoopLen :=
		inputCompleteNonterminalQuadsLen - inputCompleteQuadsAfterUnrolledChunksLen

	// chunks of 32 bytes
	for chunkIndex := 0; (chunkIndex+1)*unrolledInputChunkSize <= inputUnrolledLoopLen; chunkIndex++ {
		chunk := input[chunkIndex*unrolledInputChunkSize : (chunkIndex+1)*unrolledInputChunkSize]
		inputIndex := chunkIndex * unrolledInputChunkSize
		chunkOutput := output[chunkIndex*unrolledOutputChunkSize : (chunkIndex+1)*unrolledOutputChunkSize]

		if err := generalPurposeDecodeChunk8(
			chunk[0:8],
			inputIndex,
			decodeTable,
			chunkOutput[0:6],
		); err != nil {
			return EngineDecodeMetadata{}, DecodeSliceErrorFromDecodeError(err)
		}
		if err := generalPurposeDecodeChunk8(
			chunk[8:16],
			inputIndex+8,
			decodeTable,
			chunkOutput[6:12],
		); err != nil {
			return EngineDecodeMetadata{}, DecodeSliceErrorFromDecodeError(err)
		}
		if err := generalPurposeDecodeChunk8(
			chunk[16:24],
			inputIndex+16,
			decodeTable,
			chunkOutput[12:18],
		); err != nil {
			return EngineDecodeMetadata{}, DecodeSliceErrorFromDecodeError(err)
		}
		if err := generalPurposeDecodeChunk8(
			chunk[24:32],
			inputIndex+24,
			decodeTable,
			chunkOutput[18:24],
		); err != nil {
			return EngineDecodeMetadata{}, DecodeSliceErrorFromDecodeError(err)
		}
	}

	// remaining quads, except for the last possibly partial one, as it may have padding
	outputUnrolledLoopLen := inputUnrolledLoopLen / 4 * 3
	outputCompleteQuadLen := inputCompleteNonterminalQuadsLen / 4 * 3
	{
		outputAfterUnroll := output[outputUnrolledLoopLen:outputCompleteQuadLen]

		remaining := input[inputUnrolledLoopLen:inputCompleteNonterminalQuadsLen]
		for chunkIndex := 0; (chunkIndex+1)*4 <= len(remaining); chunkIndex++ {
			chunk := remaining[chunkIndex*4 : (chunkIndex+1)*4]
			chunkOutput := outputAfterUnroll[chunkIndex*3 : chunkIndex*3+3]

			if err := generalPurposeDecodeChunk4(
				chunk,
				inputUnrolledLoopLen+chunkIndex*4,
				decodeTable,
				chunkOutput,
			); err != nil {
				return EngineDecodeMetadata{}, DecodeSliceErrorFromDecodeError(err)
			}
		}
	}

	return generalPurposeDecodeSuffix(
		input,
		inputCompleteNonterminalQuadsLen,
		output,
		outputCompleteQuadLen,
		decodeTable,
		decodeAllowTrailingBits,
		paddingMode,
	)
}

// Returns the length of complete quads, except for the last one, even if it is complete.
//
// Returns an error if the output len is not big enough for decoding those complete quads, or if
// the input % 4 == 1, and that last byte is an invalid value other than a pad byte.
//
//   - `input` is the base64 input
//   - `input_len_rem` is input len % 4
//   - `output_len` is the length of the output slice
func generalPurposeCompleteQuadsLen(
	input []byte,
	inputLenRem int,
	outputLen int,
	decodeTable *[256]byte,
) (int, DecodeSliceError) {
	rs.DebugAssert(len(input)%4 == inputLenRem)

	// detect a trailing invalid byte, like a newline, as a user convenience
	if inputLenRem == 1 {
		lastByte := input[len(input)-1]
		// exclude pad bytes; might be part of padding that extends from earlier in the input
		if lastByte != libPadByte && decodeTable[lastByte] == generalPurposeInvalidValue {
			return 0, DecodeSliceErrorFromDecodeError(&DecodeError_InvalidByte{F0: len(input) - 1, F1: lastByte})
		}
	}

	// skip last quad, even if it's complete, as it may have padding
	skipLast := 0
	if inputLenRem == 0 {
		skipLast = 1
	}
	inputCompleteNonterminalQuadsLen := max(max(len(input)-inputLenRem, 0)-
		// if rem was 0, subtract 4 to avoid padding
		skipLast*4, 0)
	rs.DebugAssert(
		len(input) == 0 || (1 <= len(input)-inputCompleteNonterminalQuadsLen && len(input)-inputCompleteNonterminalQuadsLen <= 4),
	)

	// check that everything except the last quad handled by decode_suffix will fit
	if outputLen < inputCompleteNonterminalQuadsLen/4*3 {
		return 0, &DecodeSliceError_OutputSliceTooSmall{}
	}
	return inputCompleteNonterminalQuadsLen, nil
}

// Decode 8 bytes of input into 6 bytes of output.
//
// `input` is the 8 bytes to decode.
// `index_at_start_of_input` is the offset in the overall input (used for reporting errors
// accurately)
// `decode_table` is the lookup table for the particular base64 alphabet.
// `output` will have its first 6 bytes overwritten
// yes, really inline (worth 30-50% speedup)
func generalPurposeDecodeChunk8(
	input []byte,
	indexAtStartOfInput int,
	decodeTable *[256]byte,
	output []byte,
) DecodeError {
	morsel := decodeTable[input[0]]
	if morsel == generalPurposeInvalidValue {
		return &DecodeError_InvalidByte{F0: indexAtStartOfInput, F1: input[0]}
	}
	accum := uint64(morsel) << 58

	morsel = decodeTable[input[1]]
	if morsel == generalPurposeInvalidValue {
		return &DecodeError_InvalidByte{
			F0: indexAtStartOfInput + 1,
			F1: input[1],
		}
	}
	accum |= uint64(morsel) << 52

	morsel = decodeTable[input[2]]
	if morsel == generalPurposeInvalidValue {
		return &DecodeError_InvalidByte{
			F0: indexAtStartOfInput + 2,
			F1: input[2],
		}
	}
	accum |= uint64(morsel) << 46

	morsel = decodeTable[input[3]]
	if morsel == generalPurposeInvalidValue {
		return &DecodeError_InvalidByte{
			F0: indexAtStartOfInput + 3,
			F1: input[3],
		}
	}
	accum |= uint64(morsel) << 40

	morsel = decodeTable[input[4]]
	if morsel == generalPurposeInvalidValue {
		return &DecodeError_InvalidByte{
			F0: indexAtStartOfInput + 4,
			F1: input[4],
		}
	}
	accum |= uint64(morsel) << 34

	morsel = decodeTable[input[5]]
	if morsel == generalPurposeInvalidValue {
		return &DecodeError_InvalidByte{
			F0: indexAtStartOfInput + 5,
			F1: input[5],
		}
	}
	accum |= uint64(morsel) << 28

	morsel = decodeTable[input[6]]
	if morsel == generalPurposeInvalidValue {
		return &DecodeError_InvalidByte{
			F0: indexAtStartOfInput + 6,
			F1: input[6],
		}
	}
	accum |= uint64(morsel) << 22

	morsel = decodeTable[input[7]]
	if morsel == generalPurposeInvalidValue {
		return &DecodeError_InvalidByte{
			F0: indexAtStartOfInput + 7,
			F1: input[7],
		}
	}
	accum |= uint64(morsel) << 16

	var bytes [8]byte
	binary.BigEndian.PutUint64(bytes[:], accum)
	copy(output[:6], bytes[:6])

	return nil
}

// Like [decode_chunk_8] but for 4 bytes of input and 3 bytes of output.
func generalPurposeDecodeChunk4(
	input []byte,
	indexAtStartOfInput int,
	decodeTable *[256]byte,
	output []byte,
) DecodeError {
	morsel := decodeTable[input[0]]
	if morsel == generalPurposeInvalidValue {
		return &DecodeError_InvalidByte{F0: indexAtStartOfInput, F1: input[0]}
	}
	accum := uint32(morsel) << 26

	morsel = decodeTable[input[1]]
	if morsel == generalPurposeInvalidValue {
		return &DecodeError_InvalidByte{
			F0: indexAtStartOfInput + 1,
			F1: input[1],
		}
	}
	accum |= uint32(morsel) << 20

	morsel = decodeTable[input[2]]
	if morsel == generalPurposeInvalidValue {
		return &DecodeError_InvalidByte{
			F0: indexAtStartOfInput + 2,
			F1: input[2],
		}
	}
	accum |= uint32(morsel) << 14

	morsel = decodeTable[input[3]]
	if morsel == generalPurposeInvalidValue {
		return &DecodeError_InvalidByte{
			F0: indexAtStartOfInput + 3,
			F1: input[3],
		}
	}
	accum |= uint32(morsel) << 8

	var bytes [4]byte
	binary.BigEndian.PutUint32(bytes[:], accum)
	copy(output[:3], bytes[:3])

	return nil
}
