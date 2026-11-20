// Port of pulldown-cmark 0.10.3 src/html.rs

package pulldowncmark

import (
	"bytes"
	"testing"
)

// The examples in the documentation of push_html and write_html, which
// Rust runs as doctests.

func TestHtml_doc_push_html(t *testing.T) {
	markdownStr := "\n" +
		"hello\n" +
		"=====\n" +
		"\n" +
		"* alpha\n" +
		"* beta\n"
	parser := ParserNew(markdownStr)

	htmlBuf := ""
	HtmlPushHtml(&htmlBuf, parser)

	assertEq(t, htmlBuf, "<h1>hello</h1>\n"+
		"<ul>\n"+
		"<li>alpha</li>\n"+
		"<li>beta</li>\n"+
		"</ul>\n")
}

func TestHtml_doc_write_html(t *testing.T) {
	markdownStr := "\n" +
		"hello\n" +
		"=====\n" +
		"\n" +
		"* alpha\n" +
		"* beta\n"
	var bytes_ bytes.Buffer
	parser := ParserNew(markdownStr)

	if err := HtmlWriteHtml(&bytes_, parser); err != nil {
		t.Fatal(err)
	}

	assertEq(t, bytes_.String(), "<h1>hello</h1>\n"+
		"<ul>\n"+
		"<li>alpha</li>\n"+
		"<li>beta</li>\n"+
		"</ul>\n")
}
