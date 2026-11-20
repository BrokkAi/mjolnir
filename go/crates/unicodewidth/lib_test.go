// Port of unicode-width 0.2.2 src/lib.rs

// src/lib.rs has no `#[test]` functions. These are its documentation
// examples, which Rust runs as tests.

package unicodewidth

import "testing"

// The first example of the crate documentation. It asserts nothing: Rust
// checks only that it runs.
func TestLib_doc_crate(t *testing.T) {
	teststr := "\uFF28\uFF45\uFF4C\uFF4C\uFF4F, \uFF57\uFF4F\uFF52\uFF4C\uFF44!"
	width := UnicodeWidthStrWidth(teststr)
	t.Log(teststr)
	t.Logf("The above string is %d columns wide.", width)
}

// The example of the section "`"cjk"` feature flag".
func TestLib_doc_cjk_feature_flag(t *testing.T) {
	teststr := "\u201C\U00018000\u201D"
	assertEq(t, UnicodeWidthStrWidth(teststr), 4)

	// cfg(feature = "cjk")
	assertEq(t, UnicodeWidthStrWidthCjk(teststr), 6)
}
