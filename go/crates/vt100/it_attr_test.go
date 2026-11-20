// Port of vt100 0.16.2 tests/attr.rs

package vt100_test

import (
	"testing"

	"github.com/BrokkAi/mjolnir/go/crates/vt100"
)

func TestItAttr_colors(t *testing.T) {
	helpersFixture(t, "colors")
}

func TestItAttr_attrs(t *testing.T) {
	helpersFixture(t, "attrs")
}

func TestItAttr_attributes_formatted(t *testing.T) {
	parser := vt100.ParserDefault()
	assertEq(t, parser.Screen().AttributesFormatted(), []byte("\x1b[m"))
	parser.Process([]byte("\x1b[32mfoo\x1b[41mbar\x1b[33mbaz"))
	assertEq(t, parser.Screen().AttributesFormatted(), []byte("\x1b[m\x1b[33;41m"))
	parser.Process([]byte("\x1b[1m\x1b[39m"))
	assertEq(t, parser.Screen().AttributesFormatted(), []byte("\x1b[m\x1b[41;1m"))
	parser.Process([]byte("\x1b[m"))
	assertEq(t, parser.Screen().AttributesFormatted(), []byte("\x1b[m"))
}
