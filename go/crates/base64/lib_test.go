// Port of base64 0.22.1 src/tests.rs

package base64

import (
	"bytes"
	"math/rand/v2"
	"strings"
	"testing"
	"unicode/utf8"

	"github.com/BrokkAi/mjolnir/go/rs"
)

// The crate's tests draw from rand's SmallRng seeded from entropy. The port
// draws from a seeded PCG, so a run can be repeated.

func testRng(seed uint64) *rand.Rand {
	return rand.New(rand.NewPCG(seed, 0x6261736536340a))
}

// testUniform is `Uniform::new(low, high).sample(rng)`: low <= n < high.
func testUniform(rng *rand.Rand, low int, high int) int {
	return low + rng.IntN(high-low)
}

// testFillRandom appends n random bytes to buf.
func testFillRandom(buf []byte, rng *rand.Rand, n int) []byte {
	for range n {
		buf = append(buf, byte(rng.Uint32()))
	}
	return buf
}

// assertEq is `assert_eq!`.
func assertEq[T any](t *testing.T, left T, right T, context ...string) {
	t.Helper()
	if !rs.Eq(left, right) {
		t.Fatalf("assertion `left == right` failed%s\n  left: %#v\n right: %#v", strings.Join(append([]string{""}, context...), ": "), left, right)
	}
}

// assertBytes is `assert_eq!` on two byte sequences.
func assertBytes(t *testing.T, left []byte, right []byte, context ...string) {
	t.Helper()
	if !bytes.Equal(left, right) {
		t.Fatalf("assertion `left == right` failed%s\n  left: %q\n right: %q", strings.Join(append([]string{""}, context...), ": "), left, right)
	}
}

// assertDecodeErr is `assert_eq!(Err(expected), result)` for a decode.
func assertDecodeErr(t *testing.T, expected DecodeError, decoded []byte, err error, context ...string) {
	t.Helper()
	if err == nil {
		t.Fatalf("expected Err(%s), got Ok(%q)%s", expected, decoded, strings.Join(append([]string{""}, context...), ": "))
	}
	if !rs.Eq[error](expected, err) {
		t.Fatalf("expected Err(%s), got Err(%s)%s", expected, err, strings.Join(append([]string{""}, context...), ": "))
	}
}

// assertOk is `.unwrap()` of an error.
func assertOk(t *testing.T, err error) {
	t.Helper()
	if err != nil {
		t.Fatalf("called `Result::unwrap()` on an `Err` value: %s", err)
	}
}

func TestLib_roundtrip_random_config_short(t *testing.T) {
	// exercise the slower encode/decode routines that operate on shorter buffers more vigorously
	roundtripRandomConfig(t, 0, 50, 10_000)
}

func TestLib_roundtrip_random_config_long(t *testing.T) {
	roundtripRandomConfig(t, 0, 1000, 10_000)
}

func assertEncodeSanity(t *testing.T, encoded string, padded bool, inputLen int) {
	t.Helper()
	inputRem := inputLen % 3
	expectedPaddingLen := 0
	if inputRem > 0 {
		if padded {
			expectedPaddingLen = 3 - inputRem
		} else {
			expectedPaddingLen = 0
		}
	}

	expectedEncodedLen := EncodedLen(inputLen, padded).Unwrap()

	assertEq(t, expectedEncodedLen, len(encoded))

	paddingLen := strings.Count(encoded, "=")

	assertEq(t, expectedPaddingLen, paddingLen)

	if !utf8.ValidString(encoded) {
		t.Fatal("Base64 should be valid utf8")
	}
}

func roundtripRandomConfig(t *testing.T, low int, high int, iterations int) {
	t.Helper()
	inputBuf := []byte{}
	encodedBuf := ""
	rng := testRng(uint64(high))

	for range iterations {
		inputBuf = inputBuf[:0]
		encodedBuf = ""

		inputLen := testUniform(rng, low, high)

		engine := randomEngine(rng)

		inputBuf = testFillRandom(inputBuf, rng, inputLen)

		engine.EncodeString(inputBuf, &encodedBuf)

		assertEncodeSanity(t, encodedBuf, engine.Config().EncodePadding(), inputLen)

		decoded, err := engine.Decode([]byte(encodedBuf))
		assertOk(t, err)
		assertBytes(t, inputBuf, decoded)
	}
}

// randomPaddingMode is rand's `Standard` distribution of DecodePaddingMode.
func randomPaddingMode(rng *rand.Rand) EngineDecodePaddingMode {
	switch rng.IntN(3) {
	case 0:
		return EngineDecodePaddingMode_Indifferent
	case 1:
		return EngineDecodePaddingMode_RequireCanonical
	default:
		return EngineDecodePaddingMode_RequireNone
	}
}

func randomConfig(rng *rand.Rand) EngineGeneralPurposeConfig {
	mode := randomPaddingMode(rng)
	var encodePadding bool
	switch mode {
	case EngineDecodePaddingMode_Indifferent:
		encodePadding = rng.IntN(2) == 0
	case EngineDecodePaddingMode_RequireCanonical:
		encodePadding = true
	case EngineDecodePaddingMode_RequireNone:
		encodePadding = false
	}
	return EngineGeneralPurposeConfigNew().
		WithEncodePadding(encodePadding).
		WithDecodePaddingMode(mode).
		WithDecodeAllowTrailingBits(rng.IntN(2) == 0)
}

func randomAlphabet(rng *rand.Rand) *AlphabetAlphabet {
	return testAlphabets[rng.IntN(len(testAlphabets))]
}

func randomEngine(rng *rand.Rand) *EngineGeneralPurpose {
	alphabet := randomAlphabet(rng)
	config := randomConfig(rng)
	engine := EngineGeneralPurposeNew(alphabet, config)
	return &engine
}

var testAlphabets = []*AlphabetAlphabet{
	&AlphabetUrlSafe,
	&AlphabetStandard,
	&AlphabetCrypt,
	&AlphabetBcrypt,
	&AlphabetImapMutf7,
	&AlphabetBinHex,
}
