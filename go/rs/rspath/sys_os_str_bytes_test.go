// Port of std 1.99.1 src/sys/os_str/bytes/tests.rs

package rspath

import "testing"

func TestSysOsStrBytes_slice_debug_output(t *testing.T) {
	input := "\xF0hello,\tworld"
	expected := `"\xF0hello,\tworld"`
	output := Unix.DebugString(input)

	if output != expected {
		t.Errorf("expected %s, got %s", expected, output)
	}
}

func TestSysOsStrBytes_display(t *testing.T) {
	if got := Unix.Display("Hello\xC0\x80 There\xE6\x83 Goodbye"); got != "Hello\uFFFD\uFFFD There\uFFFD Goodbye" {
		t.Errorf("%q", got)
	}
}
