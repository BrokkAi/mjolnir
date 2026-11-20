// Port of toml_edit 0.20.2 tests/encoder_compliance.rs

package tomledit_test

import (
	"strings"
	"testing"
)

// TestItEncoderCompliance_main is the harness's EncoderHarness: every valid
// case's expected JSON is encoded to TOML, decoded again, and must give the
// same value.
func TestItEncoderCompliance_main(t *testing.T) {
	ignored := map[string]bool{
		"valid/spec/float-0": true, // Test issue; `Decoder` turns `6.626e-34` into `0.0`
	}
	ran := 0
	for _, c := range loadTomlTestCases(t) {
		if !strings.HasPrefix(c.Name, "valid/") || ignored[c.Name] {
			continue
		}
		ran++
		t.Run(c.Name, func(t *testing.T) {
			expected := decodedFromJSON(t, c.JSON)
			encoded, err := encoderEncode(expected)
			if err != nil {
				t.Fatalf("encode: %v", err)
			}
			actual, message := decoderDecode([]byte(encoded))
			if actual == nil {
				t.Fatalf("encoded TOML does not decode: %s\n%s", message, encoded)
			}
			if !expected.equal(actual) {
				t.Fatalf("round trip differs\nexpected: %s\n  actual: %s\n encoded:\n%s", expected, actual, encoded)
			}
		})
	}
	if ran == 0 {
		t.Fatal("no toml-test case ran")
	}
}
