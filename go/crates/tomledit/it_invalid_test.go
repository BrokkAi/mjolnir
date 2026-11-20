// Port of toml_edit 0.20.2 tests/invalid.rs

package tomledit_test

import (
	"strings"
	"testing"

	"github.com/BrokkAi/mjolnir/go/crates/tomledit"
	"github.com/BrokkAi/mjolnir/go/rs/strs"
)

// TestItInvalid_main compares the error every invalid toml-test case gives
// with the crate's recorded expectation (tests/fixtures/invalid/**/*.stderr,
// kept as the "stderr" of each case in testdata/toml_test.jsonl), as snapbox
// compares them.
func TestItInvalid_main(t *testing.T) {
	ran := 0
	for _, c := range loadTomlTestCases(t) {
		if !strings.HasPrefix(c.Name, "invalid/") {
			continue
		}
		ran++
		t.Run(c.Name, func(t *testing.T) {
			if c.Stderr == nil {
				t.Fatalf("%s has no recorded error", c.Name)
			}
			err := runInvalidCase(c.fixture(t))
			snapboxAssertEq(t, *c.Stderr, err)
		})
	}
	if ran == 0 {
		t.Fatal("no toml-test case ran")
	}
}

func runInvalidCase(input []byte) string {
	raw, err := strs.FromUtf8(input)
	if err != nil {
		return err.Error()
	}
	if _, err := tomledit.DocumentFromStr(raw); err != nil {
		return err.Error()
	}
	return ""
}
