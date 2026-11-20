// Port of base64 0.22.1 src/engine/tests.rs

package base64

import (
	"bytes"
	"fmt"
	"math/rand/v2"
	"runtime"
	"strconv"
	"strings"
	"sync"
	"sync/atomic"
	"testing"

	"github.com/BrokkAi/mjolnir/go/rs"
)

// The crate runs each of these tests on three engines through rstest: the
// GeneralPurpose engine, a naive reference engine and the DecoderReader
// pseudo-engine. Only the GeneralPurpose engine is ported (the other two are
// the crate's test fixture and its io::Read adapter, which this package
// leaves out), so each test runs once, on the engines generalPurposeWrapper
// returns.

// A wrapper to make using engines in rstest fixtures easier.
type generalPurposeWrapper struct{}

// Return an engine configured for RFC standard base64
func (generalPurposeWrapper) standard() *EngineGeneralPurpose {
	engine := EngineGeneralPurposeNew(&AlphabetStandard, EngineGeneralPurposePad)
	return &engine
}

// Return an engine configured for RFC standard base64, except with no padding appended on
// encode, and required no padding on decode.
func (generalPurposeWrapper) standardUnpadded() *EngineGeneralPurpose {
	engine := EngineGeneralPurposeNew(&AlphabetStandard, EngineGeneralPurposeNoPad)
	return &engine
}

// Return an engine configured for RFC standard alphabet with the provided encode and decode
// pad settings
func (generalPurposeWrapper) standardWithPadMode(encodePad bool, decodePadMode EngineDecodePaddingMode) *EngineGeneralPurpose {
	engine := EngineGeneralPurposeNew(
		&AlphabetStandard,
		EngineGeneralPurposeConfigNew().
			WithEncodePadding(encodePad).
			WithDecodePaddingMode(decodePadMode),
	)
	return &engine
}

// Return an engine configured for RFC standard base64 that allows invalid trailing bits
func (generalPurposeWrapper) standardAllowTrailingBits() *EngineGeneralPurpose {
	engine := EngineGeneralPurposeNew(
		&AlphabetStandard,
		EngineGeneralPurposeConfigNew().WithDecodeAllowTrailingBits(true),
	)
	return &engine
}

// Return an engine configured with a randomized alphabet and config
func (w generalPurposeWrapper) random(rng *rand.Rand) *EngineGeneralPurpose {
	alphabet := randomAlphabet(rng)

	return w.randomAlphabet(rng, alphabet)
}

// Return an engine configured with the specified alphabet and randomized config
func (generalPurposeWrapper) randomAlphabet(rng *rand.Rand, alphabet *AlphabetAlphabet) *EngineGeneralPurpose {
	engine := EngineGeneralPurposeNew(alphabet, randomConfig(rng))
	return &engine
}

var E = generalPurposeWrapper{}

func TestEngine_rfc_test_vectors_std_alphabet(t *testing.T) {
	data := [][2]string{
		{"", ""},
		{"f", "Zg=="},
		{"fo", "Zm8="},
		{"foo", "Zm9v"},
		{"foob", "Zm9vYg=="},
		{"fooba", "Zm9vYmE="},
		{"foobar", "Zm9vYmFy"},
	}

	engine := E.standard()
	engineNoPadding := E.standardUnpadded()

	for _, pair := range data {
		orig, encoded := pair[0], pair[1]
		encodedWithoutPadding := strings.TrimRight(encoded, "=")

		// unpadded
		{
			var encodeBuf [8]byte
			var decodeBuf [6]byte

			encodeLen :=
				engineNoPadding.InternalEncode([]byte(orig), encodeBuf[:])
			assertEq(t,
				encodedWithoutPadding,
				string(encodeBuf[0:encodeLen]),
			)
			decodeLen, err := engineNoPadding.
				DecodeSliceUnchecked([]byte(encodedWithoutPadding), decodeBuf[:])
			assertOk(t, err)
			assertEq(t, len(orig), decodeLen)

			assertEq(t,
				orig,
				string(decodeBuf[0:decodeLen]),
			)

			// if there was any padding originally, the no padding engine won't decode it
			if strings.Contains(encoded, "=") {
				decoded, err := engineNoPadding.Decode([]byte(encoded))
				assertDecodeErr(t, &DecodeError_InvalidPadding{}, decoded, err)
			}
		}

		// padded
		{
			var encodeBuf [8]byte
			var decodeBuf [6]byte

			encodeLen := engine.InternalEncode([]byte(orig), encodeBuf[:])
			assertEq(t,
				// doesn't have padding added yet
				encodedWithoutPadding,
				string(encodeBuf[0:encodeLen]),
			)
			padLen := encodeAddPadding(encodeLen, encodeBuf[encodeLen:])
			assertBytes(t, []byte(encoded), encodeBuf[:encodeLen+padLen])

			decodeLen, err := engine.
				DecodeSliceUnchecked([]byte(encoded), decodeBuf[:])
			assertOk(t, err)
			assertEq(t, len(orig), decodeLen)

			assertEq(t,
				orig,
				string(decodeBuf[0:decodeLen]),
			)

			// if there was (canonical) padding, and we remove it, the standard engine won't decode
			if strings.Contains(encoded, "=") {
				decoded, err := engine.Decode([]byte(encodedWithoutPadding))
				assertDecodeErr(t, &DecodeError_InvalidPadding{}, decoded, err)
			}
		}
	}
}

func TestEngine_roundtrip_random(t *testing.T) {
	rng := seededRng(1)

	origData := []byte{}
	encodeBuf := []byte{}
	decodeBuf := []byte{}

	for range 10_000 {
		engine := E.random(rng)

		origData = origData[:0]
		encodeBuf = encodeBuf[:0]
		decodeBuf = decodeBuf[:0]

		origLen, _, encodedLen := generateRandomEncodedData(
			t,
			engine,
			&origData,
			&encodeBuf,
			rng,
			1, 1_000,
		)

		// exactly the right size
		decodeBuf = make([]byte, origLen)

		decLen, err := engine.
			DecodeSliceUnchecked(encodeBuf[0:encodedLen], decodeBuf[:])
		assertOk(t, err)

		assertEq(t, origLen, decLen)
		assertBytes(t, origData[:], decodeBuf[:decLen])
	}
}

func TestEngine_encode_doesnt_write_extra_bytes(t *testing.T) {
	rng := seededRng(2)

	origData := []byte{}
	encodeBuf := []byte{}
	encodeBufBackup := []byte{}

	for range 10_000 {
		engine := E.random(rng)
		padded := engine.Config().EncodePadding()

		origData = origData[:0]
		encodeBuf = encodeBuf[:0]
		encodeBufBackup = encodeBufBackup[:0]

		origLen := fillRand(&origData, rng, 0, 1000)

		prefixLen := 1024
		// plenty of prefix and suffix
		encodeBuf = testFillRandom(encodeBuf, rng, prefixLen*2+origLen*2)
		encodeBufBackup = append(encodeBufBackup, encodeBuf...)

		expectedEncodeLenNoPad := EncodedLen(origLen, false).Unwrap()

		encodedLenNoPad :=
			engine.InternalEncode(origData[:], encodeBuf[prefixLen:])
		assertEq(t, expectedEncodeLenNoPad, encodedLenNoPad)

		// no writes past what it claimed to write
		assertBytes(t, encodeBufBackup[:prefixLen], encodeBuf[:prefixLen])
		assertBytes(t,
			encodeBufBackup[(prefixLen+encodedLenNoPad):],
			encodeBuf[(prefixLen+encodedLenNoPad):],
		)

		encodedData := encodeBuf[prefixLen:(prefixLen + encodedLenNoPad)]
		assertEncodeSanity(t,
			string(encodedData),
			// engines don't pad
			false,
			origLen,
		)

		// pad so we can decode it in case our random engine requires padding
		padLen := 0
		if padded {
			padLen = encodeAddPadding(
				encodedLenNoPad,
				encodeBuf[prefixLen+encodedLenNoPad:],
			)
		}

		decoded, err := engine.
			Decode(encodeBuf[prefixLen:(prefixLen + encodedLenNoPad + padLen)])
		assertOk(t, err)
		assertBytes(t, origData, decoded)
	}
}

func TestEngine_encode_engine_slice_fits_into_precisely_sized_slice(t *testing.T) {
	origData := []byte{}
	encodedData := []byte{}
	decoded := []byte{}

	rng := seededRng(3)

	for range 10_000 {
		origData = origData[:0]
		encodedData = encodedData[:0]
		decoded = decoded[:0]

		inputLen := testUniform(rng, 0, 1000)

		origData = testFillRandom(origData, rng, inputLen)

		engine := E.random(rng)

		encodedSize := EncodedLen(inputLen, engine.Config().EncodePadding()).Unwrap()

		encodedData = make([]byte, encodedSize)

		written, err := engine.EncodeSlice(origData, encodedData)
		assertOk(t, err)
		assertEq(t, encodedSize, written)

		assertEncodeSanity(t,
			string(encodedData[0:encodedSize]),
			engine.Config().EncodePadding(),
			inputLen,
		)

		assertOk(t, engine.DecodeVec(encodedData[0:encodedSize], &decoded))
		assertBytes(t, origData, decoded)
	}
}

func TestEngine_decode_doesnt_write_extra_bytes(t *testing.T) {
	rng := seededRng(4)

	origData := []byte{}
	encodeBuf := []byte{}
	decodeBuf := []byte{}
	decodeBufBackup := []byte{}

	for range 10_000 {
		engine := E.random(rng)

		origData = origData[:0]
		encodeBuf = encodeBuf[:0]
		decodeBuf = decodeBuf[:0]
		decodeBufBackup = decodeBufBackup[:0]

		origLen := fillRand(&origData, rng, 1, 1_000)
		encodeBuf = make([]byte, origLen*2+100)

		encodedLen, err := engine.EncodeSlice(origData[:], encodeBuf[:])
		assertOk(t, err)
		encodeBuf = encodeBuf[:encodedLen]

		// oversize decode buffer so we can easily tell if it writes anything more than
		// just the decoded data
		prefixLen := 1024
		// plenty of prefix and suffix
		decodeBuf = testFillRandom(decodeBuf, rng, prefixLen*2+origLen*2)
		decodeBufBackup = append(decodeBufBackup, decodeBuf...)

		decLen, err := engine.DecodeSliceUnchecked(encodeBuf, decodeBuf[prefixLen:])
		assertOk(t, err)

		assertEq(t, origLen, decLen)
		assertBytes(t,
			origData[:],
			decodeBuf[prefixLen:prefixLen+decLen],
		)
		assertBytes(t, decodeBufBackup[:prefixLen], decodeBuf[:prefixLen])
		assertBytes(t,
			decodeBufBackup[prefixLen+decLen:],
			decodeBuf[prefixLen+decLen:],
		)
	}
}

func TestEngine_decode_detect_invalid_last_symbol(t *testing.T) {
	// 0xFF -> "/w==", so all letters > w, 0-9, and '+', '/' should get InvalidLastSymbol
	engine := E.standard()

	decoded, err := engine.Decode([]byte("iYU="))
	assertOk(t, err)
	assertBytes(t, []byte{0x89, 0x85}, decoded)
	decoded, err = engine.Decode([]byte("/w=="))
	assertOk(t, err)
	assertBytes(t, []byte{0xFF}, decoded)

	for _, c := range []struct {
		suffix string
		offset int
	}{
		// suffix, offset of bad byte from start of suffix
		{"/x==", 1},
		{"/z==", 1},
		{"/0==", 1},
		{"/9==", 1},
		{"/+==", 1},
		{"//==", 1},
		// trailing 01
		{"iYV=", 2},
		// trailing 10
		{"iYW=", 2},
		// trailing 11
		{"iYX=", 2},
	} {
		for prefixQuads := range 256 {
			encoded := strings.Repeat("AAAA", prefixQuads)
			encoded += c.suffix

			decoded, err := engine.Decode([]byte(encoded))
			assertDecodeErr(t,
				&DecodeError_InvalidLastSymbol{
					F0: len(encoded) - 4 + c.offset,
					F1: c.suffix[c.offset],
				},
				decoded, err,
			)
		}
	}
}

func TestEngine_decode_detect_1_valid_symbol_in_last_quad_invalid_length(t *testing.T) {
	for len_ := range 256 {
		len_ = len_*4 + 1
		for _, mode := range allPadModes() {
			input := bytes.Repeat([]byte{'A'}, len_)

			engine := E.standardWithPadMode(true, mode)

			decoded, err := engine.Decode(input)
			assertDecodeErr(t, &DecodeError_InvalidLength{F0: len_}, decoded, err)
			// if we add padding, then the first pad byte in the quad is invalid because it should
			// be the second symbol
			for range 3 {
				input = append(input, libPadByte)
				decoded, err := engine.Decode(input)
				assertDecodeErr(t,
					&DecodeError_InvalidByte{F0: len_, F1: libPadByte},
					decoded, err,
				)
			}
		}
	}
}

func TestEngine_decode_detect_1_invalid_byte_in_last_quad_invalid_byte(t *testing.T) {
	for prefixLen := range 256 {
		prefixLen = prefixLen * 4
		for _, mode := range allPadModes() {
			input := bytes.Repeat([]byte{'A'}, prefixLen)
			input = append(input, '*')

			engine := E.standardWithPadMode(true, mode)

			decoded, err := engine.Decode(input)
			assertDecodeErr(t,
				&DecodeError_InvalidByte{F0: prefixLen, F1: '*'},
				decoded, err,
			)
			// adding padding doesn't matter
			for range 3 {
				input = append(input, libPadByte)
				decoded, err := engine.Decode(input)
				assertDecodeErr(t,
					&DecodeError_InvalidByte{F0: prefixLen, F1: '*'},
					decoded, err,
				)
			}
		}
	}
}

func TestEngine_decode_detect_invalid_last_symbol_every_possible_two_symbols(t *testing.T) {
	engine := E.standard()

	base64ToBytes := map[string][]byte{}

	for b := range 256 {
		b64 := make([]byte, 4)
		assertEq(t, 2, engine.InternalEncode([]byte{byte(b)}, b64[:]))
		_ = encodeAddPadding(2, b64[2:])

		if _, ok := base64ToBytes[string(b64)]; ok {
			t.Fatal("assertion failed: base64_to_bytes.insert(b64, vec![b]).is_none()")
		}
		base64ToBytes[string(b64)] = []byte{byte(b)}
	}

	// every possible combination of trailing symbols must either decode to 1 byte or get InvalidLastSymbol, with or without any leading chunks

	prefix := []byte{}
	for range 256 {
		clone := append([]byte(nil), prefix...)

		var symbols [4]byte
		for _, s1 := range AlphabetStandard.symbols {
			symbols[0] = s1
			for _, s2 := range AlphabetStandard.symbols {
				symbols[1] = s2
				symbols[2] = libPadByte
				symbols[3] = libPadByte

				// chop off previous symbols
				clone = clone[:len(prefix)]
				clone = append(clone, symbols[:]...)
				decodedPrefixLen := len(prefix) / 4 * 3

				if bytes_, ok := base64ToBytes[string(symbols[:])]; ok {
					res, err := engine.Decode(clone)
					assertOk(t, err)
					// remove prefix
					assertBytes(t, bytes_, res[decodedPrefixLen:])
				} else {
					decoded, err := engine.Decode(symbols[:])
					assertDecodeErr(t,
						&DecodeError_InvalidLastSymbol{F0: 1, F1: s2},
						decoded, err,
					)
				}
			}
		}

		prefix = append(prefix, "AAAA"...)
	}
}

func TestEngine_decode_detect_invalid_last_symbol_every_possible_three_symbols(t *testing.T) {
	engine := E.standard()

	base64ToBytes := map[string][]byte{}

	var bytes_ [2]byte
	for b1 := range 256 {
		bytes_[0] = byte(b1)
		for b2 := range 256 {
			bytes_[1] = byte(b2)
			b64 := make([]byte, 4)
			assertEq(t, 3, engine.InternalEncode(bytes_[:], b64[:]))
			_ = encodeAddPadding(3, b64[3:])

			v := append([]byte(nil), bytes_[:]...)

			if _, ok := base64ToBytes[string(b64)]; ok {
				t.Fatal("assertion failed: base64_to_bytes.insert(b64, v).is_none()")
			}
			base64ToBytes[string(b64)] = v
		}
	}

	// every possible combination of symbols must either decode to 2 bytes or get InvalidLastSymbol, with or without any leading chunks
	//
	// The Rust test walks the 256 prefix lengths in order. They are
	// independent and the engine is only read, so here they are shared out
	// among goroutines; every combination is still decoded.
	var failure atomic.Pointer[string]
	fail := func(message string) { failure.CompareAndSwap(nil, &message) }
	prefixQuads := make(chan int)
	var workers sync.WaitGroup
	for range runtime.GOMAXPROCS(0) {
		workers.Add(1)
		go func() {
			defer workers.Done()
			input := []byte{}
			for quads := range prefixQuads {
				prefix := []byte(strings.Repeat("AAAA", quads))
				input = input[:0]
				input = append(input, prefix...)

				var symbols [4]byte
				for _, s1 := range AlphabetStandard.symbols {
					symbols[0] = s1
					for _, s2 := range AlphabetStandard.symbols {
						symbols[1] = s2
						for _, s3 := range AlphabetStandard.symbols {
							symbols[2] = s3
							symbols[3] = libPadByte

							// chop off previous symbols
							input = input[:len(prefix)]
							input = append(input, symbols[:]...)
							decodedPrefixLen := len(prefix) / 4 * 3

							if expected, ok := base64ToBytes[string(symbols[:])]; ok {
								res, err := engine.Decode(input)
								// remove prefix
								if err != nil || !bytes.Equal(expected, res[decodedPrefixLen:]) {
									fail(fmt.Sprintf("%q: decoded %q, %v; expected %q", input, res, err, expected))
								}
							} else {
								_, err := engine.Decode(symbols[:])
								if got, ok := err.(*DecodeError_InvalidLastSymbol); !ok || *got != (DecodeError_InvalidLastSymbol{F0: 2, F1: s3}) {
									fail(fmt.Sprintf("%q: got %v, expected InvalidLastSymbol(2, %d)", symbols, err, s3))
								}
							}
						}
					}
				}
			}
		}()
	}
	for quads := range 256 {
		prefixQuads <- quads
	}
	close(prefixQuads)
	workers.Wait()
	if message := failure.Load(); message != nil {
		t.Fatal(*message)
	}
}

func TestEngine_decode_invalid_trailing_bits_ignored_when_configured(t *testing.T) {
	strict := E.standard()
	forgiving := E.standardAllowTrailingBits()

	assertTolerantDecode := func(
		engine Engine,
		input *string,
		b64PrefixLen int,
		expectedDecodeBytes []byte,
		data string,
	) {
		t.Helper()
		prefixed := prefixedData(input, b64PrefixLen, data)
		decoded, err := engine.Decode([]byte(prefixed))
		assertOk(t, err)
		// prefix is always complete chunks
		decodedPrefixLen := b64PrefixLen / 4 * 3
		assertBytes(t, expectedDecodeBytes, decoded[decodedPrefixLen:])
	}

	prefix := ""
	for range 256 {
		input := prefix

		// example from https://github.com/marshallpierce/rust-base64/issues/75
		_, err := strict.Decode([]byte(prefixedData(&input, len(prefix), "/w==")))
		assertOk(t, err)
		_, err = strict.Decode([]byte(prefixedData(&input, len(prefix), "iYU=")))
		assertOk(t, err)
		// trailing 01
		assertTolerantDecode(forgiving, &input, len(prefix), []byte{255}, "/x==")
		assertTolerantDecode(forgiving, &input, len(prefix), []byte{137, 133}, "iYV=")
		// trailing 10
		assertTolerantDecode(forgiving, &input, len(prefix), []byte{255}, "/y==")
		assertTolerantDecode(forgiving, &input, len(prefix), []byte{137, 133}, "iYW=")
		// trailing 11
		assertTolerantDecode(forgiving, &input, len(prefix), []byte{255}, "/z==")
		assertTolerantDecode(forgiving, &input, len(prefix), []byte{137, 133}, "iYX=")

		prefix += "AAAA"
	}
}

func TestEngine_decode_invalid_byte_error(t *testing.T) {
	rng := seededRng(5)

	origData := []byte{}
	encodeBuf := []byte{}
	decodeBuf := []byte{}

	for range 100_000 {
		alphabet := randomAlphabet(rng)
		engine := E.randomAlphabet(rng, alphabet)

		origData = origData[:0]
		encodeBuf = encodeBuf[:0]
		decodeBuf = decodeBuf[:0]

		origLen, _, encodedLenWithPadding :=
			generateRandomEncodedData(
				t,
				engine,
				&origData,
				&encodeBuf,
				rng,
				1, 1_000,
			)

		// exactly the right size
		decodeBuf = make([]byte, origLen)

		// replace one encoded byte with an invalid byte
		var invalidByte byte
		for {
			byte_ := byte(rng.Uint32())

			if bytes.IndexByte(alphabet.symbols[:], byte_) >= 0 || byte_ == libPadByte {
				continue
			} else {
				invalidByte = byte_
				break
			}
		}

		invalidIndex := testUniform(rng, 0, origLen)
		encodeBuf[invalidIndex] = invalidByte

		_, err := engine.DecodeSliceUnchecked(
			encodeBuf[0:encodedLenWithPadding],
			decodeBuf[:],
		)
		assertDecodeErr(t,
			&DecodeError_InvalidByte{F0: invalidIndex, F1: invalidByte},
			nil, err,
		)
	}
}

// Any amount of padding anywhere before the final non padding character = invalid byte at first
// pad byte.
// From this and [decode_padding_before_final_non_padding_char_error_invalid_byte_at_first_pad_non_canonical_padding_suffix_all_modes],
// we know padding must extend contiguously to the end of the input.
func TestEngine_decode_padding_before_final_non_padding_char_error_invalid_byte_at_first_pad_all_modes(t *testing.T) {
	// Different amounts of padding, w/ offset from end for the last non-padding char.
	// Only canonical padding, so Canonical mode will work.
	suffixes := []paddingSuffix{{"AA==", 2}, {"AAA=", 1}, {"AAAA", 0}}

	for _, mode := range padModesAllowingPadding() {
		// We don't encode, so we don't care about encode padding.
		engine := E.standardWithPadMode(true, mode)

		decodePaddingBeforeFinalNonPaddingCharErrorInvalidByteAtFirstPad(
			t,
			engine,
			suffixes,
		)
	}
}

// See [decode_padding_before_final_non_padding_char_error_invalid_byte_at_first_pad_all_modes]
func TestEngine_decode_padding_before_final_non_padding_char_error_invalid_byte_at_first_pad_non_canonical_padding_suffix(t *testing.T) {
	// Different amounts of padding, w/ offset from end for the last non-padding char, and
	// non-canonical padding.
	suffixes := []paddingSuffix{
		{"AA==", 2},
		{"AA=", 1},
		{"AA", 0},
		{"AAA=", 1},
		{"AAA", 0},
		{"AAAA", 0},
	}

	// We don't encode, so we don't care about encode padding.
	// Decoding is indifferent so that we don't get caught by missing padding on the last quad
	engine := E.standardWithPadMode(true, EngineDecodePaddingMode_Indifferent)

	decodePaddingBeforeFinalNonPaddingCharErrorInvalidByteAtFirstPad(
		t,
		engine,
		suffixes,
	)
}

type paddingSuffix struct {
	suffix       string
	suffixOffset int
}

func decodePaddingBeforeFinalNonPaddingCharErrorInvalidByteAtFirstPad(
	t *testing.T,
	engine Engine,
	suffixes []paddingSuffix,
) {
	t.Helper()
	rng := seededRng(6)

	for range 100_000 {
		for _, c := range suffixes {
			s := strings.Repeat("AAAA", rng.IntN(257))
			s += c.suffix
			encoded := []byte(s)

			// calculate a range to write padding into that leaves at least one non padding char
			lastNonPaddingOffset := len(encoded) - 1 - c.suffixOffset

			// don't include last non padding char as it must stay not padding
			paddingEnd := rng.IntN(lastNonPaddingOffset)

			// don't use more than 100 bytes of padding, but also use shorter lengths when
			// padding_end is near the start of the encoded data to avoid biasing to padding
			// the entire prefix on short lengths
			paddingLen := 1 + rng.IntN(min(100, paddingEnd+1))
			paddingStart := max(paddingEnd-paddingLen, 0)

			for i := paddingStart; i <= paddingEnd; i++ {
				encoded[i] = libPadByte
			}

			// should still have non-padding before any final padding
			if encoded[lastNonPaddingOffset] == libPadByte {
				t.Fatal("assertion `left != right` failed")
			}
			decoded, err := engine.Decode(encoded)
			assertDecodeErr(t,
				&DecodeError_InvalidByte{F0: paddingStart, F1: libPadByte},
				decoded, err,
				"len: "+strconv.Itoa(len(encoded))+", input: "+string(encoded),
			)
		}
	}
}

// Any amount of padding before final chunk that crosses over into final chunk with 1-4 bytes =
// invalid byte at first pad byte.
// From this we know the padding must start in the final chunk.
func TestEngine_decode_padding_starts_before_final_chunk_error_invalid_byte_at_first_pad(t *testing.T) {
	rng := seededRng(7)

	// must have at least one prefix quad
	// don't use no-padding mode, as the reader decode might decode a block that ends with
	// valid padding, which should then be referenced when encountering the later invalid byte
	for _, mode := range padModesAllowingPadding() {
		// we don't encode so we don't care about encode padding
		engine := E.standardWithPadMode(true, mode)
		for range 100_000 {
			suffixLen := 1 + rng.IntN(4)
			// all 0 bits so we don't hit InvalidLastSymbol with the reader decoder
			encoded := []byte(strings.Repeat("AAAA", testUniform(rng, 1, 256)))
			encoded = append(encoded, bytes.Repeat([]byte{libPadByte}, suffixLen)...)

			// amount of padding must be long enough to extend back from suffix into previous
			// quads
			paddingLen := testUniform(rng, suffixLen+1, len(encoded))
			// no non-padding after padding in this test, so padding goes to the end
			paddingStart := len(encoded) - paddingLen
			for i := paddingStart; i < len(encoded); i++ {
				encoded[i] = libPadByte
			}

			decoded, err := engine.Decode(encoded)
			assertDecodeErr(t,
				&DecodeError_InvalidByte{F0: paddingStart, F1: libPadByte},
				decoded, err,
				"suffix_len: "+strconv.Itoa(suffixLen)+", padding_len: "+strconv.Itoa(paddingLen)+", b64: "+string(encoded),
			)
		}
	}
}

// 0-1 bytes of data before any amount of padding in final chunk = invalid byte, since padding
// is not valid data (consistent with error for pad bytes in earlier chunks).
// From this we know there must be 2-3 bytes of data before padding
func TestEngine_decode_too_little_data_before_padding_error_invalid_byte(t *testing.T) {
	rng := seededRng(8)

	// want to test no prefix quad case, so start at 0
	for _, mode := range allPadModes() {
		// we don't encode so we don't care about encode padding
		engine := E.standardWithPadMode(true, mode)
		for range 100_000 {
			suffixDataLen := rng.IntN(2)
			prefixQuadLen := rng.IntN(256)

			// for all possible padding lengths
			for paddingLen := 1; paddingLen <= 4-suffixDataLen; paddingLen++ {
				encoded := []byte(strings.Repeat("ABCD", prefixQuadLen))
				encoded = append(encoded, bytes.Repeat([]byte{'A'}, suffixDataLen)...)
				encoded = append(encoded, bytes.Repeat([]byte{libPadByte}, paddingLen)...)

				decoded, err := engine.Decode(encoded)
				assertDecodeErr(t,
					&DecodeError_InvalidByte{
						F0: prefixQuadLen*4 + suffixDataLen,
						F1: libPadByte,
					},
					decoded, err,
					"input "+string(encoded)+" suffix data len "+strconv.Itoa(suffixDataLen)+" pad len "+strconv.Itoa(paddingLen),
				)
			}
		}
	}
}

// https://eprint.iacr.org/2022/361.pdf table 2, test 1
func TestEngine_decode_malleability_test_case_3_byte_suffix_valid(t *testing.T) {
	decoded, err := E.standard().Decode([]byte("SGVsbG8="))
	assertOk(t, err)
	assertBytes(t, []byte("Hello"), decoded)
}

// https://eprint.iacr.org/2022/361.pdf table 2, test 2
func TestEngine_decode_malleability_test_case_3_byte_suffix_invalid_trailing_symbol(t *testing.T) {
	decoded, err := E.standard().Decode([]byte("SGVsbG9="))
	assertDecodeErr(t, &DecodeError_InvalidLastSymbol{F0: 6, F1: 0x39}, decoded, err)
}

// https://eprint.iacr.org/2022/361.pdf table 2, test 3
func TestEngine_decode_malleability_test_case_3_byte_suffix_no_padding(t *testing.T) {
	decoded, err := E.standard().Decode([]byte("SGVsbG9"))
	assertDecodeErr(t, &DecodeError_InvalidPadding{}, decoded, err)
}

// https://eprint.iacr.org/2022/361.pdf table 2, test 4
func TestEngine_decode_malleability_test_case_2_byte_suffix_valid_two_padding_symbols(t *testing.T) {
	decoded, err := E.standard().Decode([]byte("SGVsbA=="))
	assertOk(t, err)
	assertBytes(t, []byte("Hell"), decoded)
}

// https://eprint.iacr.org/2022/361.pdf table 2, test 5
func TestEngine_decode_malleability_test_case_2_byte_suffix_short_padding(t *testing.T) {
	decoded, err := E.standard().Decode([]byte("SGVsbA="))
	assertDecodeErr(t, &DecodeError_InvalidPadding{}, decoded, err)
}

// https://eprint.iacr.org/2022/361.pdf table 2, test 6
func TestEngine_decode_malleability_test_case_2_byte_suffix_no_padding(t *testing.T) {
	decoded, err := E.standard().Decode([]byte("SGVsbA"))
	assertDecodeErr(t, &DecodeError_InvalidPadding{}, decoded, err)
}

// https://eprint.iacr.org/2022/361.pdf table 2, test 7
// DecoderReader pseudo-engine gets InvalidByte at 8 (extra padding) since it decodes the first
// two complete quads correctly.
func TestEngine_decode_malleability_test_case_2_byte_suffix_too_much_padding(t *testing.T) {
	decoded, err := E.standard().Decode([]byte("SGVsbA===="))
	assertDecodeErr(t, &DecodeError_InvalidByte{F0: 6, F1: libPadByte}, decoded, err)
}

// Requires canonical padding -> accepts 2 + 2, 3 + 1, 4 + 0 final quad configurations
func TestEngine_decode_pad_mode_requires_canonical_accepts_canonical(t *testing.T) {
	assertAllSuffixesOk(t,
		E.standardWithPadMode(true, EngineDecodePaddingMode_RequireCanonical),
		[]string{"/w==", "iYU=", "AAAA"},
	)
}

// Requires canonical padding -> rejects 2 + 0-1, 3 + 0 final chunk configurations
func TestEngine_decode_pad_mode_requires_canonical_rejects_non_canonical(t *testing.T) {
	engine := E.standardWithPadMode(true, EngineDecodePaddingMode_RequireCanonical)

	suffixes := []string{"/w", "/w=", "iYU"}
	for numPrefixQuads := range 256 {
		for _, suffix := range suffixes {
			encoded := strings.Repeat("AAAA", numPrefixQuads)
			encoded += suffix

			decoded, err := engine.Decode([]byte(encoded))

			assertDecodeErr(t, &DecodeError_InvalidPadding{}, decoded, err)
		}
	}
}

// Requires no padding -> accepts 2 + 0, 3 + 0, 4 + 0 final chunk configuration
func TestEngine_decode_pad_mode_requires_no_padding_accepts_no_padding(t *testing.T) {
	assertAllSuffixesOk(t,
		E.standardWithPadMode(true, EngineDecodePaddingMode_RequireNone),
		[]string{"/w", "iYU", "AAAA"},
	)
}

// Requires no padding -> rejects 2 + 1-2, 3 + 1 final chunk configuration
func TestEngine_decode_pad_mode_requires_no_padding_rejects_any_padding(t *testing.T) {
	engine := E.standardWithPadMode(true, EngineDecodePaddingMode_RequireNone)

	suffixes := []string{"/w=", "/w==", "iYU="}
	for numPrefixQuads := range 256 {
		for _, suffix := range suffixes {
			encoded := strings.Repeat("AAAA", numPrefixQuads)
			encoded += suffix

			decoded, err := engine.Decode([]byte(encoded))

			assertDecodeErr(t, &DecodeError_InvalidPadding{}, decoded, err)
		}
	}
}

// Indifferent padding accepts 2 + 0-2, 3 + 0-1, 4 + 0 final chunk configuration
func TestEngine_decode_pad_mode_indifferent_padding_accepts_anything(t *testing.T) {
	assertAllSuffixesOk(t,
		E.standardWithPadMode(true, EngineDecodePaddingMode_Indifferent),
		[]string{"/w", "/w=", "/w==", "iYU", "iYU=", "AAAA"},
	)
}

// 1 trailing byte that's not padding is detected as invalid byte even though there's padding
// in the middle of the input. This is essentially mandating the eager check for 1 trailing byte
// to catch the \n suffix case.
// DecoderReader pseudo-engine can't handle DecodePaddingMode::RequireNone since it will decode
// a complete quad with padding in it before encountering the stray byte that makes it an invalid
// length
func TestEngine_decode_invalid_trailing_bytes_all_pad_modes_invalid_byte(t *testing.T) {
	for _, mode := range allPadModes() {
		doInvalidTrailingByte(t, E.standardWithPadMode(true, mode), mode)
	}
}

func TestEngine_decode_invalid_trailing_bytes_invalid_byte(t *testing.T) {
	// excluding no padding mode because the DecoderWrapper pseudo-engine will fail with
	// InvalidPadding because it will decode the last complete quad with padding first
	for _, mode := range padModesAllowingPadding() {
		doInvalidTrailingByte(t, E.standardWithPadMode(true, mode), mode)
	}
}

func doInvalidTrailingByte(t *testing.T, engine Engine, mode EngineDecodePaddingMode) {
	t.Helper()
	for _, lastByte := range []byte{'*', '\n'} {
		for numPrefixQuads := range 256 {
			s := strings.Repeat("ABCD", numPrefixQuads)
			s += "Cg=="
			input := []byte(s)
			input = append(input, lastByte)

			// The case of trailing newlines is common enough to warrant a test for a good error
			// message.
			decoded, err := engine.Decode(input)
			assertDecodeErr(t,
				&DecodeError_InvalidByte{
					F0: numPrefixQuads*4 + 4,
					F1: lastByte,
				},
				decoded, err,
				"mode: "+strconv.Itoa(int(mode))+", input: "+string(input),
			)
		}
	}
}

// When there's 1 trailing byte, but it's padding, it's only InvalidByte if there isn't padding
// earlier.
func TestEngine_decode_invalid_trailing_padding_as_invalid_byte_at_first_pad_byte(t *testing.T) {
	// excluding no padding mode because the DecoderWrapper pseudo-engine will fail with
	// InvalidPadding because it will decode the last complete quad with padding first
	for _, mode := range padModesAllowingPadding() {
		doInvalidTrailingPaddingAsInvalidByteAtFirstPadding(
			t,
			E.standardWithPadMode(true, mode),
			mode,
		)
	}
}

// DecoderReader pseudo-engine can't handle DecodePaddingMode::RequireNone since it will decode
// a complete quad with padding in it before encountering the stray byte that makes it an invalid
// length
func TestEngine_decode_invalid_trailing_padding_as_invalid_byte_at_first_byte_all_modes(t *testing.T) {
	for _, mode := range allPadModes() {
		doInvalidTrailingPaddingAsInvalidByteAtFirstPadding(
			t,
			E.standardWithPadMode(true, mode),
			mode,
		)
	}
}

func doInvalidTrailingPaddingAsInvalidByteAtFirstPadding(
	t *testing.T,
	engine Engine,
	mode EngineDecodePaddingMode,
) {
	t.Helper()
	for numPrefixQuads := range 256 {
		for _, c := range []struct {
			suffix    string
			padOffset int
		}{{"AA===", 2}, {"AAA==", 3}, {"AAAA=", 4}} {
			s := strings.Repeat("ABCD", numPrefixQuads)
			s += c.suffix

			decoded, err := engine.Decode([]byte(s))
			assertDecodeErr(t,
				// pad after `g`, not the last one
				&DecodeError_InvalidByte{
					F0: numPrefixQuads*4 + c.padOffset,
					F1: libPadByte,
				},
				decoded, err,
				"mode: "+strconv.Itoa(int(mode))+", input: "+s,
			)
		}
	}
}

func TestEngine_decode_into_slice_fits_in_precisely_sized_slice(t *testing.T) {
	origData := []byte{}
	encodedData := ""
	decodeBuf := []byte{}

	rng := seededRng(9)

	for range 10_000 {
		origData = origData[:0]
		encodedData = ""
		decodeBuf = decodeBuf[:0]

		inputLen := testUniform(rng, 0, 1000)

		origData = testFillRandom(origData, rng, inputLen)

		engine := E.random(rng)
		engine.EncodeString(origData, &encodedData)
		assertEncodeSanity(t, encodedData, engine.Config().EncodePadding(), inputLen)

		decodeBuf = make([]byte, inputLen)
		// decode into the non-empty buf
		decodeBytesWritten, err := engine.
			DecodeSliceUnchecked([]byte(encodedData), decodeBuf[:])
		assertOk(t, err)
		assertEq(t, len(origData), decodeBytesWritten)
		assertBytes(t, origData, decodeBuf)

		// same for checked variant
		decodeBuf = make([]byte, inputLen)
		// decode into the non-empty buf
		decodeBytesWritten, err = engine.
			DecodeSlice([]byte(encodedData), decodeBuf[:])
		assertOk(t, err)
		assertEq(t, len(origData), decodeBytesWritten)
		assertBytes(t, origData, decodeBuf)
	}
}

func TestEngine_inner_decode_reports_padding_position(t *testing.T) {
	b64 := ""
	decoded := []byte{}
	engine := E.standard()

	for padPosition := 1; padPosition < 10_000; padPosition++ {
		b64 = ""
		// plenty of room for original data
		decoded = make([]byte, padPosition)

		b64 += strings.Repeat("A", padPosition)
		// finish the quad with padding
		b64 += strings.Repeat("=", 4-(padPosition%4))

		decodeRes, err := engine.InternalDecode(
			[]byte(b64),
			decoded[:],
			engine.InternalDecodedLenEstimate(len(b64)),
		)
		if padPosition%4 < 2 {
			// impossible padding
			assertEq[error](t,
				DecodeSliceErrorFromDecodeError(&DecodeError_InvalidByte{
					F0: padPosition,
					F1: libPadByte,
				}),
				err,
			)
		} else {
			if err != nil {
				t.Fatalf("pad position %d: %s", padPosition, err)
			}
			decodedBytes := padPosition / 4 * 3
			switch padPosition % 4 {
			case 0:
			case 2:
				decodedBytes += 1
			case 3:
				decodedBytes += 2
			default:
				t.Fatal("internal error: entered unreachable code")
			}
			assertEq(t,
				engineDecodeMetadataNew(decodedBytes, rs.Some(padPosition)),
				decodeRes,
			)
		}
	}
}

func TestEngine_decode_length_estimate_delta(t *testing.T) {
	for _, engine := range []*EngineGeneralPurpose{E.standard(), E.standardUnpadded()} {
		for _, padding := range []bool{true, false} {
			for origLen := range 1000 {
				encodedLen := EncodedLen(origLen, padding).Unwrap()

				decodedEstimate := engine.
					InternalDecodedLenEstimate(encodedLen).
					DecodedLenEstimate()
				if !(decodedEstimate >= origLen) {
					t.Fatal("assertion failed: decoded_estimate >= orig_len")
				}
				if !(decodedEstimate-origLen < 3) {
					t.Fatalf("estimate: %d, encoded: %d, orig: %d", decodedEstimate, encodedLen, origLen)
				}
			}
		}
	}
}

func TestEngine_estimate_via_u128_inflation(t *testing.T) {
	// cover both ends of usize (a Go int here, so the top end is that of
	// int: the formula is checked against the same 64-bit arithmetic)
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
		// inflate to 128 bit type to be able to safely use the easy formulas: in
		// unsigned 64 bits, (len + 3) cannot overflow for len below 2^63.
		len128 := uint64(encodedLen)

		estimate := E.standard().
			InternalDecodedLenEstimate(encodedLen).
			DecodedLenEstimate()

		// This check is a little too strict: it requires using the (len + 3) / 4 * 3 formula
		// or equivalent, but until other engines come along that use a different formula
		// requiring that we think more carefully about what the allowable criteria are, this
		// will do.
		if int((len128+3)/4*3) != estimate {
			t.Fatalf("enc len %d: estimate %d", encodedLen, estimate)
		}
	}
}

func TestEngine_decode_slice_checked_fails_gracefully_at_all_output_lengths(t *testing.T) {
	rng := seededRng(10)
	for originalLen := range 1000 {
		original := testFillRandom(nil, rng, originalLen)

		for _, mode := range allPadModes() {
			encodePad := true
			if mode == EngineDecodePaddingMode_RequireNone {
				encodePad = false
			}
			engine := E.standardWithPadMode(encodePad, mode)

			encoded := engine.Encode(original)
			decodeBuf := make([]byte, 0, originalLen)
			for decodeBufLen := range originalLen {
				decodeBuf = decodeBuf[:decodeBufLen]
				_, err := engine.DecodeSlice([]byte(encoded), decodeBuf[:])
				assertEq[error](t,
					&DecodeSliceError_OutputSliceTooSmall{},
					err,
					"original len: "+strconv.Itoa(originalLen)+", encoded len: "+strconv.Itoa(len(encoded))+", buf len: "+strconv.Itoa(decodeBufLen),
				)
				// internal method works the same
				_, sliceErr := engine.InternalDecode(
					[]byte(encoded),
					decodeBuf[:],
					engine.InternalDecodedLenEstimate(len(encoded)),
				)
				assertEq[error](t, &DecodeSliceError_OutputSliceTooSmall{}, sliceErr)
			}

			decodeBuf = testFillRandom(decodeBuf[:0], rng, originalLen)
			written, err := engine.DecodeSlice([]byte(encoded), decodeBuf[:])
			assertOk(t, err)
			assertEq(t, originalLen, written)
			assertBytes(t, original, decodeBuf)
		}
	}
}

// Returns a tuple of the original data length, the encoded data length (just data), and the length including padding.
//
// Vecs provided should be empty.
func generateRandomEncodedData(
	t *testing.T,
	engine Engine,
	origData *[]byte,
	encodeBuf *[]byte,
	rng *rand.Rand,
	lengthLow int, lengthHigh int,
) (int, int, int) {
	t.Helper()
	padding := engine.Config().EncodePadding()

	origLen := fillRand(origData, rng, lengthLow, lengthHigh)
	expectedEncodedLen := EncodedLen(origLen, padding).Unwrap()
	*encodeBuf = make([]byte, expectedEncodedLen)

	baseEncodedLen := engine.InternalEncode((*origData)[:], (*encodeBuf)[:])

	encLenWithPadding := baseEncodedLen
	if padding {
		encLenWithPadding = baseEncodedLen + encodeAddPadding(baseEncodedLen, (*encodeBuf)[baseEncodedLen:])
	}

	assertEq(t, expectedEncodedLen, encLenWithPadding)

	return origLen, baseEncodedLen, encLenWithPadding
}

// fill to a random length
func fillRand(vec *[]byte, rng *rand.Rand, lengthLow int, lengthHigh int) int {
	len_ := testUniform(rng, lengthLow, lengthHigh)
	*vec = testFillRandom(*vec, rng, len_)

	return len_
}

func prefixedData(inputWithPrefix *string, prefixLen int, data string) string {
	*inputWithPrefix = (*inputWithPrefix)[:prefixLen]
	*inputWithPrefix += data
	return *inputWithPrefix
}

func seededRng(seed uint64) *rand.Rand {
	return testRng(seed + 100)
}

func allPadModes() []EngineDecodePaddingMode {
	return []EngineDecodePaddingMode{
		EngineDecodePaddingMode_Indifferent,
		EngineDecodePaddingMode_RequireCanonical,
		EngineDecodePaddingMode_RequireNone,
	}
}

func padModesAllowingPadding() []EngineDecodePaddingMode {
	return []EngineDecodePaddingMode{
		EngineDecodePaddingMode_Indifferent,
		EngineDecodePaddingMode_RequireCanonical,
	}
}

func assertAllSuffixesOk(t *testing.T, engine Engine, suffixes []string) {
	t.Helper()
	for numPrefixQuads := range 256 {
		for _, suffix := range suffixes {
			encoded := strings.Repeat("AAAA", numPrefixQuads)
			encoded += suffix

			_, err := engine.Decode([]byte(encoded))
			if err != nil {
				t.Fatalf("assertion failed: res.is_ok(): %q: %s", encoded, err)
			}
		}
	}
}
