// Port of pulldown-cmark 0.10.3 tests/errors.rs

package pulldowncmark

import "testing"

// itErrorsParse is `parse`.
func itErrorsParse(md string) {
	parser := ParserNew(md)

	for range parser.All() {
	}
}

// itErrorsParseAllOptions is `parse_all_options`.
func itErrorsParseAllOptions(md string) {
	parser := ParserNewExt(md, OptionsAll())

	for range parser.All() {
	}
}

func TestItErrors_test_lists_inside_code_spans(t *testing.T) {
	itErrorsParse("- `\n" +
		"x\n" +
		"**\n" +
		"  *\n" +
		"  `")
}

func TestItErrors_test_fuzzer_input_1(t *testing.T) {
	itErrorsParse(">\n >>><N\n")
}

func TestItErrors_test_fuzzer_input_2(t *testing.T) {
	itErrorsParse(" \u000b\\\r- ")
}

func TestItErrors_test_fuzzer_input_3(t *testing.T) {
	itErrorsParseAllOptions("\n # #\r\u001c ")
}

func TestItErrors_test_fuzzer_input_4(t *testing.T) {
	itErrorsParseAllOptions("\u0000{\tϐ}\n-")
}

func TestItErrors_test_fuzzer_input_5(t *testing.T) {
	itErrorsParseAllOptions(" \u000c{}\n-\n")
}

func TestItErrors_test_fuzzer_input_6(t *testing.T) {
	itErrorsParse("*\t[][\n\t<p]>\n\t[]")
}

func TestItErrors_test_fuzzer_input_7(t *testing.T) {
	itErrorsParseAllOptions("[][{]}\n-")
}

func TestItErrors_test_fuzzer_input_8(t *testing.T) {
	itErrorsParseAllOptions("a\n \u000c{}\n-")
}

func TestItErrors_test_fuzzer_input_9(t *testing.T) {
	itErrorsParseAllOptions("a\n \u000c{}\\\n-")
}

func TestItErrors_test_fuzzer_input_10(t *testing.T) {
	itErrorsParseAllOptions("[[    \t\n   \u000c\u000c\u000c\u000c\u000c    {}\n-\r\u000e\u0000\u0000{# }\n\u000b\u000b\u000b\u000b\u000b\u000b\u000b\u000b\u000b\u000b\u0000\u0000")
}

func TestItErrors_test_fuzzer_input_11(t *testing.T) {
	itErrorsParseAllOptions(
		"[[\u000c\u000c   \t\n   \u000c\u000c\u000c\u000c\u000c\u000c\u000c\u000c\u000c       {}\n-\r\u000e",
	)
}

func TestItErrors_test_fuzzer_input_12(t *testing.T) {
	itErrorsParseAllOptions("\u000c-\n\u000c\n-")
}

func TestItErrors_test_fuzzer_input_13(t *testing.T) {
	// Does not fail with Options::all():
	ParserNewExt(
		".\r> ^](\r\u000c\r\x00\x00\r.\r[^\x00\x00\\\x00\x00\x00^^^^^]",
		OptionsEnableFootnotes,
	)
}

func TestItErrors_test_wrong_code_block(t *testing.T) {
	itErrorsParse("```\n" +
		" * ```\n" +
		" ")
}

func TestItErrors_test_unterminated_link(t *testing.T) {
	itErrorsParse("[](\\")
}

func TestItErrors_test_unterminated_autolink(t *testing.T) {
	itErrorsParse("<a")
}

func TestItErrors_test_infinite_loop(t *testing.T) {
	itErrorsParse("[<!W\n\\\n")
}

func TestItErrors_test_html_tag(t *testing.T) {
	itErrorsParse("<script\ufeff")
}

// all of test_bad_slice_* were found in https://github.com/raphlinus/pulldown-cmark/issues/521
func TestItErrors_test_bad_slice_a(t *testing.T) {
	itErrorsParse("><a\n")
}

func TestItErrors_test_bad_slice_b(t *testing.T) {
	itErrorsParse("><a a\n")
}

func TestItErrors_test_bad_slice_unicode(t *testing.T) {
	itErrorsParse("><a a=\n毿>")
}
