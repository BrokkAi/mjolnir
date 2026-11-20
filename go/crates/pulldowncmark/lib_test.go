// Port of pulldown-cmark 0.10.3 src/lib.rs

package pulldowncmark

import "testing"

// The first example in the crate's documentation, which Rust runs as a
// doctest. The second one uses TextMergeStream, which is not ported
// (doc.go).
func TestLib_doc_example(t *testing.T) {
	markdownInput := "Hello world, this is a ~~complicated~~ *very simple* example."

	// Set up options and parser. Strikethroughs are not part of the CommonMark standard
	// and we therefore must enable it explicitly.
	options := OptionsEmpty()
	options.Insert(OptionsEnableStrikethrough)
	parser := ParserNewExt(markdownInput, options)

	// Write to String buffer.
	htmlOutput := ""
	HtmlPushHtml(&htmlOutput, parser)

	// Check that the output is what we expected.
	expectedHtml := "<p>Hello world, this is a <del>complicated</del> <em>very simple</em> example.</p>\n"
	assertEq(t, expectedHtml, htmlOutput)
}
