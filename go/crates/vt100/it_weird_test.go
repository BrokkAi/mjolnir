// Port of vt100 0.16.2 tests/weird.rs

package vt100_test

import (
	"testing"

	"github.com/BrokkAi/mjolnir/go/crates/vt100"
)

func TestItWeird_intermediate_control(t *testing.T) {
	helpersFixture(t, "intermediate_control")
}

// The test passes when nothing panics.
func TestItWeird_params(t *testing.T) {
	parser := vt100.ParserDefault()
	parser.Process([]byte("\x1b[::::::::::::::::::::::::::::::::@"))
	parser.Process([]byte("\x1b[::::::::::::::::::::::::::::::::H"))
	parser.Process([]byte("\x1b[::::::::::::::::::::::::::::::::r"))
	parser.Process([]byte("a\x1b[8888888X"))
}
