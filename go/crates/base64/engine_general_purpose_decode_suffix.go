// Port of base64 0.22.1 src/engine/general_purpose/decode_suffix.rs

package base64

import (
	"github.com/BrokkAi/mjolnir/go/rs"
)

// Decode the last 0-4 bytes, checking for trailing set bits and padding per the provided
// parameters.
//
// Returns the decode metadata representing the total number of bytes decoded, including the ones
// indicated as already written by `output_index`.
func generalPurposeDecodeSuffix(
	input []byte,
	inputIndex int,
	output []byte,
	outputIndex int,
	decodeTable *[256]byte,
	decodeAllowTrailingBits bool,
	paddingMode EngineDecodePaddingMode,
) (EngineDecodeMetadata, DecodeSliceError) {
	rs.DebugAssert((len(input) - inputIndex) <= 4)

	// Decode any leftovers that might not be a complete input chunk of 4 bytes.
	// Use a u32 as a stack-resident 4 byte buffer.
	morselsInLeftover := 0
	paddingBytesCount := 0
	// offset from input_index
	firstPaddingOffset := 0
	lastSymbol := byte(0)
	var morsels [4]byte

	for leftoverIndex, b := range input[inputIndex:] {
		// '=' padding
		if b == libPadByte {
			// There can be bad padding bytes in a few ways:
			// 1 - Padding with non-padding characters after it
			// 2 - Padding after zero or one characters in the current quad (should only
			//     be after 2 or 3 chars)
			// 3 - More than two characters of padding. If 3 or 4 padding chars
			//     are in the same quad, that implies it will be caught by #2.
			//     If it spreads from one quad to another, it will be an invalid byte
			//     in the first quad.
			// 4 - Non-canonical padding -- 1 byte when it should be 2, etc.
			//     Per config, non-canonical but still functional non- or partially-padded base64
			//     may be treated as an error condition.

			if leftoverIndex < 2 {
				// Check for error #2.
				// Either the previous byte was padding, in which case we would have already hit
				// this case, or it wasn't, in which case this is the first such error.
				rs.DebugAssert(
					leftoverIndex == 0 || (leftoverIndex == 1 && paddingBytesCount == 0),
				)
				badPaddingIndex := inputIndex + leftoverIndex
				return EngineDecodeMetadata{}, DecodeSliceErrorFromDecodeError(&DecodeError_InvalidByte{F0: badPaddingIndex, F1: b})
			}

			if paddingBytesCount == 0 {
				firstPaddingOffset = leftoverIndex
			}

			paddingBytesCount += 1
			continue
		}

		// Check for case #1.
		// To make '=' handling consistent with the main loop, don't allow
		// non-suffix '=' in trailing chunk either. Report error as first
		// erroneous padding.
		if paddingBytesCount > 0 {
			return EngineDecodeMetadata{}, DecodeSliceErrorFromDecodeError(
				&DecodeError_InvalidByte{F0: inputIndex + firstPaddingOffset, F1: libPadByte},
			)
		}

		lastSymbol = b

		// can use up to 8 * 6 = 48 bits of the u64, if last chunk has no padding.
		// Pack the leftovers from left to right.
		morsel := decodeTable[b]
		if morsel == generalPurposeInvalidValue {
			return EngineDecodeMetadata{}, DecodeSliceErrorFromDecodeError(&DecodeError_InvalidByte{F0: inputIndex + leftoverIndex, F1: b})
		}

		morsels[morselsInLeftover] = morsel
		morselsInLeftover += 1
	}

	// If there was 1 trailing byte, and it was valid, and we got to this point without hitting
	// an invalid byte, now we can report invalid length
	if len(input) != 0 && morselsInLeftover < 2 {
		return EngineDecodeMetadata{}, DecodeSliceErrorFromDecodeError(&DecodeError_InvalidLength{F0: inputIndex + morselsInLeftover})
	}

	switch paddingMode {
	case EngineDecodePaddingMode_Indifferent:
		/* everything we care about was already checked */
	case EngineDecodePaddingMode_RequireCanonical:
		// allow empty input
		if (paddingBytesCount+morselsInLeftover)%4 != 0 {
			return EngineDecodeMetadata{}, DecodeSliceErrorFromDecodeError(&DecodeError_InvalidPadding{})
		}
	case EngineDecodePaddingMode_RequireNone:
		if paddingBytesCount > 0 {
			// check at the end to make sure we let the cases of padding that should be InvalidByte
			// get hit
			return EngineDecodeMetadata{}, DecodeSliceErrorFromDecodeError(&DecodeError_InvalidPadding{})
		}
	default:
		panic(rs.Unreachable(paddingMode))
	}

	// When encoding 1 trailing byte (e.g. 0xFF), 2 base64 bytes ("/w") are needed.
	// / is the symbol for 63 (0x3F, bottom 6 bits all set) and w is 48 (0x30, top 2 bits
	// of bottom 6 bits set).
	// When decoding two symbols back to one trailing byte, any final symbol higher than
	// w would still decode to the original byte because we only care about the top two
	// bits in the bottom 6, but would be a non-canonical encoding. So, we calculate a
	// mask based on how many bits are used for just the canonical encoding, and optionally
	// error if any other bits are set. In the example of one encoded byte -> 2 symbols,
	// 2 symbols can technically encode 12 bits, but the last 4 are non-canonical, and
	// useless since there are no more symbols to provide the necessary 4 additional bits
	// to finish the second original byte.

	leftoverBytesToAppend := morselsInLeftover * 6 / 8
	// Put the up to 6 complete bytes as the high bytes.
	// Gain a couple percent speedup from nudging these ORs to use more ILP with a two-way split.
	leftoverNum := (uint32(morsels[0]) << 26) |
		(uint32(morsels[1]) << 20) |
		(uint32(morsels[2]) << 14) |
		(uint32(morsels[3]) << 8)

	// if there are bits set outside the bits we care about, last symbol encodes trailing bits that
	// will not be included in the output
	mask := ^uint32(0) >> (leftoverBytesToAppend * 8)
	if !decodeAllowTrailingBits && (leftoverNum&mask) != 0 {
		// last morsel is at `morsels_in_leftover` - 1
		return EngineDecodeMetadata{}, DecodeSliceErrorFromDecodeError(&DecodeError_InvalidLastSymbol{
			F0: inputIndex + morselsInLeftover - 1,
			F1: lastSymbol,
		})
	}

	// Strangely, this approach benchmarks better than writing bytes one at a time,
	// or copy_from_slice into output.
	for range leftoverBytesToAppend {
		hiByte := byte(leftoverNum >> 24)
		leftoverNum <<= 8
		if outputIndex >= len(output) {
			return EngineDecodeMetadata{}, &DecodeSliceError_OutputSliceTooSmall{}
		}
		output[outputIndex] = hiByte
		outputIndex += 1
	}

	paddingOffset := rs.None[int]()
	if paddingBytesCount > 0 {
		paddingOffset = rs.Some(inputIndex + firstPaddingOffset)
	}
	return engineDecodeMetadataNew(
		outputIndex,
		paddingOffset,
	), nil
}
