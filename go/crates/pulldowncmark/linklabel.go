// Port of pulldown-cmark 0.10.3 src/linklabel.rs

// Copyright 2018 Google LLC
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

import "strings"

// Link label parsing and matching.

type linklabelReferenceLabelKind uint8

const (
	linklabelReferenceLabel_Link linklabelReferenceLabelKind = iota
	linklabelReferenceLabel_Footnote
)

// linklabelReferenceLabel is the crate's ReferenceLabel enum: both variants
// carry the label.
type linklabelReferenceLabel struct {
	kind  linklabelReferenceLabelKind
	label string
}

// LinkLabel and FootnoteLabel are `UniCase<CowStr>` in Rust; see unicase.go
// for how the port keys its maps by them.

// linklabelScanLinkLabelRest assumes the opening bracket has already been
// scanned. The line break handler determines what happens when a linebreak
// is found. It is passed the bytes following the line break and
// either returns `Some(k)`, where `k` is the number of bytes to skip,
// or `None` to abort parsing the label.
// Returns the number of bytes read (including closing bracket) and label on success.
func linklabelScanLinkLabelRest(
	text string,
	linebreakHandler func(string) (int, bool),
	isInTable bool,
) (int, string, bool) {
	bytes := text
	ix := 0
	onlyWhiteSpace := true
	codepoints := 0
	// no worries, doesn't allocate until we push things onto it
	var label strings.Builder
	mark := 0

loop:
	for {
		if codepoints >= 1000 {
			return 0, "", false
		}
		if ix >= len(bytes) {
			return 0, "", false
		}
		switch b := bytes[ix]; {
		case b == '[':
			return 0, "", false
		case b == ']':
			break loop
		// Backslash escapes in link references are normally untouched, but
		// tables are an exception, because they're parsed as-if the tables
		// were parsed in a discrete pass, changing `\|` to `|`, and then
		// passing the changed string to the inline parser.
		case b == '|' && isInTable && ix != 0 && bytes[ix-1] == '\\':
			// only way to reach this spot is to have `\\|` (even number of `\` before `|`)
			label.WriteString(text[mark : ix-1])
			label.WriteByte('|')
			ix += 1
			onlyWhiteSpace = false
			mark = ix
		case b == '\\' && isInTable && ix+1 < len(bytes) && bytes[ix+1] == '|':
			// only way to reach this spot is to have `\|` (odd number of `\` before `|`)
			label.WriteString(text[mark:ix])
			label.WriteByte('|')
			ix += 2
			codepoints += 1
			onlyWhiteSpace = false
			mark = ix
		case b == '\\':
			// The guard of this arm is `is_ascii_punctuation(*bytes.get(ix + 1)?)`:
			// a backslash at the end of the text ends the scan with None.
			if ix+1 >= len(bytes) {
				return 0, "", false
			}
			if puncttableIsAsciiPunctuation(bytes[ix+1]) {
				ix += 2
				codepoints += 2
				onlyWhiteSpace = false
			} else {
				// the last arm (`b => ..`)
				onlyWhiteSpace = false
				ix += 1
				if b&0b1000_0000 != 0 {
					codepoints += 1
				}
			}
		case scannersIsAsciiWhitespace(b):
			// normalize labels by collapsing whitespaces, including linebreaks
			whitespaces := 0
			linebreaks := 0
			whitespaceStart := ix

			for ix < len(bytes) && scannersIsAsciiWhitespace(bytes[ix]) {
				if eolBytes, ok := scannersScanEol(bytes[ix:]); ok {
					linebreaks += 1
					if linebreaks > 1 {
						return 0, "", false
					}
					ix += eolBytes
					skip, ok := linebreakHandler(bytes[ix:])
					if !ok {
						return 0, "", false
					}
					ix += skip
					whitespaces += 2 // indicate that we need to replace
				} else {
					if bytes[ix] == ' ' {
						whitespaces += 1
					} else {
						whitespaces += 2
					}
					ix += 1
				}
			}
			if whitespaces > 1 {
				label.WriteString(text[mark:whitespaceStart])
				label.WriteByte(' ')
				mark = ix
				codepoints += ix - whitespaceStart
			} else {
				codepoints += 1
			}
		default:
			onlyWhiteSpace = false
			ix += 1
			if b&0b1000_0000 != 0 {
				codepoints += 1
			}
		}
	}

	if onlyWhiteSpace {
		return 0, "", false
	} else {
		var cow string
		if mark == 0 {
			const asciiws = " \r\n\t"
			cow = strings.Trim(text[:ix], asciiws)
		} else {
			label.WriteString(text[mark:ix])
			built := label.String()
			for len(built) > 0 && linklabelIsLabelWhitespace(built[len(built)-1]) {
				built = built[:len(built)-1]
			}
			for len(built) > 0 && linklabelIsLabelWhitespace(built[0]) {
				built = built[1:]
			}
			cow = built
		}
		return ix + 1, cow, true
	}
}

// linklabelIsLabelWhitespace is the pattern `b' ' | b'\r' | b'\n' | b'\t'`
// the label's ends are trimmed with.
func linklabelIsLabelWhitespace(b byte) bool {
	return b == ' ' || b == '\r' || b == '\n' || b == '\t'
}
