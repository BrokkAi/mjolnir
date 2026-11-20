// Port of base64 0.22.1 src/engine/general_purpose/mod.rs

package base64

import (
	"encoding/binary"

	"github.com/BrokkAi/mjolnir/go/rs"
)

// Provides the [GeneralPurpose] engine and associated config types.

const generalPurposeInvalidValue byte = 255

// A general-purpose base64 engine.
//
//   - It uses no vector CPU instructions, so it will work on any system.
//   - It is reasonably fast (~2-3GiB/s).
//   - It is not constant-time, though, so it is vulnerable to timing side-channel attacks. For loading cryptographic keys, etc, it is suggested to use the forthcoming constant-time implementation.
//
// `base64::engine::GeneralPurpose`, also reached as
// `base64::engine::general_purpose::GeneralPurpose`.
type EngineGeneralPurpose struct {
	encodeTable [64]byte                   `rs:"encode_table"`
	decodeTable [256]byte                  `rs:"decode_table"`
	config      EngineGeneralPurposeConfig `rs:"config"`
}

var _ = rs.RegisterStruct[EngineGeneralPurpose]("GeneralPurpose")

// Create a `GeneralPurpose` engine from an [Alphabet].
//
// While not very expensive to initialize, ideally these should be cached
// if the engine will be used repeatedly.
func EngineGeneralPurposeNew(alphabet *AlphabetAlphabet, config EngineGeneralPurposeConfig) EngineGeneralPurpose {
	return EngineGeneralPurpose{
		encodeTable: generalPurposeEncodeTable(alphabet),
		decodeTable: generalPurposeDecodeTable(alphabet),
		config:      config,
	}
}

var _ Engine = (*EngineGeneralPurpose)(nil)

// InternalEncode is Engine::internal_encode.
func (self *EngineGeneralPurpose) InternalEncode(input []byte, output []byte) int {
	inputIndex := 0

	const blocksPerFastLoop = 4
	const lowSixBits uint64 = 0x3F

	// we read 8 bytes at a time (u64) but only actually consume 6 of those bytes. Thus, we need
	// 2 trailing bytes to be available to read..
	lastFastIndex := max(len(input)-(blocksPerFastLoop*6+2), 0)
	outputIndex := 0

	if lastFastIndex > 0 {
		for inputIndex <= lastFastIndex {
			// Major performance wins from letting the optimizer do the bounds check once, mostly
			// on the output side
			inputChunk := input[inputIndex:(inputIndex + (blocksPerFastLoop*6 + 2))]
			outputChunk := output[outputIndex:(outputIndex + blocksPerFastLoop*8)]

			// Hand-unrolling for 32 vs 16 or 8 bytes produces yields performance about equivalent
			// to unsafe pointer code on a Xeon E5-1650v3. 64 byte unrolling was slightly better for
			// large inputs but significantly worse for 50-byte input, unsurprisingly. I suspect
			// that it's a not uncommon use case to encode smallish chunks of data (e.g. a 64-byte
			// SHA-512 digest), so it would be nice if that fit in the unrolled loop at least once.
			// Plus, single-digit percentage performance differences might well be quite different
			// on different hardware.

			inputU64 := generalPurposeReadU64(inputChunk[0:])

			outputChunk[0] = self.encodeTable[((inputU64 >> 58) & lowSixBits)]
			outputChunk[1] = self.encodeTable[((inputU64 >> 52) & lowSixBits)]
			outputChunk[2] = self.encodeTable[((inputU64 >> 46) & lowSixBits)]
			outputChunk[3] = self.encodeTable[((inputU64 >> 40) & lowSixBits)]
			outputChunk[4] = self.encodeTable[((inputU64 >> 34) & lowSixBits)]
			outputChunk[5] = self.encodeTable[((inputU64 >> 28) & lowSixBits)]
			outputChunk[6] = self.encodeTable[((inputU64 >> 22) & lowSixBits)]
			outputChunk[7] = self.encodeTable[((inputU64 >> 16) & lowSixBits)]

			inputU64 = generalPurposeReadU64(inputChunk[6:])

			outputChunk[8] = self.encodeTable[((inputU64 >> 58) & lowSixBits)]
			outputChunk[9] = self.encodeTable[((inputU64 >> 52) & lowSixBits)]
			outputChunk[10] = self.encodeTable[((inputU64 >> 46) & lowSixBits)]
			outputChunk[11] = self.encodeTable[((inputU64 >> 40) & lowSixBits)]
			outputChunk[12] = self.encodeTable[((inputU64 >> 34) & lowSixBits)]
			outputChunk[13] = self.encodeTable[((inputU64 >> 28) & lowSixBits)]
			outputChunk[14] = self.encodeTable[((inputU64 >> 22) & lowSixBits)]
			outputChunk[15] = self.encodeTable[((inputU64 >> 16) & lowSixBits)]

			inputU64 = generalPurposeReadU64(inputChunk[12:])

			outputChunk[16] = self.encodeTable[((inputU64 >> 58) & lowSixBits)]
			outputChunk[17] = self.encodeTable[((inputU64 >> 52) & lowSixBits)]
			outputChunk[18] = self.encodeTable[((inputU64 >> 46) & lowSixBits)]
			outputChunk[19] = self.encodeTable[((inputU64 >> 40) & lowSixBits)]
			outputChunk[20] = self.encodeTable[((inputU64 >> 34) & lowSixBits)]
			outputChunk[21] = self.encodeTable[((inputU64 >> 28) & lowSixBits)]
			outputChunk[22] = self.encodeTable[((inputU64 >> 22) & lowSixBits)]
			outputChunk[23] = self.encodeTable[((inputU64 >> 16) & lowSixBits)]

			inputU64 = generalPurposeReadU64(inputChunk[18:])

			outputChunk[24] = self.encodeTable[((inputU64 >> 58) & lowSixBits)]
			outputChunk[25] = self.encodeTable[((inputU64 >> 52) & lowSixBits)]
			outputChunk[26] = self.encodeTable[((inputU64 >> 46) & lowSixBits)]
			outputChunk[27] = self.encodeTable[((inputU64 >> 40) & lowSixBits)]
			outputChunk[28] = self.encodeTable[((inputU64 >> 34) & lowSixBits)]
			outputChunk[29] = self.encodeTable[((inputU64 >> 28) & lowSixBits)]
			outputChunk[30] = self.encodeTable[((inputU64 >> 22) & lowSixBits)]
			outputChunk[31] = self.encodeTable[((inputU64 >> 16) & lowSixBits)]

			outputIndex += blocksPerFastLoop * 8
			inputIndex += blocksPerFastLoop * 6
		}
	}

	// Encode what's left after the fast loop.

	const lowSixBitsU8 byte = 0x3F

	rem := len(input) % 3
	startOfRem := len(input) - rem

	// start at the first index not handled by fast loop, which may be 0.

	for inputIndex < startOfRem {
		inputChunk := input[inputIndex:(inputIndex + 3)]
		outputChunk := output[outputIndex:(outputIndex + 4)]

		outputChunk[0] = self.encodeTable[(inputChunk[0] >> 2)]
		outputChunk[1] = self.encodeTable[((inputChunk[0]<<4 | inputChunk[1]>>4) & lowSixBitsU8)]
		outputChunk[2] = self.encodeTable[((inputChunk[1]<<2 | inputChunk[2]>>6) & lowSixBitsU8)]
		outputChunk[3] = self.encodeTable[(inputChunk[2] & lowSixBitsU8)]

		inputIndex += 3
		outputIndex += 4
	}

	if rem == 2 {
		output[outputIndex] = self.encodeTable[(input[startOfRem] >> 2)]
		output[outputIndex+1] =
			self.encodeTable[((input[startOfRem]<<4 | input[startOfRem+1]>>4) & lowSixBitsU8)]
		output[outputIndex+2] =
			self.encodeTable[((input[startOfRem+1] << 2) & lowSixBitsU8)]
		outputIndex += 3
	} else if rem == 1 {
		output[outputIndex] = self.encodeTable[(input[startOfRem] >> 2)]
		output[outputIndex+1] =
			self.encodeTable[((input[startOfRem] << 4) & lowSixBitsU8)]
		outputIndex += 2
	}

	return outputIndex
}

// InternalDecodedLenEstimate is Engine::internal_decoded_len_estimate.
func (self *EngineGeneralPurpose) InternalDecodedLenEstimate(inputLen int) EngineDecodeEstimate {
	return generalPurposeEstimateNew(inputLen)
}

// InternalDecode is Engine::internal_decode. estimate must be the one
// InternalDecodedLenEstimate returned.
func (self *EngineGeneralPurpose) InternalDecode(
	input []byte,
	output []byte,
	estimate EngineDecodeEstimate,
) (EngineDecodeMetadata, DecodeSliceError) {
	return generalPurposeDecodeHelper(
		input,
		estimate.(generalPurposeEstimate),
		output,
		&self.decodeTable,
		self.config.decodeAllowTrailingBits,
		self.config.decodePaddingMode,
	)
}

// Config is Engine::config.
func (self *EngineGeneralPurpose) Config() EngineConfig {
	return self.config
}

// Encode is Engine::encode.
func (self *EngineGeneralPurpose) Encode(input []byte) string {
	return engineEncode(self, input)
}

// EncodeString is Engine::encode_string.
func (self *EngineGeneralPurpose) EncodeString(input []byte, outputBuf *string) {
	engineEncodeString(self, input, outputBuf)
}

// EncodeSlice is Engine::encode_slice. The error is an EncodeSliceError.
func (self *EngineGeneralPurpose) EncodeSlice(input []byte, outputBuf []byte) (int, error) {
	return engineEncodeSlice(self, input, outputBuf)
}

// Decode is Engine::decode. The error is a DecodeError.
func (self *EngineGeneralPurpose) Decode(input []byte) ([]byte, error) {
	return engineDecode(self, input)
}

// DecodeVec is Engine::decode_vec. The error is a DecodeError.
func (self *EngineGeneralPurpose) DecodeVec(input []byte, buffer *[]byte) error {
	return engineDecodeVec(self, input, buffer)
}

// DecodeSlice is Engine::decode_slice. The error is a DecodeSliceError.
func (self *EngineGeneralPurpose) DecodeSlice(input []byte, output []byte) (int, error) {
	return engineDecodeSlice(self, input, output)
}

// DecodeSliceUnchecked is Engine::decode_slice_unchecked. The error is a
// DecodeError.
func (self *EngineGeneralPurpose) DecodeSliceUnchecked(input []byte, output []byte) (int, error) {
	return engineDecodeSliceUnchecked(self, input, output)
}

// Returns a table mapping a 6-bit index to the ASCII byte encoding of the index
func generalPurposeEncodeTable(alphabet *AlphabetAlphabet) [64]byte {
	// the encode table is just the alphabet:
	// 6-bit index lookup -> printable byte
	var encodeTable [64]byte
	{
		index := 0
		for index < 64 {
			encodeTable[index] = alphabet.symbols[index]
			index += 1
		}
	}

	return encodeTable
}

// Returns a table mapping base64 bytes as the lookup index to either:
//   - [INVALID_VALUE] for bytes that aren't members of the alphabet
//   - a byte whose lower 6 bits are the value that was encoded into the index byte
func generalPurposeDecodeTable(alphabet *AlphabetAlphabet) [256]byte {
	var decodeTable [256]byte
	for i := range decodeTable {
		decodeTable[i] = generalPurposeInvalidValue
	}

	// Since the table is full of `INVALID_VALUE` already, we only need to overwrite
	// the parts that are valid.
	index := 0
	for index < 64 {
		// The index in the alphabet is the 6-bit value we care about.
		// Since the index is in 0-63, it is safe to cast to u8.
		decodeTable[alphabet.symbols[index]] = byte(index)
		index += 1
	}

	return decodeTable
}

func generalPurposeReadU64(s []byte) uint64 {
	return binary.BigEndian.Uint64(s[:8])
}

// Contains configuration parameters for base64 encoding and decoding.
//
//	config := base64.EngineGeneralPurposeConfigNew().
//		WithEncodePadding(false)
//		// further customize using `.With*` methods as needed
//
// The constants [PAD] and [NO_PAD] cover most use cases.
//
// To specify the characters used, see [Alphabet].
type EngineGeneralPurposeConfig struct {
	encodePadding           bool                    `rs:"encode_padding"`
	decodeAllowTrailingBits bool                    `rs:"decode_allow_trailing_bits"`
	decodePaddingMode       EngineDecodePaddingMode `rs:"decode_padding_mode"`
}

var _ = rs.RegisterStruct[EngineGeneralPurposeConfig]("GeneralPurposeConfig")

// Create a new config with `padding` = `true`, `decode_allow_trailing_bits` = `false`, and
// `decode_padding_mode = DecodePaddingMode::RequireCanonicalPadding`.
//
// This probably matches most people's expectations, but consider disabling padding to save
// a few bytes unless you specifically need it for compatibility with some legacy system.
func EngineGeneralPurposeConfigNew() EngineGeneralPurposeConfig {
	return EngineGeneralPurposeConfig{
		// RFC states that padding must be applied by default
		encodePadding:           true,
		decodeAllowTrailingBits: false,
		decodePaddingMode:       EngineDecodePaddingMode_RequireCanonical,
	}
}

// Create a new config based on `self` with an updated `padding` setting.
//
// If `padding` is `true`, encoding will append either 1 or 2 `=` padding characters as needed
// to produce an output whose length is a multiple of 4.
//
// Padding is not needed for correct decoding and only serves to waste bytes, but it's in the
// [spec](https://datatracker.ietf.org/doc/html/rfc4648#section-3.2).
//
// For new applications, consider not using padding if the decoders you're using don't require
// padding to be present.
func (self EngineGeneralPurposeConfig) WithEncodePadding(padding bool) EngineGeneralPurposeConfig {
	self.encodePadding = padding
	return self
}

// Create a new config based on `self` with an updated `decode_allow_trailing_bits` setting.
//
// Most users will not need to configure this. It's useful if you need to decode base64
// produced by a buggy encoder that has bits set in the unused space on the last base64
// character as per [forgiving-base64 decode](https://infra.spec.whatwg.org/#forgiving-base64-decode).
// If invalid trailing bits are present and this is `true`, those bits will
// be silently ignored, else `DecodeError::InvalidLastSymbol` will be emitted.
func (self EngineGeneralPurposeConfig) WithDecodeAllowTrailingBits(allow bool) EngineGeneralPurposeConfig {
	self.decodeAllowTrailingBits = allow
	return self
}

// Create a new config based on `self` with an updated `decode_padding_mode` setting.
//
// Padding is not useful in terms of representing encoded data -- it makes no difference to
// the decoder if padding is present or not, so if you have some un-padded input to decode, it
// is perfectly fine to use `DecodePaddingMode::Indifferent` to prevent errors from being
// emitted.
//
// However, since in practice
// [people who learned nothing from BER vs DER seem to expect base64 to have one canonical encoding](https://eprint.iacr.org/2022/361),
// the default setting is the stricter `DecodePaddingMode::RequireCanonicalPadding`.
//
// Or, if "canonical" in your circumstance means _no_ padding rather than padding to the
// next multiple of four, there's `DecodePaddingMode::RequireNoPadding`.
func (self EngineGeneralPurposeConfig) WithDecodePaddingMode(mode EngineDecodePaddingMode) EngineGeneralPurposeConfig {
	self.decodePaddingMode = mode
	return self
}

// EngineGeneralPurposeConfigDefault is `impl Default for
// GeneralPurposeConfig`: delegates to [GeneralPurposeConfig::new].
func EngineGeneralPurposeConfigDefault() EngineGeneralPurposeConfig {
	return EngineGeneralPurposeConfigNew()
}

// EncodePadding is `impl Config for GeneralPurposeConfig`.
func (self EngineGeneralPurposeConfig) EncodePadding() bool {
	return self.encodePadding
}

// A [GeneralPurpose] engine using the [alphabet::STANDARD] base64 alphabet and [PAD] config.
var EngineGeneralPurposeStandard = EngineGeneralPurposeNew(&AlphabetStandard, EngineGeneralPurposePad)

// A [GeneralPurpose] engine using the [alphabet::STANDARD] base64 alphabet and [NO_PAD] config.
var EngineGeneralPurposeStandardNoPad = EngineGeneralPurposeNew(&AlphabetStandard, EngineGeneralPurposeNoPad)

// A [GeneralPurpose] engine using the [alphabet::URL_SAFE] base64 alphabet and [PAD] config.
var EngineGeneralPurposeUrlSafe = EngineGeneralPurposeNew(&AlphabetUrlSafe, EngineGeneralPurposePad)

// A [GeneralPurpose] engine using the [alphabet::URL_SAFE] base64 alphabet and [NO_PAD] config.
var EngineGeneralPurposeUrlSafeNoPad = EngineGeneralPurposeNew(&AlphabetUrlSafe, EngineGeneralPurposeNoPad)

// Include padding bytes when encoding, and require that they be present when decoding.
//
// This is the standard per the base64 RFC, but consider using [NO_PAD] instead as padding serves
// little purpose in practice.
var EngineGeneralPurposePad = EngineGeneralPurposeConfigNew()

// Don't add padding when encoding, and require no padding when decoding.
var EngineGeneralPurposeNoPad = EngineGeneralPurposeConfigNew().
	WithEncodePadding(false).
	WithDecodePaddingMode(EngineDecodePaddingMode_RequireNone)
