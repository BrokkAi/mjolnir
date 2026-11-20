package base64

import (
	"bytes"
	"encoding/hex"
	"encoding/json"
	"math/rand/v2"
	"os"
	"strconv"
	"strings"
	"testing"

	"github.com/BrokkAi/mjolnir/go/rs"
	"github.com/BrokkAi/mjolnir/go/rs/rstest/oracle"
)

// The differential test asks the real crate (go/port/oracle, module `base64`)
// to encode and decode with each engine and holds this package to the same
// bytes, the same error variant with the same offset and symbol, the same
// Display and Debug text, and the same panics. The corpus is recorded in
// testdata/oracle.
//
// Bytes travel as hex, so input that is not UTF-8 is decoded too.
//
// MJ_ORACLE_SCALE multiplies the number of generated random cases. It is for
// a longer run against the live oracle (`MJ_ORACLE=live MJ_ORACLE_SCALE=50
// ...`); the recorded corpus is made with the default of 1.
func oracleScale() int {
	scale, err := strconv.Atoi(os.Getenv("MJ_ORACLE_SCALE"))
	if err != nil || scale < 1 {
		return 1
	}
	return scale
}

type oracleRequest struct {
	Op                      string `json:"op"`
	Engine                  string `json:"engine"`
	Alphabet                string `json:"alphabet"`
	EncodePadding           bool   `json:"encode_padding"`
	DecodeAllowTrailingBits bool   `json:"decode_allow_trailing_bits"`
	DecodePaddingMode       string `json:"decode_padding_mode"`
	Hex                     string `json:"hex"`
	OutLen                  int    `json:"out_len"`
	Text                    string `json:"text"`
	Len                     uint64 `json:"len"`
}

var oracleAlphabets = map[string]*AlphabetAlphabet{
	"standard":   &AlphabetStandard,
	"url_safe":   &AlphabetUrlSafe,
	"crypt":      &AlphabetCrypt,
	"bcrypt":     &AlphabetBcrypt,
	"imap_mutf7": &AlphabetImapMutf7,
	"bin_hex":    &AlphabetBinHex,
}

func oracleEngine(request oracleRequest) *EngineGeneralPurpose {
	switch request.Engine {
	case "standard":
		return &EngineGeneralPurposeStandard
	case "standard_no_pad":
		return &EngineGeneralPurposeStandardNoPad
	case "url_safe":
		return &EngineGeneralPurposeUrlSafe
	case "url_safe_no_pad":
		return &EngineGeneralPurposeUrlSafeNoPad
	}
	var mode EngineDecodePaddingMode
	switch request.DecodePaddingMode {
	case "indifferent":
		mode = EngineDecodePaddingMode_Indifferent
	case "none":
		mode = EngineDecodePaddingMode_RequireNone
	default:
		mode = EngineDecodePaddingMode_RequireCanonical
	}
	config := EngineGeneralPurposeConfigNew().
		WithEncodePadding(request.EncodePadding).
		WithDecodeAllowTrailingBits(request.DecodeAllowTrailingBits).
		WithDecodePaddingMode(mode)
	alphabet, ok := oracleAlphabets[request.Alphabet]
	if !ok {
		alphabet = &AlphabetStandard
	}
	engine := EngineGeneralPurposeNew(alphabet, config)
	return &engine
}

func oracleErrorReply(err error) map[string]any {
	return map[string]any{"err": rs.ToString(err), "err_debug": rs.DebugString(err)}
}

// oracleLossy is String::from_utf8_lossy of a buffer the oracle prints.
func oracleLossy(buffer []byte) string {
	return strings.ToValidUTF8(string(buffer), "�")
}

func oracleEncode(request oracleRequest) any {
	engine := oracleEngine(request)
	input := rs.Unwrap(hex.DecodeString(request.Hex))
	string_ := "prefix:"
	engine.EncodeString(input, &string_)
	buffer := bytes.Repeat([]byte{'#'}, request.OutLen)
	var slice any
	if written, err := engine.EncodeSlice(input, buffer); err != nil {
		slice = oracleErrorReply(err)
	} else {
		slice = map[string]any{"ok": written, "buffer": oracleLossy(buffer)}
	}
	return map[string]any{"text": engine.Encode(input), "string": string_, "slice": slice}
}

func oracleDecode(request oracleRequest) any {
	engine := oracleEngine(request)
	input := rs.Unwrap(hex.DecodeString(request.Hex))

	var decoded any
	if bytes_, err := engine.Decode(input); err != nil {
		decoded = oracleErrorReply(err)
	} else {
		decoded = map[string]any{"ok": hex.EncodeToString(bytes_)}
	}

	vec := []byte("prefix")
	var vecReply any
	if err := engine.DecodeVec(input, &vec); err != nil {
		reply := oracleErrorReply(err)
		reply["buffer"] = hex.EncodeToString(vec)
		vecReply = reply
	} else {
		vecReply = map[string]any{"ok": hex.EncodeToString(vec)}
	}

	buffer := bytes.Repeat([]byte{0xEE}, request.OutLen)
	var slice any
	if written, err := engine.DecodeSlice(input, buffer); err != nil {
		slice = oracleErrorReply(err)
	} else {
		slice = map[string]any{"ok": written, "buffer": hex.EncodeToString(buffer)}
	}

	buffer = bytes.Repeat([]byte{0xEE}, request.OutLen)
	unchecked := oracle.Catch(func() any {
		written, err := engine.DecodeSliceUnchecked(input, buffer)
		if err != nil {
			return oracleErrorReply(err)
		}
		return map[string]any{"ok": written, "buffer": hex.EncodeToString(buffer)}
	})

	return map[string]any{"decode": decoded, "vec": vecReply, "slice": slice, "unchecked": unchecked}
}

func oracleAlphabet(request oracleRequest) any {
	alphabet, err := AlphabetAlphabetNew(request.Text)
	if err != nil {
		return oracleErrorReply(err)
	}
	return map[string]any{"ok": alphabet.AsStr()}
}

func oracleLengths(request oracleRequest) any {
	len_ := int(request.Len)
	optional := func(option rs.Option[int]) any {
		if value, ok := option.Get(); ok {
			return value
		}
		return nil
	}
	return map[string]any{
		"padded":   optional(EncodedLen(len_, true)),
		"unpadded": optional(EncodedLen(len_, false)),
		"estimate": DecodedLenEstimate(len_),
	}
}

func oracleAnswer(t *testing.T) func(json.RawMessage) any {
	return func(raw json.RawMessage) any {
		var request oracleRequest
		if err := json.Unmarshal(raw, &request); err != nil {
			t.Fatalf("decode %s: %v", raw, err)
		}
		return oracle.Catch(func() any {
			switch request.Op {
			case "encode":
				return oracleEncode(request)
			case "decode":
				return oracleDecode(request)
			case "alphabet":
				return oracleAlphabet(request)
			case "lengths":
				return oracleLengths(request)
			default:
				t.Fatalf("unknown op %q", request.Op)
				return nil
			}
		})
	}
}

// oracleEngines are the engine descriptions of the requests: the four
// constants Mjolnir uses, and custom engines over every alphabet, padding
// mode and trailing-bits setting.
func oracleEngines() []map[string]any {
	engines := []map[string]any{
		{"engine": "standard"}, {"engine": "standard_no_pad"}, {"engine": "url_safe"}, {"engine": "url_safe_no_pad"},
	}
	for _, alphabet := range []string{"standard", "url_safe", "crypt", "bcrypt", "imap_mutf7", "bin_hex"} {
		for _, mode := range []string{"indifferent", "canonical", "none"} {
			for _, flags := range [][2]bool{{true, false}, {false, true}} {
				engines = append(engines, map[string]any{
					"engine": "custom", "alphabet": alphabet, "decode_padding_mode": mode,
					"encode_padding": flags[0], "decode_allow_trailing_bits": flags[1],
				})
			}
		}
	}
	return engines
}

func oracleWith(engine map[string]any, fields map[string]any) map[string]any {
	request := map[string]any{}
	for key, value := range engine {
		request[key] = value
	}
	for key, value := range fields {
		request[key] = value
	}
	return request
}

// oracleEncodeWith encodes data with the engine as the Go side does, for
// building decode inputs from valid encodings.
func oracleEncodeWith(engine map[string]any, data []byte) string {
	request := oracleRequest{Engine: engine["engine"].(string)}
	if request.Engine == "custom" {
		request.Alphabet = engine["alphabet"].(string)
		request.DecodePaddingMode = engine["decode_padding_mode"].(string)
		request.EncodePadding = engine["encode_padding"].(bool)
		request.DecodeAllowTrailingBits = engine["decode_allow_trailing_bits"].(bool)
	}
	return oracleEngine(request).Encode(data)
}

func oracleRandomBytes(random *rand.Rand, n int) []byte {
	data := make([]byte, n)
	for i := range data {
		data[i] = byte(random.Uint32())
	}
	return data
}

// oracleOutLens are output buffer sizes around what a decode needs: none,
// one short, exact, the conservative estimate and more.
func oracleOutLens(needed int, estimate int) []int {
	return []int{0, max(needed-1, 0), needed, estimate, estimate + 5}
}

func TestDiffAgainstRust(t *testing.T) {
	// The recorded corpus is a deterministic subset that stays under 1 MB
	// compressed: long random payloads do not compress. Under MJ_ORACLE=live
	// the long inputs are also tried with every custom engine, and more often.
	full := os.Getenv("MJ_ORACLE") == "live"
	oracle.Check(t, "base64", "base64", func(yield func(any)) {
		random := rand.New(rand.NewPCG(1, 0x6261736536340a))
		engines := oracleEngines()
		decode := func(engine map[string]any, input []byte, outLen int) {
			yield(oracleWith(engine, map[string]any{"op": "decode", "hex": hex.EncodeToString(input), "out_len": outLen}))
		}

		// Encoding: every length up to 70, the lengths around the chunked
		// encoder's 768-byte chunk, and output buffers of every size near
		// what is needed.
		lengths := []int{}
		for n := range 71 {
			lengths = append(lengths, n)
		}
		long := []int{766, 767, 768, 769, 770, 1535, 1536, 1537, 2305}
		for index, engine := range engines {
			lengths := lengths
			// The four general_purpose constants come first.
			if index < 4 || full {
				lengths = append(lengths, long...)
			}
			for _, n := range lengths {
				data := oracleRandomBytes(random, n)
				encoded := len(oracleEncodeWith(engine, data))
				outLen := []int{encoded, encoded - 1, encoded + 3, 0}[random.IntN(4)]
				yield(oracleWith(engine, map[string]any{"op": "encode", "hex": hex.EncodeToString(data), "out_len": max(outLen, 0)}))
			}
		}

		// Decoding written-out inputs: the crate's test vectors, the cases
		// its tests pin, and what Mjolnir meets (JWT parts, "not-base64",
		// a trailing newline, the other alphabet's symbols).
		texts := []string{
			"", "Zg==", "Zm8=", "Zm9v", "Zm9vYg==", "Zm9vYmE=", "Zm9vYmFy", "Zg", "Zm8", "Zm9vYg", "Zm9vYmE",
			"SGVsbG8=", "SGVsbG9=", "SGVsbG9", "SGVsbA==", "SGVsbA=", "SGVsbA", "SGVsbA====", "iYU=", "iYV=", "iYW=",
			"iYX=", "/w==", "/x==", "/y==", "/z==", "/0==", "/9==", "/+==", "//==", "/w", "/w=", "iYU", "AAAA", "A",
			"AA", "AAA", "A=", "AA=", "AAA=", "AAAA=", "AA===", "AAA==", "AAAA====", "=", "==", "===", "====",
			"=AAA", "A=AA", "AA=A", "Cg==\n", "Cg==*", "ABCDCg==\n", "aGVsbG8gd29ybGR+Cg==", "aGVsbG8gaW50ZXJuZXR-Cg",
			"aGVsbG8gaW50ZXJuZXR-Cg==", "+uwgVQA=", "/+wgVQA=", "-_-_", "+/+/", "not-base64", "image-data",
			"eyJ0eXAiOiJKV1QiLCJhbGciOiJFUzI1NiJ9", "eyJzdWIiOiJvbmx5In0", "eyJzdWIiOiJvbmx5In0=", " Zm9v", "Zm9v ",
			"Zm 9v", "Zm9v\r\n", "Zm9v\x00", "Zm9v\xff", "Zm9v\xc3\xa9", "\xff", "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
			"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA==", "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA", "ABCDEFGHIJKLZg",
		}
		for _, text := range texts {
			for _, engine := range engines {
				decode(engine, []byte(text), 3*len(text)/4+3)
			}
		}

		// Valid encodings of random data, with each engine, then broken in
		// one place: a byte outside the alphabet, padding inserted, padding
		// added or removed, a set trailing bit, a stray last byte. The
		// lengths cross the decoder's 8-byte and 32-byte blocks.
		mutate := func(encoded []byte) []byte {
			mutated := append([]byte(nil), encoded...)
			switch random.IntN(9) {
			case 0:
				if len(mutated) > 0 {
					bad := []byte{'=', '\n', ' ', '*', 0x80, 0xFF, '-', '_', '+', '/', '.', 0x00, '!', ','}
					mutated[random.IntN(len(mutated))] = bad[random.IntN(len(bad))]
				}
			case 1:
				position := random.IntN(len(mutated) + 1)
				mutated = append(mutated[:position], append([]byte{'='}, mutated[position:]...)...)
			case 2:
				mutated = append(mutated, bytes.Repeat([]byte{'='}, 1+random.IntN(3))...)
			case 3:
				mutated = bytes.TrimRight(mutated, "=")
			case 4:
				trimmed := bytes.TrimRight(mutated, "=")
				if len(trimmed) > 0 {
					// The next symbol of the alphabet, which may set a trailing bit.
					last := len(trimmed) - 1
					mutated[last] = encoded[random.IntN(len(trimmed))]
				}
			case 5:
				mutated = append(mutated, []byte{'\n', 'A', '=', '*'}[random.IntN(4)])
			case 6:
				if len(mutated) > 0 {
					mutated = mutated[:random.IntN(len(mutated))]
				}
			case 7:
				// Padding run reaching back into earlier quads.
				if len(mutated) > 2 {
					start := random.IntN(len(mutated) - 1)
					for i := start; i < len(mutated); i++ {
						mutated[i] = '='
					}
				}
			}
			return mutated
		}
		for range 2500 * oracleScale() {
			engine := engines[random.IntN(len(engines))]
			n := random.IntN(80)
			if random.IntN(50) == 0 || (full && random.IntN(10) == 0) {
				n = random.IntN(1200)
			}
			data := oracleRandomBytes(random, n)
			encoded := []byte(oracleEncodeWith(engine, data))
			// Decode with an engine of the same alphabet family sometimes and
			// any engine otherwise, so padding modes meet foreign padding.
			decoder := engine
			if random.IntN(3) == 0 {
				decoder = engines[random.IntN(len(engines))]
			}
			input := encoded
			if random.IntN(4) > 0 {
				input = mutate(encoded)
			}
			estimate := DecodedLenEstimate(len(input))
			outLens := oracleOutLens(n, estimate)
			decode(decoder, input, outLens[random.IntN(len(outLens))])
		}

		// Unstructured input made of the bytes the decoder branches on.
		soup := []byte("AZaz09+/-_=\n *\x80\xff,.!")
		for range 1500 * oracleScale() {
			input := make([]byte, random.IntN(40))
			for i := range input {
				input[i] = soup[random.IntN(len(soup))]
			}
			engine := engines[random.IntN(len(engines))]
			decode(engine, input, random.IntN(32))
		}

		// Alphabets.
		standard := string(AlphabetStandard.symbols[:])
		alphabets := []string{
			standard, string(AlphabetUrlSafe.symbols[:]), string(AlphabetBinHex.symbols[:]), standard[:63],
			standard + "!", "", standard[:62] + "é", standard[:63] + "é", "A" + standard[1:63] + "A",
			standard[:63] + "=", "=" + standard[1:], "\x0c" + standard[1:], standard[:63] + "\x7f", " " + standard[1:],
			"~" + standard[1:], standard[:32] + "\x00" + standard[33:], "AACDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/",
			"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789//",
			"ABCDEFGHIJKLMNOPQRSTUVWXYZZbcdefghijklmnopqrstuvwxyz0123456789+/",
			"xxxxxxxxxABCDEFGHIJKLMNOPQRSTUVWXYZZbcdefghijklmnopqrstuvwxyz0123456789+/",
		}
		for _, alphabet := range alphabets {
			yield(map[string]any{"op": "alphabet", "text": alphabet})
		}
		for range 200 * oracleScale() {
			symbols := []byte(standard)
			symbols[random.IntN(64)] = byte(random.IntN(128))
			yield(map[string]any{"op": "alphabet", "text": strings.ToValidUTF8(string(symbols), "")})
		}

		// Lengths. A usize is a Go int here, so the lengths stay below the
		// point where 2^63 and Rust's 2^64 bound give different answers.
		for n := range 100 {
			yield(map[string]any{"op": "lengths", "len": n})
		}
		for _, n := range []uint64{1 << 20, 1<<32 - 1, 1 << 32, 1<<60 - 1, 1 << 60, 1<<61 + 1, 1<<62 - 1} {
			yield(map[string]any{"op": "lengths", "len": n})
		}
	}, oracleAnswer(t))
}
