// Port of toml_edit 0.20.2 tests/decoder_compliance.rs

package tomledit_test

import (
	"strings"
	"testing"
)

// TestItDecoderCompliance_main is the harness's DecoderHarness for TOML
// 1.0.0: every valid case of that version decodes to its expected JSON, and
// every invalid one fails to decode.
func TestItDecoderCompliance_main(t *testing.T) {
	ignored := map[string]bool{"valid/spec/float-0": true}
	ran := 0
	for _, c := range loadTomlTestCases(t) {
		if !c.Toml10 || ignored[c.Name] {
			continue
		}
		ran++
		t.Run(c.Name, func(t *testing.T) {
			actual, message := decoderDecode(c.fixture(t))
			if strings.HasPrefix(c.Name, "valid/") {
				if actual == nil {
					t.Fatalf("expected success, got error: %s", message)
				}
				expected := decodedFromJSON(t, c.JSON)
				if !expected.equal(actual) {
					t.Fatalf("decoded value differs\nexpected: %s\n  actual: %s", expected, actual)
				}
				return
			}
			if actual != nil {
				t.Fatalf("expected an error, decoded %s", actual)
			}
		})
	}
	if ran == 0 {
		t.Fatal("no toml-test case ran")
	}
}
