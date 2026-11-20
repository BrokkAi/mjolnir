// Port of pulldown-cmark-escape 0.10.1 src/lib.rs

// Copyright 2015 Google Inc. All rights reserved.
//
// Permission is hereby granted, free of charge, to any person obtaining a copy
// of this software and associated documentation files (the "Software"), to deal
// in the Software without restriction, including without limitation the rights
// to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
// copies of the Software, and to permit persons to whom the Software is
// furnished to do so, subject to the following conditions:
//
// The above copyright notice and this permission notice shall be included in
// all copies or substantial portions of the Software.
//
// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
// IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
// FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
// AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
// LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
// OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN
// THE SOFTWARE.

package pulldowncmark

import (
	"io"
	"strings"
)

// Utility functions for HTML escaping. Only useful when building your own
// HTML renderer.
//
// The crate is a dependency of pulldown-cmark's "html" feature and is ported
// here, unexported, for html.go.

var escapeHrefSafe = [128]byte{
	0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
	0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
	0, 1, 0, 1, 1, 1, 0, 0, 1, 1, 1, 1, 1, 1, 1, 1,
	1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 0, 1, 0, 1,
	1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,
	1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 0, 0, 0, 1, 1,
	0, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,
	1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 0, 0, 0, 1, 0,
}

const escapeHexChars = "0123456789ABCDEF"
const escapeAmpEscape = "&amp;"
const escapeSingleQuoteEscape = "&#x27;"

// escapeStrWrite is the trait that allows writing string slices. This is
// basically an extension of `std::io::Write` in order to include `String`.
// The trait's `write_fmt` is not needed: the port formats with strconv and
// writes the result with WriteStr.
type escapeStrWrite interface {
	WriteStr(s string) error
}

// escapeWriteWrapper is the wrapper around an `io::Write`.
type escapeWriteWrapper struct{ F0 io.Writer }

func (w escapeWriteWrapper) WriteStr(s string) error {
	_, err := io.WriteString(w.F0, s)
	return err
}

// escapeStringWriter is `impl StrWrite for String`.
type escapeStringWriter struct{ F0 *strings.Builder }

func (w escapeStringWriter) WriteStr(s string) error {
	w.F0.WriteString(s)
	return nil
}

// escapeEscapeHref writes an href to the buffer, escaping href unsafe bytes.
func escapeEscapeHref(w escapeStrWrite, s string) error {
	bytes := s
	mark := 0
	for i := 0; i < len(bytes); i++ {
		c := bytes[i]
		if c >= 0x80 || escapeHrefSafe[c] == 0 {
			// character needing escape

			// write partial substring up to mark
			if mark < i {
				if err := w.WriteStr(s[mark:i]); err != nil {
					return err
				}
			}
			switch c {
			case '&':
				if err := w.WriteStr(escapeAmpEscape); err != nil {
					return err
				}
			case '\'':
				if err := w.WriteStr(escapeSingleQuoteEscape); err != nil {
					return err
				}
			default:
				var buf [3]byte
				buf[0] = '%'
				buf[1] = escapeHexChars[(int(c)>>4)&0xF]
				buf[2] = escapeHexChars[int(c)&0xF]
				escaped := string(buf[:])
				if err := w.WriteStr(escaped); err != nil {
					return err
				}
			}
			mark = i + 1 // all escaped characters are ASCII
		}
	}
	return w.WriteStr(s[mark:])
}

func escapeCreateHtmlEscapeTable(body bool) [256]byte {
	var table [256]byte
	table['&'] = 1
	table['<'] = 2
	table['>'] = 3
	if !body {
		table['"'] = 4
		table['\''] = 5
	}
	return table
}

var escapeHtmlEscapeTable = escapeCreateHtmlEscapeTable(false)
var escapeHtmlBodyTextEscapeTable = escapeCreateHtmlEscapeTable(true)

var escapeHtmlEscapes = [6]string{"", "&amp;", "&lt;", "&gt;", "&quot;", "&#39;"}

// escapeEscapeHtml writes the given string to the Write sink, replacing
// special HTML bytes (<, >, &, ", ') by escape sequences.
//
// Use this function to write output to quoted HTML attributes.
// Since this function doesn't escape spaces, unquoted attributes
// cannot be used.
func escapeEscapeHtml(w escapeStrWrite, s string) error {
	return escapeEscapeHtmlScalar(w, s, &escapeHtmlEscapeTable)
}

// escapeEscapeHtmlBodyText, for use in HTML body text, writes the given
// string to the Write sink, replacing special HTML bytes (<, >, &) by escape
// sequences.
//
// This function should be used for escaping text nodes, not attributes.
func escapeEscapeHtmlBodyText(w escapeStrWrite, s string) error {
	return escapeEscapeHtmlScalar(w, s, &escapeHtmlBodyTextEscapeTable)
}

func escapeEscapeHtmlScalar(w escapeStrWrite, s string, table *[256]byte) error {
	bytes := s
	mark := 0
	i := 0
	for i < len(s) {
		pos := -1
		for k := i; k < len(bytes); k++ {
			if table[bytes[k]] != 0 {
				pos = k - i
				break
			}
		}
		if pos < 0 {
			break
		}
		i += pos
		c := bytes[i]
		escape := table[c]
		escapeSeq := escapeHtmlEscapes[escape]
		if err := w.WriteStr(s[mark:i]); err != nil {
			return err
		}
		if err := w.WriteStr(escapeSeq); err != nil {
			return err
		}
		i += 1
		mark = i // all escaped characters are ASCII
	}
	return w.WriteStr(s[mark:])
}
