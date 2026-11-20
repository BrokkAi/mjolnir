package pulldowncmark

import "testing"

// A Rust string is always UTF-8; a Go string need not be. doc.go promises
// that a byte outside a valid encoding is read as U+FFFD where the parser
// looks at characters, stays in the text as it is, and never makes the parser
// panic. This drives the parser, its offset iterator and the HTML writer over
// documents of Markdown syntax mixed with broken encodings: stray
// continuation bytes, truncated sequences, overlong forms, surrogates and
// bytes that never occur in UTF-8.
func TestInvalidUtf8IsParsedWithoutPanicking(t *testing.T) {
	r := corpusRand{state: 0x75746638}
	pieces := []string{
		"\x80", "\xbf", "\xc3", "\xe4\xb8", "\xf0\x9f", "\xf0\x9f\x99", "\xff", "\xfe", "\xc0\x80", "\xed\xa0\x80",
		"é", "中", "🙂", "*", "**", "_", "`", "```", "[", "]", "(", ")", "<", ">", "> ", "!", "\\", "&", "&#", ";",
		"#", "# ", "-", "- ", "|", ":", "\n", "\n\n", " ", "    ", "\t", "\r", "a", "~", "~~", "^", "[^", "]:",
		"{", "}", "=", "1.", "'", "\"", "<a>", "<!--",
	}
	for range 20000 {
		var text []byte
		for range r.between(1, 40) {
			text = append(text, r.pick(pieces)...)
		}
		document := string(text)
		for _, options := range optionSetsFor(document) {
			for range ParserNewExt(document, options).IntoOffsetIter().All() {
			}
			html := ""
			HtmlPushHtml(&html, ParserNewExt(document, options))
		}
	}
}
