// Port of pulldown-cmark 0.10.3 src/strings.rs

package pulldowncmark

import "testing"

// The tests of `mod test_special_string` check CowStr's three
// representations (borrowed, boxed, inlined) and InlineStr. The port holds a
// CowStr as a Go string and has no InlineStr (strings.go), so none of them
// has anything to check here.

const stringsNotPorted = "checks CowStr's borrowed, boxed and inlined representations and InlineStr, which the port does not have: a CowStr is a Go string (strings.go)"

func TestStrings_inlinestr_ascii(t *testing.T) {
	t.Skip(stringsNotPorted)
}

func TestStrings_inlinestr_unicode(t *testing.T) {
	t.Skip(stringsNotPorted)
}

func TestStrings_cowstr_size(t *testing.T) {
	t.Skip(stringsNotPorted)
}

func TestStrings_cowstr_char_to_string(t *testing.T) {
	t.Skip(stringsNotPorted)
}

func TestStrings_max_inline_str_len_atleast_four(t *testing.T) {
	t.Skip(stringsNotPorted)
}

func TestStrings_inlinestr_fits_twentytwo(t *testing.T) {
	t.Skip(stringsNotPorted)
}

func TestStrings_inlinestr_not_fits_twentythree(t *testing.T) {
	t.Skip(stringsNotPorted)
}

func TestStrings_small_boxed_str_clones_to_stack(t *testing.T) {
	t.Skip(stringsNotPorted)
}

func TestStrings_cow_to_cow_str(t *testing.T) {
	t.Skip(stringsNotPorted)
}

func TestStrings_cow_str_to_cow(t *testing.T) {
	t.Skip(stringsNotPorted)
}

func TestStrings_cow_char_to_cow_str(t *testing.T) {
	t.Skip(stringsNotPorted)
}
