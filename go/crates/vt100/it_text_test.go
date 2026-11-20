// Port of vt100 0.16.2 tests/text.rs

package vt100_test

import "testing"

func TestItText_ascii(t *testing.T) {
	helpersFixture(t, "ascii")
}

func TestItText_utf8(t *testing.T) {
	helpersFixture(t, "utf8")
}

func TestItText_newlines(t *testing.T) {
	helpersFixture(t, "newlines")
}

func TestItText_wide(t *testing.T) {
	helpersFixture(t, "wide")
}

func TestItText_combining(t *testing.T) {
	helpersFixture(t, "combining")
}

func TestItText_wrap(t *testing.T) {
	helpersFixture(t, "wrap")
}

func TestItText_wrap_weird(t *testing.T) {
	helpersFixture(t, "wrap_weird")
}
