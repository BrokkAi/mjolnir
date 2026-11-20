// Port of pulldown-cmark 0.10.3 src/linklabel.rs

package pulldowncmark

import "testing"

func TestLinklabel_whitespace_normalization(t *testing.T) {
	input := "«\t\tBlurry Eyes\t\t»][blurry_eyes]"
	expectedOutput := "« Blurry Eyes »" // regular spaces!

	_, normalizedLabel, ok := linklabelScanLinkLabelRest(input, func(string) (int, bool) { return 0, false }, false)
	assert(t, ok)
	assertEq(t, expectedOutput, normalizedLabel)
}

func TestLinklabel_return_carriage_linefeed_ok(t *testing.T) {
	input := "hello\r\nworld\r\n]"
	_, _, ok := linklabelScanLinkLabelRest(input, func(string) (int, bool) { return 0, true }, false)
	assert(t, ok)
}
