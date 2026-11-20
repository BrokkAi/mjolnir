// Port of pulldown-cmark 0.10.3 src/scanners.rs

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
	"strconv"
	"strings"

	"github.com/BrokkAi/mjolnir/go/rs"
)

// Scanners for fragments of CommonMark syntax
//
// Rust scans `&[u8]` slices of the input here. The port scans Go strings: a
// string indexes and slices by byte with the same bounds checks, and never
// needs copying to become one of the strings an event carries.

// sorted for binary search
var scannersHtmlTags = [62]string{
	"address",
	"article",
	"aside",
	"base",
	"basefont",
	"blockquote",
	"body",
	"caption",
	"center",
	"col",
	"colgroup",
	"dd",
	"details",
	"dialog",
	"dir",
	"div",
	"dl",
	"dt",
	"fieldset",
	"figcaption",
	"figure",
	"footer",
	"form",
	"frame",
	"frameset",
	"h1",
	"h2",
	"h3",
	"h4",
	"h5",
	"h6",
	"head",
	"header",
	"hr",
	"html",
	"iframe",
	"legend",
	"li",
	"link",
	"main",
	"menu",
	"menuitem",
	"nav",
	"noframes",
	"ol",
	"optgroup",
	"option",
	"p",
	"param",
	"search",
	"section",
	"summary",
	"table",
	"tbody",
	"td",
	"tfoot",
	"th",
	"thead",
	"title",
	"tr",
	"track",
	"ul",
}

// scannersLineStart is the analysis of the beginning of a line, including
// indentation and container markers.
//
// It is `Clone` in Rust and saved and restored by value; here that is a
// struct copy (`save := *lineStart`, `*lineStart = save`).
type scannersLineStart struct {
	bytes string
	ix    int

	// The index in `bytes` after the last tab we scanned; initially
	// zero.
	//
	// Thus, there are no tab characters between `ix` and here, and for
	// the purpose of defining block structure, this position can be
	// considered to fall on a tab stop.
	//
	// This is only valid while scanning the initial portion of the
	// line; methods that work with interior structure don't bother to
	// update it.
	tabStart int

	// In contexts where spaces help to define block structure, tabs
	// behave as if they were replaced by spaces with a tab stop of 4
	// characters.
	//
	// If we have scanned past a tab character but not consumed all
	// the horizontal width it contributed, this is the number of
	// spaces logically remaining, before the character at `ix`.
	spacesRemaining int

	// no thematic breaks can occur before this offset.
	// this prevents scanning over and over up to a certain point
	minHruleOffset int
}

func scannersLineStartNew(bytes string) scannersLineStart {
	return scannersLineStart{
		bytes:           bytes,
		tabStart:        0,
		ix:              0,
		spacesRemaining: 0,
		minHruleOffset:  0,
	}
}

// scanSpace tries to scan a number of spaces.
//
// Returns true if all spaces were consumed.
//
// Note: consumes some spaces even if not successful.
func (lineStart *scannersLineStart) scanSpace(nSpace int) bool {
	return lineStart.scanSpaceInner(nSpace) == 0
}

// scanSpaceUpto scans a number of spaces up to a maximum.
//
// Returns number of spaces scanned.
func (lineStart *scannersLineStart) scanSpaceUpto(nSpace int) int {
	return nSpace - lineStart.scanSpaceInner(nSpace)
}

// scanSpaceInner returns unused remainder of spaces.
func (lineStart *scannersLineStart) scanSpaceInner(nSpace int) int {
	// Consume any common prefix between the number of spaces we
	// want and the number of unscanned tab-introduced spaces.
	nFromRemaining := min(lineStart.spacesRemaining, nSpace)
	lineStart.spacesRemaining -= nFromRemaining
	nSpace -= nFromRemaining

loop:
	for nSpace > 0 && lineStart.ix < len(lineStart.bytes) {
		switch lineStart.bytes[lineStart.ix] {
		case ' ':
			lineStart.ix += 1
			nSpace -= 1
		case '\t':
			spaces := 4 - (lineStart.ix-lineStart.tabStart)%4
			lineStart.ix += 1
			lineStart.tabStart = lineStart.ix
			n := min(spaces, nSpace)
			nSpace -= n

			// Record the unscanned portion of the tab.
			lineStart.spacesRemaining = spaces - n
		default:
			break loop
		}
	}
	return nSpace
}

// scanAllSpace scans all available ASCII whitespace (not including eol).
func (lineStart *scannersLineStart) scanAllSpace() {
	lineStart.spacesRemaining = 0
	rest := lineStart.bytes[lineStart.ix:]
	count := 0
	for count < len(rest) && (rest[count] == ' ' || rest[count] == '\t') {
		count += 1
	}
	lineStart.ix += count
}

// isAtEol determines whether we're at end of line (includes end of file).
func (lineStart *scannersLineStart) isAtEol() bool {
	if lineStart.ix >= len(lineStart.bytes) {
		return true
	}
	c := lineStart.bytes[lineStart.ix]
	return c == '\r' || c == '\n'
}

func (lineStart *scannersLineStart) scanCh(c byte) bool {
	if lineStart.ix < len(lineStart.bytes) && lineStart.bytes[lineStart.ix] == c {
		lineStart.ix += 1
		return true
	} else {
		return false
	}
}

func (lineStart *scannersLineStart) scanBlockquoteMarker() bool {
	save := *lineStart
	_ = lineStart.scanSpace(3)
	if lineStart.scanCh('>') {
		_ = lineStart.scanSpace(1)
		return true
	} else {
		*lineStart = save
		return false
	}
}

// scanListMarker scans a list marker.
//
// Return value is the character, the start index, and the indent in spaces.
// For ordered list markers, the character will be one of b'.' or b')'. For
// bullet list markers, it will be one of b'-', b'+', or b'*'.
func (lineStart *scannersLineStart) scanListMarker() (byte, uint64, int, bool) {
	save := *lineStart
	indent := lineStart.scanSpaceUpto(4)
	if indent < 4 && lineStart.ix < len(lineStart.bytes) {
		c := lineStart.bytes[lineStart.ix]
		if c == '-' || c == '+' || c == '*' {
			if lineStart.ix >= lineStart.minHruleOffset {
				// there could be an hrule here
				if minOffset, isHrule := scannersScanHrule(lineStart.bytes[lineStart.ix:]); !isHrule {
					lineStart.minHruleOffset = minOffset
				} else {
					*lineStart = save
					return 0, 0, 0, false
				}
			}
			lineStart.ix += 1
			if lineStart.scanSpace(1) || lineStart.isAtEol() {
				return lineStart.finishListMarker(c, 0, indent+2)
			}
		} else if u8IsAsciiDigit(c) {
			startIx := lineStart.ix
			ix := lineStart.ix + 1
			val := uint64(c - '0')
			for ix < len(lineStart.bytes) && ix-startIx < 10 {
				c := lineStart.bytes[ix]
				ix += 1
				if u8IsAsciiDigit(c) {
					val = val*10 + uint64(c-'0')
				} else if c == ')' || c == '.' {
					lineStart.ix = ix
					if lineStart.scanSpace(1) || lineStart.isAtEol() {
						return lineStart.finishListMarker(c, val, indent+1+ix-startIx)
					} else {
						break
					}
				} else {
					break
				}
			}
		}
	}
	*lineStart = save
	return 0, 0, 0, false
}

func (lineStart *scannersLineStart) finishListMarker(
	c byte,
	start uint64,
	indent int,
) (byte, uint64, int, bool) {
	save := *lineStart

	// skip the rest of the line if it's blank
	if _, ok := scannersScanBlankLine(lineStart.bytes[lineStart.ix:]); ok {
		return c, start, indent, true
	}

	postIndent := lineStart.scanSpaceUpto(4)
	if postIndent < 4 {
		indent += postIndent
	} else {
		*lineStart = save
	}
	return c, start, indent, true
}

// scanTaskListMarker returns Some(is_checked) when a task list marker was
// found. Resets itself to original state otherwise.
func (lineStart *scannersLineStart) scanTaskListMarker() (isChecked bool, ok bool) {
	save := *lineStart
	lineStart.scanSpaceUpto(3)

	if !lineStart.scanCh('[') {
		*lineStart = save
		return false, false
	}
	if lineStart.ix >= len(lineStart.bytes) {
		*lineStart = save
		return false, false
	}
	switch c := lineStart.bytes[lineStart.ix]; {
	case scannersIsAsciiWhitespaceNoNl(c):
		lineStart.ix += 1
		isChecked = false
	case c == 'x' || c == 'X':
		lineStart.ix += 1
		isChecked = true
	default:
		*lineStart = save
		return false, false
	}
	if !lineStart.scanCh(']') {
		*lineStart = save
		return false, false
	}
	if !(lineStart.ix < len(lineStart.bytes) && scannersIsAsciiWhitespaceNoNl(lineStart.bytes[lineStart.ix])) {
		*lineStart = save
		return false, false
	}
	return isChecked, true
}

func (lineStart *scannersLineStart) bytesScanned() int {
	return lineStart.ix
}

func (lineStart *scannersLineStart) remainingSpace() int {
	return lineStart.spacesRemaining
}

func scannersIsAsciiWhitespace(c byte) bool {
	return (0x09 <= c && c <= 0x0d) || c == ' '
}

func scannersIsAsciiWhitespaceNoNl(c byte) bool {
	return c == '\t' || c == 0x0b || c == 0x0c || c == ' '
}

func scannersIsAsciiAlpha(c byte) bool {
	return u8IsAsciiAlphabetic(c)
}

func scannersIsAsciiAlphanumeric(c byte) bool {
	return ('0' <= c && c <= '9') || ('a' <= c && c <= 'z') || ('A' <= c && c <= 'Z')
}

func scannersIsAsciiLetterdigitdash(c byte) bool {
	return c == '-' || scannersIsAsciiAlphanumeric(c)
}

func scannersIsDigit(c byte) bool {
	return u8IsAsciiDigit(c)
}

func scannersIsValidUnquotedAttrValueChar(c byte) bool {
	switch c {
	case '\'', '"', ' ', '=', '>', '<', '`', '\n', '\r':
		return false
	default:
		return true
	}
}

// scan a single character
func scannersScanCh(data string, c byte) int {
	if len(data) != 0 && data[0] == c {
		return 1
	} else {
		return 0
	}
}

func scannersScanWhile(data string, f func(byte) bool) int {
	count := 0
	for count < len(data) && f(data[count]) {
		count += 1
	}
	return count
}

func scannersScanRevWhile(data string, f func(byte) bool) int {
	count := 0
	for count < len(data) && f(data[len(data)-1-count]) {
		count += 1
	}
	return count
}

func scannersScanChRepeat(data string, c byte) int {
	count := 0
	for count < len(data) && data[count] == c {
		count += 1
	}
	return count
}

// Note: this scans ASCII whitespace only, for Unicode whitespace use
// a different function.
func scannersScanWhitespaceNoNl(data string) int {
	return scannersScanWhile(data, scannersIsAsciiWhitespaceNoNl)
}

func scannersScanAttrValueChars(data string) int {
	return scannersScanWhile(data, scannersIsValidUnquotedAttrValueChar)
}

func scannersScanEol(bytes string) (int, bool) {
	if len(bytes) == 0 {
		return 0, true
	}
	switch bytes[0] {
	case '\n':
		return 1, true
	case '\r':
		if len(bytes) > 1 && bytes[1] == '\n' {
			return 2, true
		}
		return 1, true
	default:
		return 0, false
	}
}

func scannersScanBlankLine(bytes string) (int, bool) {
	i := scannersScanWhitespaceNoNl(bytes)
	n, ok := scannersScanEol(bytes[i:])
	if !ok {
		return 0, false
	}
	return i + n, true
}

func scannersScanNextline(bytes string) int {
	// memchr(b'\n', bytes)
	if x := strings.IndexByte(bytes, '\n'); x >= 0 {
		return x + 1
	}
	return len(bytes)
}

// return: end byte for closing code fence, or None
// if the line is not a closing code fence
func scannersScanClosingCodeFence(
	bytes string,
	fenceChar byte,
	nFenceChar int,
) (int, bool) {
	if len(bytes) == 0 {
		return 0, true
	}
	i := 0
	numFenceCharsFound := scannersScanChRepeat(bytes[i:], fenceChar)
	if numFenceCharsFound < nFenceChar {
		return 0, false
	}
	i += numFenceCharsFound
	numTrailingSpaces := scannersScanChRepeat(bytes[i:], ' ')
	i += numTrailingSpaces
	if _, ok := scannersScanEol(bytes[i:]); ok {
		return i, true
	}
	return 0, false
}

// return: end byte for closing metadata block, or None
// if the line is not a closing metadata block
func scannersScanClosingMetadataBlock(bytes string, fenceChar byte) (int, bool) {
	i := 0
	numFenceCharsFound := scannersScanChRepeat(bytes[i:], fenceChar)
	if numFenceCharsFound != 3 {
		// if YAML style metadata block the closing character can also be `.`
		if fenceChar == '-' {
			numFenceCharsFound = scannersScanChRepeat(bytes[i:], '.')
			if numFenceCharsFound != 3 {
				return 0, false
			}
		} else {
			return 0, false
		}
	}
	i += numFenceCharsFound
	numTrailingSpaces := scannersScanChRepeat(bytes[i:], ' ')
	i += numTrailingSpaces
	if _, ok := scannersScanEol(bytes[i:]); ok {
		return i, true
	}
	return 0, false
}

// returned pair is (number of bytes, number of spaces)
func scannersCalcIndent(text string, max int) (int, int) {
	spaces := 0
	offset := 0

loop:
	for i := 0; i < len(text); i++ {
		b := text[i]
		offset = i
		switch b {
		case ' ':
			spaces += 1
			if spaces == max {
				break loop
			}
		case '\t':
			newSpaces := spaces + 4 - (spaces & 3)
			if newSpaces > max {
				break loop
			}
			spaces = newSpaces
		default:
			break loop
		}
	}

	return offset, spaces
}

// scannersScanHrule scans hrule opening sequence.
//
// Returns Ok(x) when it finds an hrule, where x is the
// size of line containing the hrule, including the trailing newline.
//
// Returns Err(x) when it does not find an hrule and x is
// the offset in data before no hrule can appear.
//
// The second result is true for Ok and false for Err.
func scannersScanHrule(bytes string) (int, bool) {
	if len(bytes) < 3 {
		return 0, false
	}
	c := bytes[0]
	if !(c == '*' || c == '-' || c == '_') {
		return 0, false
	}
	n := 0
	i := 0

loop:
	for i < len(bytes) {
		switch c2 := bytes[i]; {
		case c2 == '\n' || c2 == '\r':
			eol, _ := scannersScanEol(bytes[i:])
			i += eol
			break loop
		case c2 == c:
			n += 1
		case c2 == ' ' || c2 == '\t':
		default:
			return i, false
		}
		i += 1
	}
	if n >= 3 {
		return i, true
	} else {
		return i, false
	}
}

// scannersScanAtxHeading scans an ATX heading opening sequence.
//
// Returns number of bytes in prefix and level.
func scannersScanAtxHeading(data string) (HeadingLevel, bool) {
	level := scannersScanChRepeat(data, '#')
	if level >= len(data) || scannersIsAsciiWhitespace(data[level]) {
		headingLevel, err := HeadingLevelTryFromUsize(level)
		return headingLevel, err == nil
	} else {
		return 0, false
	}
}

// scannersScanSetextHeading scans a setext heading underline.
//
// Returns number of bytes in line (including trailing newline) and level.
func scannersScanSetextHeading(data string) (int, HeadingLevel, bool) {
	if len(data) == 0 {
		return 0, 0, false
	}
	c := data[0]
	var level HeadingLevel
	if c == '=' {
		level = HeadingLevel_H1
	} else if c == '-' {
		level = HeadingLevel_H2
	} else {
		return 0, 0, false
	}
	i := 1 + scannersScanChRepeat(data[1:], c)
	blank, ok := scannersScanBlankLine(data[i:])
	if !ok {
		return 0, 0, false
	}
	i += blank
	return i, level, true
}

// returns number of bytes in line (including trailing
// newline) and column alignments
func scannersScanTableHead(data string) (int, []Alignment) {
	i, spaces := scannersCalcIndent(data, 4)
	if spaces > 3 || i == len(data) {
		return 0, []Alignment{}
	}
	cols := []Alignment{}
	activeCol := Alignment_None
	startCol := true
	foundPipe := false
	foundHyphen := false
	foundHyphenInCol := false
	if data[i] == '|' {
		i += 1
		foundPipe = true
	}
	// `for c in &data[i..]`: the iterator and i advance together.
	for iter := i; iter < len(data); iter++ {
		c := data[iter]
		if n, ok := scannersScanEol(data[i:]); ok {
			i += n
			break
		}
		switch c {
		case ' ':
		case ':':
			switch {
			case startCol && activeCol == Alignment_None:
				activeCol = Alignment_Left
			case !startCol && activeCol == Alignment_Left:
				activeCol = Alignment_Center
			case !startCol && activeCol == Alignment_None:
				activeCol = Alignment_Right
			default:
			}
			startCol = false
		case '-':
			startCol = false
			foundHyphen = true
			foundHyphenInCol = true
		case '|':
			startCol = true
			foundPipe = true
			cols = append(cols, activeCol)
			activeCol = Alignment_None
			if !foundHyphenInCol {
				// It isn't a table head if it has back-to-back pipes.
				return 0, []Alignment{}
			}
			foundHyphenInCol = false
		default:
			// It isn't a table head if it has characters outside the allowed set.
			return 0, []Alignment{}
		}
		i += 1
	}

	if !startCol {
		cols = append(cols, activeCol)
	}
	if !foundPipe || !foundHyphen {
		// It isn't a table head if it doesn't have a least one pipe or hyphen.
		// It's a list, a header, or a thematic break.
		return 0, []Alignment{}
	}

	return i, cols
}

// scannersScanCodeFence scans code fence.
//
// Returns number of bytes scanned and the char that is repeated to make the code fence.
func scannersScanCodeFence(data string) (int, byte, bool) {
	if len(data) == 0 {
		return 0, 0, false
	}
	c := data[0]
	if !(c == '`' || c == '~') {
		return 0, 0, false
	}
	i := 1 + scannersScanChRepeat(data[1:], c)
	if i >= 3 {
		if c == '`' {
			suffix := data[i:]
			nextLine := i + scannersScanNextline(suffix)
			// FIXME: make sure this is correct
			if strings.IndexByte(suffix[:(nextLine-i)], '`') >= 0 {
				return 0, 0, false
			}
		}
		return i, c, true
	} else {
		return 0, 0, false
	}
}

// scannersScanMetadataBlock scans metadata block, returning the number of
// delimiter bytes (always 3 for now) and the delimiter character.
//
// Differently to code blocks, metadata blocks must be closed with the closing
// sequence not being a valid terminator the end of the file.
//
// In addition, they cannot be empty (closing sequence in the next line) and
// the next line cannot be an empty line.
func scannersScanMetadataBlock(
	data string,
	yamlStyleEnabled bool,
	plusesStyleEnabled bool,
) (int, byte, bool) {
	// Only if metadata blocks are enabled
	if yamlStyleEnabled || plusesStyleEnabled {
		if len(data) == 0 {
			return 0, 0, false
		}
		c := data[0]
		if !((c == '-' && yamlStyleEnabled) || (c == '+' && plusesStyleEnabled)) {
			return 0, 0, false
		}
		i := 1 + scannersScanChRepeat(data[1:], c)
		// Only trailing spaces after the delimiters in the line
		nextLine := scannersScanNextline(data[i:])
		for k := i; k < i+nextLine; k++ {
			if !u8IsAsciiWhitespace(data[k]) {
				return 0, 0, false
			}
		}
		if i == 3 {
			// Search the closing sequence
			j := i
			firstLine := true
			for j < len(data) {
				j += scannersScanNextline(data[j:])
				_, closed := scannersScanClosingMetadataBlock(data[j:], c)
				// The first line of the metadata block cannot be an empty line
				// nor the end of the block
				if firstLine {
					if _, blank := scannersScanBlankLine(data[j:]); closed || blank {
						return 0, 0, false
					}
					firstLine = false
				}
				if closed {
					return i, c, true
				}
			}
			return 0, 0, false
		} else {
			return 0, 0, false
		}
	} else {
		return 0, 0, false
	}
}

func scannersScanBlockquoteStart(data string) (int, bool) {
	if len(data) > 0 && data[0] == '>' {
		space := 0
		if len(data) > 1 && data[1] == ' ' {
			space = 1
		}
		return 1 + space, true
	} else {
		return 0, false
	}
}

// return number of bytes scanned, delimiter, start index, and indent
func scannersScanListitem(bytes string) (int, byte, int, int, bool) {
	if len(bytes) == 0 {
		return 0, 0, 0, 0, false
	}
	c := bytes[0]
	var w, start int
	switch {
	case c == '-' || c == '+' || c == '*':
		w, start = 1, 0
	case '0' <= c && c <= '9':
		length, parsed := scannersParseDecimal(bytes, 9)
		if length >= len(bytes) {
			return 0, 0, 0, 0, false
		}
		c = bytes[length]
		if !(c == '.' || c == ')') {
			return 0, 0, 0, 0, false
		}
		w, start = length+1, parsed
	default:
		return 0, 0, 0, 0, false
	}
	// TODO: replace calc_indent with scan_leading_whitespace, for tab correctness
	postn, postindent := scannersCalcIndent(bytes[w:], 5)
	if postindent == 0 {
		if _, ok := scannersScanEol(bytes[w:]); !ok {
			return 0, 0, 0, 0, false
		}
		postindent += 1
	} else if postindent > 4 {
		postn = 1
		postindent = 1
	}
	if _, ok := scannersScanBlankLine(bytes[w:]); ok {
		postn = 0
		postindent = 1
	}
	return w + postn, c, start, w + postindent, true
}

// usizeCheckedMulAdd is `acc.checked_mul(factor).and_then(|x|
// x.checked_add(digit))` for a usize held in an int.
func usizeCheckedMulAdd(acc int, factor int, digit int) (int, bool) {
	const usizeMax = int(^uint(0) >> 1)
	if acc > usizeMax/factor {
		return 0, false
	}
	product := acc * factor
	if product > usizeMax-digit {
		return 0, false
	}
	return product + digit, true
}

// returns (number of bytes, parsed decimal)
func scannersParseDecimal(bytes string, limit int) (int, int) {
	count, acc := 0, 0
	for i := 0; i < len(bytes) && i < limit; i++ {
		c := bytes[i]
		if !scannersIsDigit(c) {
			break
		}
		digit := int(c - '0')
		number, ok := usizeCheckedMulAdd(acc, 10, digit)
		if !ok {
			// stop early on overflow
			return count, acc
		}
		count, acc = count+1, number
	}
	return count, acc
}

// returns (number of bytes, parsed hex)
func scannersParseHex(bytes string, limit int) (int, int) {
	count, acc := 0, 0
	for i := 0; i < len(bytes) && i < limit; i++ {
		c := bytes[i]
		var digit int
		if u8IsAsciiDigit(c) {
			digit = int(c - '0')
		} else {
			// make lower case
			c |= 0x20
			if 'a' <= c && c <= 'f' {
				digit = int(c - 'a' + 10)
			} else {
				return count, acc
			}
		}
		number, ok := usizeCheckedMulAdd(acc, 16, digit)
		if !ok {
			// stop early on overflow
			return count, acc
		}
		count, acc = count+1, number
	}
	return count, acc
}

func scannersCharFromCodepoint(input int) (rune, bool) {
	// input.try_into().ok()? for a u32
	if input < 0 || input > 0xFFFFFFFF {
		return 0, false
	}
	codepoint := uint32(input)
	if codepoint == 0 {
		return 0, false
	}
	// char::from_u32
	if codepoint > 0x10FFFF || (0xD800 <= codepoint && codepoint <= 0xDFFF) {
		return 0, false
	}
	return rune(codepoint), true
}

// doesn't bother to check data[0] == '&'
//
// The third result is false for a None value; the byte count is then 0.
func scannersScanEntity(bytes string) (int, string, bool) {
	end := 1
	if scannersScanCh(bytes[end:], '#') == 1 {
		end += 1
		var bytecount, codepoint int
		if end < len(bytes) && bytes[end]|0x20 == 'x' {
			end += 1
			bytecount, codepoint = scannersParseHex(bytes[end:], 6)
		} else {
			bytecount, codepoint = scannersParseDecimal(bytes[end:], 7)
		}
		end += bytecount
		if bytecount == 0 || scannersScanCh(bytes[end:], ';') == 0 {
			return 0, "", false
		} else {
			c, ok := scannersCharFromCodepoint(codepoint)
			if !ok {
				c = '�'
			}
			return end + 1, string(c), true
		}
	}
	end += scannersScanWhile(bytes[end:], scannersIsAsciiAlphanumeric)
	if scannersScanCh(bytes[end:], ';') == 1 {
		if value, ok := entitiesGetEntity(bytes[1:end]); ok {
			return end + 1, value, true
		}
	}
	return 0, "", false
}

// note: dest returned is raw, still needs to be unescaped
// TODO: check that nested parens are really not allowed for refdefs
// TODO(performance): this func should probably its own unescaping
func scannersScanLinkDest(
	data string,
	startIx int,
	maxNext int,
) (int, string, bool) {
	bytes := data[startIx:]
	i := scannersScanCh(bytes, '<')

	if i != 0 {
		// pointy links
		for i < len(bytes) {
			switch c := bytes[i]; {
			case c == '\n' || c == '\r' || c == '<':
				return 0, "", false
			case c == '>':
				return i + 1, data[(startIx + 1):(startIx + i)], true
			case c == '\\' && i+1 < len(bytes) && puncttableIsAsciiPunctuation(bytes[i+1]):
				i += 1
			default:
			}
			i += 1
		}
		return 0, "", false
	} else {
		// non-pointy links
		nest := 0
	loop:
		for i < len(bytes) {
			switch c := bytes[i]; {
			case c <= 0x20:
				break loop
			case c == '(':
				if nest > maxNext {
					return 0, "", false
				}
				nest += 1
			case c == ')':
				if nest == 0 {
					break loop
				}
				nest -= 1
			case c == '\\' && i+1 < len(bytes) && puncttableIsAsciiPunctuation(bytes[i+1]):
				i += 1
			default:
			}
			i += 1
		}
		if nest != 0 {
			return 0, "", false
		}
		return i, data[startIx:(startIx + i)], true
	}
}

// scannersScanAttributeName returns bytes scanned
func scannersScanAttributeName(data string) (int, bool) {
	if len(data) == 0 {
		return 0, false
	}
	c, tail := data[0], data[1:]
	if scannersIsAsciiAlpha(c) || c == '_' || c == ':' {
		return 1 + scannersScanWhile(tail, func(c byte) bool {
			return scannersIsAsciiAlphanumeric(c) || c == '_' || c == '.' || c == ':' || c == '-'
		}), true
	} else {
		return 0, false
	}
}

// scannersScanAttribute returns the index immediately following the attribute
// on success. The argument `buffer_ix` refers to the index into `data` from
// which we should copy into `buffer` when we find bytes to skip.
//
// newlineHandler is `Option<&dyn Fn(&[u8]) -> usize>`: nil is None.
func scannersScanAttribute(
	data string,
	ix int,
	newlineHandler func(string) int,
	buffer *[]byte,
	bufferIx *int,
) (int, bool) {
	n, ok := scannersScanAttributeName(data[ix:])
	if !ok {
		return 0, false
	}
	ix += n
	ixAfterAttribute := ix
	ix, ok = scannersScanWhitespaceWithNewlineHandlerWithoutBuffer(data, ix, newlineHandler)
	if !ok {
		return 0, false
	}
	if scannersScanCh(data[ix:], '=') == 1 {
		ix, ok = scannersScanWhitespaceWithNewlineHandler(data, ixAfterAttribute, newlineHandler, buffer, bufferIx)
		if !ok {
			return 0, false
		}
		ix += 1
		ix, ok = scannersScanWhitespaceWithNewlineHandler(data, ix, newlineHandler, buffer, bufferIx)
		if !ok {
			return 0, false
		}
		ix, ok = scannersScanAttributeValue(data, ix, newlineHandler, buffer, bufferIx)
		if !ok {
			return 0, false
		}
		return ix, true
	} else {
		// Leave whitespace for next attribute.
		return ixAfterAttribute, true
	}
}

// scannersScanWhitespaceWithNewlineHandler scans whitespace and possibly
// newlines according to the behavior defined by the newline handler. When
// bytes are skipped, all preceding non-skipped bytes are pushed to the
// buffer.
func scannersScanWhitespaceWithNewlineHandler(
	data string,
	i int,
	newlineHandler func(string) int,
	buffer *[]byte,
	bufferIx *int,
) (int, bool) {
	for i < len(data) {
		if !scannersIsAsciiWhitespace(data[i]) {
			return i, true
		}
		if eolBytes, ok := scannersScanEol(data[i:]); ok {
			handler := newlineHandler
			if handler == nil {
				return 0, false
			}
			i += eolBytes
			skippedBytes := handler(data[i:])

			if skippedBytes > 0 {
				*buffer = append(*buffer, data[*bufferIx:i]...)
				*bufferIx = i + skippedBytes
			}

			i += skippedBytes
		} else {
			i += 1
		}
	}

	return i, true
}

// scannersScanWhitespaceWithNewlineHandlerWithoutBuffer scans whitespace and
// possible newlines according to the behavior defined by the newline handler.
//
// Unlike [`scan_whitespace_with_newline_handler`], this function doesn't
// copy skipped data into a buffer. Typically, if this function
// returns `Some`, a call to `scan_whitespace_with_newline_handler` will
// soon follow.
func scannersScanWhitespaceWithNewlineHandlerWithoutBuffer(
	data string,
	i int,
	newlineHandler func(string) int,
) (int, bool) {
	for i < len(data) {
		if !scannersIsAsciiWhitespace(data[i]) {
			return i, true
		}
		if eolBytes, ok := scannersScanEol(data[i:]); ok {
			handler := newlineHandler
			if handler == nil {
				return 0, false
			}
			i += eolBytes
			skippedBytes := handler(data[i:])
			i += skippedBytes
		} else {
			i += 1
		}
	}

	return i, true
}

// scannersScanAttributeValue returns the index immediately following the
// attribute value on success.
func scannersScanAttributeValue(
	data string,
	i int,
	newlineHandler func(string) int,
	buffer *[]byte,
	bufferIx *int,
) (int, bool) {
	if i >= len(data) {
		return 0, false
	}
	switch b := data[i]; b {
	case '"', '\'':
		i += 1
		for i < len(data) {
			if data[i] == b {
				return i + 1, true
			}
			if eolBytes, ok := scannersScanEol(data[i:]); ok {
				handler := newlineHandler
				if handler == nil {
					return 0, false
				}
				i += eolBytes
				skippedBytes := handler(data[i:])

				if skippedBytes > 0 {
					*buffer = append(*buffer, data[*bufferIx:i]...)
					*bufferIx = i + skippedBytes
				}
				i += skippedBytes
			} else {
				i += 1
			}
		}
		return 0, false
	case ' ', '=', '>', '<', '`', '\n', '\r':
		return 0, false
	default:
		// unquoted attribute value
		i += scannersScanAttrValueChars(data[i:])
	}

	return i, true
}

// Remove backslash escapes and resolve entities
func scannersUnescape(input string, isInTable bool) string {
	var result strings.Builder
	mark := 0
	i := 0
	bytes := input
	for i < len(bytes) {
		switch c := bytes[i]; {
		// Tables are special, because they're parsed as-if the tables
		// were parsed in a discrete pass, changing `\|` to `|`, and then
		// passing the changed string to the inline parser.
		case c == '\\' &&
			isInTable &&
			i+2 < len(bytes) &&
			bytes[i+1] == '\\' &&
			bytes[i+2] == '|':
			// even number of `\`s before pipe
			// odd number is handled in the normal way below
			result.WriteString(input[mark:i])
			mark = i + 2
			i += 3
		case c == '\\' && i+1 < len(bytes) && puncttableIsAsciiPunctuation(bytes[i+1]):
			result.WriteString(input[mark:i])
			mark = i + 1
			i += 2
		case c == '&':
			if n, value, ok := scannersScanEntity(bytes[i:]); ok {
				result.WriteString(input[mark:i])
				result.WriteString(value)
				i += n
				mark = i
			} else {
				i += 1
			}
		case c == '\r':
			result.WriteString(input[mark:i])
			i += 1
			mark = i
		default:
			i += 1
		}
	}
	if mark == 0 {
		return input
	} else {
		result.WriteString(input[mark:])
		return result.String()
	}
}

// scannersStartsHtmlBlockType6 assumes `data` is preceded by `<`.
func scannersStartsHtmlBlockType6(data string) bool {
	i := scannersScanCh(data, '/')
	tail := data[i:]
	n := scannersScanWhile(tail, scannersIsAsciiAlphanumeric)
	if !scannersIsHtmlTag(tail[:n]) {
		return false
	}
	// Starting condition says the next byte must be either a space, a tab,
	// the end of the line, the string >, or the string />
	tail = tail[n:]
	return len(tail) == 0 ||
		tail[0] == ' ' ||
		tail[0] == '\t' ||
		tail[0] == '\r' ||
		tail[0] == '\n' ||
		tail[0] == '>' ||
		len(tail) >= 2 && tail[:2] == "/>"
}

func scannersIsHtmlTag(tag string) bool {
	// HTML_TAGS.binary_search_by(..).is_ok()
	left := 0
	right := len(scannersHtmlTags)
	for left < right {
		mid := left + (right-left)/2
		probe := scannersHtmlTags[mid]
		// We can compare case insensitively because the probes are
		// all lower case alpha strings.
		ordering := 0
		for i := 0; i < len(probe) && i < len(tag); i++ {
			a, b := probe[i], tag[i]|0x20
			if a != b {
				if a < b {
					ordering = -1
				} else {
					ordering = 1
				}
				break
			}
		}
		if ordering == 0 {
			switch {
			case len(probe) < len(tag):
				ordering = -1
			case len(probe) > len(tag):
				ordering = 1
			}
		}
		switch {
		case ordering < 0:
			left = mid + 1
		case ordering > 0:
			right = mid
		default:
			return true
		}
	}
	return false
}

// scannersScanHtmlType7 assumes that `data` starts with `<`.
// Returns the index into data directly after the html tag on success.
func scannersScanHtmlType7(data string) (int, bool) {
	// Block type html does not allow for newlines, so we
	// do not pass a newline handler.
	_, i, ok := scannersScanHtmlBlockInner(data, nil)
	if !ok {
		return 0, false
	}
	if _, ok := scannersScanBlankLine(data[i:]); !ok {
		return 0, false
	}
	return i, true
}

// scannersScanHtmlBlockInner assumes that `data` starts with `<`.
// Returns the number of bytes scanned and the html in case of
// success.
// When some bytes were skipped, because the html was split over
// multiple leafs (e.g. over multiple lines in a blockquote),
// the html is returned as a vector of bytes.
// If no bytes were skipped, the buffer will be empty.
//
// newlineHandler is `Option<&dyn Fn(&[u8]) -> usize>`: nil is None.
func scannersScanHtmlBlockInner(
	data string,
	newlineHandler func(string) int,
) ([]byte, int, bool) {
	var buffer []byte
	lastBufIndex := 0

	closeTagBytes := scannersScanCh(data[1:], '/')
	l := scannersScanWhile(data[(1+closeTagBytes):], scannersIsAsciiAlpha)
	if l == 0 {
		return nil, 0, false
	}
	i := 1 + closeTagBytes + l
	i += scannersScanWhile(data[i:], scannersIsAsciiLetterdigitdash)

	if closeTagBytes == 0 {
		for {
			oldI := i
			for {
				i += scannersScanWhitespaceNoNl(data[i:])
				if eolBytes, ok := scannersScanEol(data[i:]); ok {
					if eolBytes == 0 {
						return nil, 0, false
					}
					handler := newlineHandler
					if handler == nil {
						return nil, 0, false
					}
					i += eolBytes
					skippedBytes := handler(data[i:])

					dataLen := len(data) - i

					if rs.DebugAssertions && !(skippedBytes <= dataLen) {
						// debug_assert! with its own message
						rs.PanicAt(0, "Handler tried to skip too many bytes, fed "+strconv.Itoa(dataLen)+", skipped "+strconv.Itoa(skippedBytes))
					}

					if skippedBytes > 0 {
						buffer = append(buffer, data[lastBufIndex:i]...)
						i += skippedBytes
						lastBufIndex = i
					}
				} else {
					break
				}
			}
			if i < len(data) && (data[i] == '/' || data[i] == '>') {
				break
			}
			if oldI == i {
				// No whitespace, which is mandatory.
				return nil, 0, false
			}
			var ok bool
			i, ok = scannersScanAttribute(data, i, newlineHandler, &buffer, &lastBufIndex)
			if !ok {
				return nil, 0, false
			}
		}
	}

	i += scannersScanWhitespaceNoNl(data[i:])

	if closeTagBytes == 0 {
		i += scannersScanCh(data[i:], '/')
	}

	if scannersScanCh(data[i:], '>') == 0 {
		return nil, 0, false
	} else {
		i += 1
		if len(buffer) != 0 {
			buffer = append(buffer, data[lastBufIndex:i]...)
		}
		return buffer, i, true
	}
}

// scannersScanAutolink returns (next_byte_offset, uri, type)
func scannersScanAutolink(text string, startIx int) (int, string, LinkType, bool) {
	if bytes, uri, ok := scannersScanUri(text, startIx); ok {
		return bytes, uri, LinkType_Autolink, true
	}
	if bytes, uri, ok := scannersScanEmail(text, startIx); ok {
		return bytes, uri, LinkType_Email, true
	}
	return 0, "", 0, false
}

// scannersScanUri returns (next_byte_offset, uri)
func scannersScanUri(text string, startIx int) (int, string, bool) {
	bytes := text[startIx:]

	// scheme's first byte must be an ascii letter
	if len(bytes) == 0 || !scannersIsAsciiAlpha(bytes[0]) {
		return 0, "", false
	}

	i := 1

loop:
	for i < len(bytes) {
		c := bytes[i]
		i += 1
		switch {
		case scannersIsAsciiAlphanumeric(c):
		case c == '.' || c == '-' || c == '+':
		case c == ':':
			break loop
		default:
			return 0, "", false
		}
	}

	// scheme length must be between 2 and 32 characters long. scheme
	// must be followed by colon
	if !(3 <= i && i <= 33) {
		return 0, "", false
	}

	for i < len(bytes) {
		switch c := bytes[i]; {
		case c == '>':
			return startIx + i + 1, text[startIx:(startIx + i)], true
		case c <= ' ' || c == '<':
			return 0, "", false
		default:
		}
		i += 1
	}

	return 0, "", false
}

// scannersScanEmail returns (next_byte_offset, email)
func scannersScanEmail(text string, startIx int) (int, string, bool) {
	// using a regex library would be convenient, but doing it by hand is not too bad
	bytes := text[startIx:]
	i := 0

loop:
	for i < len(bytes) {
		c := bytes[i]
		i += 1
		switch {
		case scannersIsAsciiAlphanumeric(c):
		case c == '.' || c == '!' || c == '#' || c == '$' || c == '%' || c == '&' || c == '\'' || c == '*' || c == '+' || c == '/' || c == '=' || c == '?' ||
			c == '^' || c == '_' || c == '`' || c == '{' || c == '|' || c == '}' || c == '~' || c == '-':
		case c == '@' && i > 1:
			break loop
		default:
			return 0, "", false
		}
	}

	for {
		labelStartIx := i
		freshLabel := true

	label:
		for i < len(bytes) {
			switch c := bytes[i]; {
			case scannersIsAsciiAlphanumeric(c):
			case c == '-' && freshLabel:
				return 0, "", false
			case c == '-':
			default:
				break label
			}
			freshLabel = false
			i += 1
		}

		if i == labelStartIx || i-labelStartIx > 63 || bytes[i-1] == '-' {
			return 0, "", false
		}

		if scannersScanCh(bytes[i:], '.') == 0 {
			break
		}
		i += 1
	}

	if scannersScanCh(bytes[i:], '>') == 0 {
		return 0, "", false
	}

	return startIx + i + 1, text[startIx:(startIx + i)], true
}

// scannersScanInlineHtmlComment scans comment, declaration, or CDATA section,
// with initial "<!" already consumed. Returns byte offset on match.
func scannersScanInlineHtmlComment(
	bytes string,
	ix int,
	scanGuard *parseHtmlScanGuard,
) (int, bool) {
	if ix >= len(bytes) {
		return 0, false
	}
	c := bytes[ix]
	ix += 1
	switch {
	// An HTML comment consists of `<!-->`, `<!--->`, or  `<!--`, a string of characters not
	// including the string `-->`, and `-->`.
	case c == '-':
		// HTML comment needs two hyphens after the !.
		if ix >= len(bytes) {
			return 0, false
		}
		if bytes[ix] != '-' {
			return 0, false
		}
		// Yes, we're intentionally going backwards.
		// We want the cursor to point here:
		//
		//     <!--
		//       ^
		//
		// This way, the `<!-->` case is covered by the loop below.
		ix -= 1

		for {
			x := strings.IndexByte(bytes[ix:], '-')
			if x < 0 {
				break
			}
			ix += x + 1
			if scannersScanCh(bytes[ix:], '-') == 1 && scannersScanCh(bytes[ix+1:], '>') == 1 {
				return ix + 2, true
			}
		}
		return 0, false
	// A CDATA section consists of the string `<![CDATA[`, a string of characters not
	// including the string `]]>`, and the string `]]>`.
	case c == '[' && strings.HasPrefix(bytes[ix:], "CDATA[") && ix > scanGuard.cdata:
		ix += len("CDATA[")
		if x := strings.IndexByte(bytes[ix:], ']'); x >= 0 {
			ix = ix + x
		} else {
			ix = len(bytes)
		}
		closeBrackets := scannersScanChRepeat(bytes[ix:], ']')
		ix += closeBrackets

		if closeBrackets == 0 || scannersScanCh(bytes[ix:], '>') == 0 {
			scanGuard.cdata = ix
			return 0, false
		} else {
			return ix + 1, true
		}
	// A declaration consists of the string `<!`, an ASCII letter, zero or more characters not
	// including the character >, and the character >.
	case u8IsAsciiAlphabetic(c) && ix > scanGuard.declaration:
		if x := strings.IndexByte(bytes[ix:], '>'); x >= 0 {
			ix = ix + x
		} else {
			ix = len(bytes)
		}
		if scannersScanCh(bytes[ix:], '>') == 0 {
			scanGuard.declaration = ix
			return 0, false
		} else {
			return ix + 1, true
		}
	default:
		return 0, false
	}
}

// scannersScanInlineHtmlProcessing scans processing directive, with initial
// "<?" already consumed. Returns the next byte offset on success.
func scannersScanInlineHtmlProcessing(
	bytes string,
	ix int,
	scanGuard *parseHtmlScanGuard,
) (int, bool) {
	if ix <= scanGuard.processing {
		return 0, false
	}
	for {
		offset := strings.IndexByte(bytes[ix:], '?')
		if offset < 0 {
			break
		}
		ix += offset + 1
		if scannersScanCh(bytes[ix:], '>') == 1 {
			return ix + 1, true
		}
	}
	scanGuard.processing = ix
	return 0, false
}
