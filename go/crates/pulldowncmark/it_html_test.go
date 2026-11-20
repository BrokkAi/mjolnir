// Port of pulldown-cmark 0.10.3 tests/html.rs

// Tests for HTML spec.

package pulldowncmark

import (
	"strings"
	"testing"

	"github.com/BrokkAi/mjolnir/go/rs"
)

func TestItHtml_html_test_1(t *testing.T) {
	original := "Little header\n" +
		"\n" +
		"<script type=\"text/js\">\n" +
		"function some_func() {\n" +
		"console.log(\"teeeest\");\n" +
		"}\n" +
		"\n" +
		"\n" +
		"function another_func() {\n" +
		"console.log(\"fooooo\");\n" +
		"}\n" +
		"</script>"
	expected := "<p>Little header</p>\n" +
		"<script type=\"text/js\">\n" +
		"function some_func() {\n" +
		"console.log(\"teeeest\");\n" +
		"}\n" +
		"\n" +
		"\n" +
		"function another_func() {\n" +
		"console.log(\"fooooo\");\n" +
		"}\n" +
		"</script>"

	s := ""
	HtmlPushHtml(&s, ParserNew(original))
	assertEq(t, expected, s)
}

func TestItHtml_html_test_2(t *testing.T) {
	original := "Little header\n" +
		"\n" +
		"<script\n" +
		"type=\"text/js\">\n" +
		"function some_func() {\n" +
		"console.log(\"teeeest\");\n" +
		"}\n" +
		"\n" +
		"\n" +
		"function another_func() {\n" +
		"console.log(\"fooooo\");\n" +
		"}\n" +
		"</script>"
	expected := "<p>Little header</p>\n" +
		"<script\n" +
		"type=\"text/js\">\n" +
		"function some_func() {\n" +
		"console.log(\"teeeest\");\n" +
		"}\n" +
		"\n" +
		"\n" +
		"function another_func() {\n" +
		"console.log(\"fooooo\");\n" +
		"}\n" +
		"</script>"

	s := ""
	HtmlPushHtml(&s, ParserNew(original))
	assertEq(t, expected, s)
}

func TestItHtml_html_test_3(t *testing.T) {
	original := "Little header\n" +
		"\n" +
		"<?\n" +
		"<div></div>\n" +
		"<p>Useless</p>\n" +
		"?>"
	expected := "<p>Little header</p>\n" +
		"<?\n" +
		"<div></div>\n" +
		"<p>Useless</p>\n" +
		"?>"

	s := ""
	HtmlPushHtml(&s, ParserNew(original))
	assertEq(t, expected, s)
}

func TestItHtml_html_test_4(t *testing.T) {
	original := "Little header\n" +
		"\n" +
		"<!--\n" +
		"<div></div>\n" +
		"<p>Useless</p>\n" +
		"-->"
	expected := "<p>Little header</p>\n" +
		"<!--\n" +
		"<div></div>\n" +
		"<p>Useless</p>\n" +
		"-->"

	s := ""
	HtmlPushHtml(&s, ParserNew(original))
	assertEq(t, expected, s)
}

func TestItHtml_html_test_5(t *testing.T) {
	original := "Little header\n" +
		"\n" +
		"<![CDATA[\n" +
		"<div></div>\n" +
		"<p>Useless</p>\n" +
		"]]>"
	expected := "<p>Little header</p>\n" +
		"<![CDATA[\n" +
		"<div></div>\n" +
		"<p>Useless</p>\n" +
		"]]>"

	s := ""
	HtmlPushHtml(&s, ParserNew(original))
	assertEq(t, expected, s)
}

func TestItHtml_html_test_6(t *testing.T) {
	original := "Little header\n" +
		"\n" +
		"<!X\n" +
		"Some things are here...\n" +
		">"
	expected := "<p>Little header</p>\n" +
		"<!X\n" +
		"Some things are here...\n" +
		">"

	s := ""
	HtmlPushHtml(&s, ParserNew(original))
	assertEq(t, expected, s)
}

func TestItHtml_html_test_7(t *testing.T) {
	original := "Little header\n" +
		"-----------\n" +
		"\n" +
		"<script>\n" +
		"function some_func() {\n" +
		"console.log(\"teeeest\");\n" +
		"}\n" +
		"\n" +
		"\n" +
		"function another_func() {\n" +
		"console.log(\"fooooo\");\n" +
		"}\n" +
		"</script>"
	expected := "<h2>Little header</h2>\n" +
		"<script>\n" +
		"function some_func() {\n" +
		"console.log(\"teeeest\");\n" +
		"}\n" +
		"\n" +
		"\n" +
		"function another_func() {\n" +
		"console.log(\"fooooo\");\n" +
		"}\n" +
		"</script>"

	s := ""
	HtmlPushHtml(&s, ParserNew(original))
	assertEq(t, expected, s)
}

func TestItHtml_html_test_8(t *testing.T) {
	original := "A | B\n---|---\nfoo | bar"
	expected := "<table><thead><tr><th>A</th><th>B</th></tr></thead><tbody>\n" +
		"<tr><td>foo</td><td>bar</td></tr>\n" +
		"</tbody></table>\n"

	s := ""
	opts := OptionsEmpty()
	opts.Insert(OptionsEnableTables)
	HtmlPushHtml(&s, ParserNewExt(original, opts))
	assertEq(t, expected, s)
}

func TestItHtml_html_test_9(t *testing.T) {
	original := "---"
	expected := "<hr />\n"

	s := ""
	HtmlPushHtml(&s, ParserNew(original))
	assertEq(t, expected, s)
}

func TestItHtml_html_test_10(t *testing.T) {
	original := "* * *"
	expected := "<hr />\n"

	s := ""
	HtmlPushHtml(&s, ParserNew(original))
	assertEq(t, expected, s)
}

func TestItHtml_html_test_11(t *testing.T) {
	original := "hi ~~no~~"
	expected := "<p>hi ~~no~~</p>\n"

	s := ""
	HtmlPushHtml(&s, ParserNew(original))
	assertEq(t, expected, s)
}

func TestItHtml_html_test_broken_callback(t *testing.T) {
	original := "[foo],\n" +
		"[bar],\n" +
		"[baz],\n" +
		"\n" +
		"   [baz]: https://example.org\n"

	expected := "<p><a href=\"https://replaced.example.org\" title=\"some title\">foo</a>,\n" +
		"[bar],\n" +
		"<a href=\"https://example.org\">baz</a>,</p>\n"

	s := ""

	callback := BrokenLinkCallbackFunc(func(brokenLink BrokenLink) rs.Option[rs.Tuple2[string, string]] {
		if brokenLink.Reference == "foo" || brokenLink.Reference == "baz" {
			return rs.Some(rs.T2("https://replaced.example.org", "some title"))
		} else {
			return rs.None[rs.Tuple2[string, string]]()
		}
	})

	p := ParserNewWithBrokenLinkCallback(original, OptionsEmpty(), rs.Some[BrokenLinkCallback](callback))
	HtmlPushHtml(&s, p)

	assertEq(t, expected, s)
}

func TestItHtml_newline_in_code(t *testing.T) {
	originals := []string{"`\n `x", "` \n`x"}
	expected := "<p><code>  </code>x</p>\n"

	for _, original := range originals {
		s := ""
		HtmlPushHtml(&s, ParserNew(original))
		assertEq(t, expected, s)
	}
}

func TestItHtml_newline_start_end_of_code(t *testing.T) {
	original := "`\nx\n`x"
	expected := "<p><code>x</code>x</p>\n"

	s := ""
	HtmlPushHtml(&s, ParserNew(original))
	assertEq(t, expected, s)
}

// https://github.com/raphlinus/pulldown-cmark/issues/715

func TestItHtml_trim_space_and_tab_at_end_of_paragraph(t *testing.T) {
	original := "one\ntwo \t"
	expected := "<p>one\ntwo</p>\n"

	s := ""
	HtmlPushHtml(&s, ParserNew(original))
	assertEq(t, expected, s)
}

func TestItHtml_newline_within_code(t *testing.T) {
	originals := []string{"`\nx \ny\n`x", "`x \ny`x", "`x\n y`x"}
	expected := "<p><code>x  y</code>x</p>\n"

	for _, original := range originals {
		s := ""
		HtmlPushHtml(&s, ParserNew(original))
		assertEq(t, expected, s)
	}
}

func TestItHtml_trim_space_tab_nl_at_end_of_paragraph(t *testing.T) {
	original := "one\ntwo \t\n"
	expected := "<p>one\ntwo</p>\n"

	s := ""
	HtmlPushHtml(&s, ParserNew(original))
	assertEq(t, expected, s)
}

func TestItHtml_trim_space_nl_at_end_of_paragraph(t *testing.T) {
	original := "one\ntwo \n"
	expected := "<p>one\ntwo</p>\n"

	s := ""
	HtmlPushHtml(&s, ParserNew(original))
	assertEq(t, expected, s)
}

func TestItHtml_trim_space_before_soft_break(t *testing.T) {
	original := "one \ntwo"
	expected := "<p>one\ntwo</p>\n"

	s := ""
	HtmlPushHtml(&s, ParserNew(original))
	assertEq(t, expected, s)
}

// Can't easily use regression.txt due to newline normalization.
func TestItHtml_issue_819(t *testing.T) {
	original := []string{
		"# \\", "# \\\n", "# \\\n\n", "# \\\r\n", "# \\\r\n\r\n", "# \\\n\r\n", "# \\\r\n\n",
	}
	expected := "<h1>\\</h1>"

	for _, orig := range original {
		s := ""
		HtmlPushHtml(&s, ParserNew(orig))
		// Trailing newline doesn't matter. Just the actual HTML.
		assertEq(t, expected, strings.TrimRight(s, "\n"))
	}
	for _, orig := range original {
		s := ""
		opts := OptionsEmpty()
		opts.Insert(OptionsEnableHeadingAttributes)
		HtmlPushHtml(&s, ParserNewExt(orig, opts))
		// Trailing newline doesn't matter. Just the actual HTML.
		assertEq(t, expected, strings.TrimRight(s, "\n"))
	}
}
