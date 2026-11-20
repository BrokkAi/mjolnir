// Port of vt100 0.16.2 tests/mode.rs

package vt100_test

import "testing"

func TestItMode_modes(t *testing.T) {
	helpersFixture(t, "modes")
}

func TestItMode_alternate_buffer(t *testing.T) {
	helpersFixture(t, "alternate_buffer")
}
