// Port of pulldown-cmark 0.10.3 tests/lib.rs

package pulldowncmark

import (
	"strings"
	"testing"
)

// testMarkdownHtml is `test_markdown_html`. Its Rust name would be exported
// as TestMarkdownHtml, which `go test` takes for a test; it is not one.
func testMarkdownHtml(
	t *testing.T,
	input string,
	output string,
	smartPunct bool,
	metadataBlocks bool,
	oldFootnotes bool,
) {
	t.Helper()
	s := ""

	opts := OptionsEmpty()
	opts.Insert(OptionsEnableTables)
	opts.Insert(OptionsEnableStrikethrough)
	opts.Insert(OptionsEnableTasklists)
	if oldFootnotes {
		opts.Insert(OptionsEnableOldFootnotes)
	} else {
		opts.Insert(OptionsEnableFootnotes)
	}
	if metadataBlocks {
		opts.Insert(OptionsEnableYamlStyleMetadataBlocks)
		opts.Insert(OptionsEnablePlusesDelimitedMetadataBlocks)
	}
	if smartPunct {
		opts.Insert(OptionsEnableSmartPunctuation)
	}
	opts.Insert(OptionsEnableHeadingAttributes)

	p := ParserNewExt(input, opts)
	HtmlPushHtml(&s, p)

	// normalizing the HTML using html5ever may hide actual errors
	// assert_eq!(html_standardize(output), html_standardize(&s));
	assertEq(t, htmlStandardize(output), htmlStandardize(s))
}

func htmlStandardize(s string) string {
	s = strings.ReplaceAll(s, "<br>", "<br />")
	s = strings.ReplaceAll(s, "<br/>", "<br />")
	s = strings.ReplaceAll(s, "<hr>", "<hr />")
	s = strings.ReplaceAll(s, "<hr/>", "<hr />")
	// permit extra or missing line breaks only between tags
	s = strings.ReplaceAll(s, ">\n<", "><")
	return s
}
