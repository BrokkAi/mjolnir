// Port of pulldown-cmark 0.10.3 src/firstpass.rs

package pulldowncmark

import (
	"strings"

	"github.com/BrokkAi/mjolnir/go/rs"
	"github.com/BrokkAi/mjolnir/go/rs/strs"
)

// The first pass resolves all block structure, generating an AST. Within a block, items
// are in a linear chain with potential inline markup identified.

// firstpassRunFirstPass runs the first pass, which resolves the block
// structure of the document, and returns the resulting tree.
func firstpassRunFirstPass(text string, options Options) (treeTree, parseAllocations) {
	// This is a very naive heuristic for the number of nodes
	// we'll need.
	startCapacity := max(128, len(text)/32)
	lookupTable := firstpassCreateLut(&options)
	firstPass := firstpassFirstPass{
		text:              text,
		tree:              treeTreeWithCapacity(startCapacity),
		beginListItem:     rs.None[int](),
		lastLineBlank:     false,
		allocs:            parseAllocationsNew(),
		options:           options,
		lookupTable:       &lookupTable,
		nextParagraphTask: rs.None[parseItem](),
	}
	return firstPass.run()
}

// firstpassFirstPass is the state for the first parsing pass.
type firstpassFirstPass struct {
	text          string
	tree          treeTree
	beginListItem rs.Option[int]
	lastLineBlank bool
	allocs        parseAllocations
	options       Options
	lookupTable   *firstpassLookupTable
	// This is `Some(item)` when the next paragraph
	// starts with a task list marker.
	nextParagraphTask rs.Option[parseItem]
}

func (firstPass *firstpassFirstPass) run() (treeTree, parseAllocations) {
	ix := 0
	for ix < len(firstPass.text) {
		ix = firstPass.parseBlock(ix)
	}
	for range firstPass.tree.spineLen() {
		firstPass.pop(ix)
	}
	return firstPass.tree, firstPass.allocs
}

// parseBlock returns offset after block.
func (firstPass *firstpassFirstPass) parseBlock(startIx int) int {
	bytes := firstPass.text
	lineStart := scannersLineStartNew(bytes[startIx:])

	i := parseScanContainers(
		&firstPass.tree,
		&lineStart,
		firstPass.options.hasGfmFootnotes(),
	)
	for range usizeRange(i, firstPass.tree.spineLen()) {
		firstPass.pop(startIx)
	}

	if firstPass.options.Contains(OptionsEnableOldFootnotes) {
		// finish footnote if it's still open and was preceded by blank line
		if nodeIx := firstPass.tree.peekUp(); nodeIx != treeNone {
			if firstPass.tree.nodes[nodeIx].item.body.kind == parseItemBody_FootnoteDefinition {
				if firstPass.lastLineBlank {
					firstPass.pop(startIx)
				}
			}
		}

		// Footnote definitions of the form
		// [^bar]:
		// * anything really
		containerStart := startIx + lineStart.bytesScanned()
		if bytecount, ok := firstPass.parseFootnote(containerStart); ok {
			startIx = containerStart + bytecount
			blank, _ := scannersScanBlankLine(bytes[startIx:])
			startIx += blank
			lineStart = scannersLineStartNew(bytes[startIx:])
		}
	}

	// Process new containers
	for {
		if firstPass.options.hasGfmFootnotes() ||
			firstPass.options.Contains(OptionsEnableOldFootnotes) {
			// Footnote definitions of the form
			// [^bar]:
			//     * anything really
			save := lineStart
			indent := lineStart.scanSpaceUpto(4)
			if indent < 4 {
				containerStart := startIx + lineStart.bytesScanned()
				if bytecount, ok := firstPass.parseFootnote(containerStart); ok {
					startIx = containerStart + bytecount
					lineStart = scannersLineStartNew(bytes[startIx:])
					continue
				} else {
					lineStart = save
				}
			} else {
				lineStart = save
			}
		}
		containerStart := startIx + lineStart.bytesScanned()
		if ch, index, indent, ok := lineStart.scanListMarker(); ok {
			afterMarkerIndex := startIx + lineStart.bytesScanned()
			firstPass.continueList(containerStart, ch, index)
			firstPass.tree.append(parseItem{
				start: containerStart,
				end:   afterMarkerIndex, // will get updated later if item not empty
				body:  parseItemBody{kind: parseItemBody_ListItem, n: indent},
			})
			firstPass.tree.push()
			if n, ok := scannersScanBlankLine(bytes[afterMarkerIndex:]); ok {
				firstPass.beginListItem = rs.Some(afterMarkerIndex + n)
				return afterMarkerIndex + n
			}
			if firstPass.options.Contains(OptionsEnableTasklists) {
				if isChecked, ok := lineStart.scanTaskListMarker(); ok {
					firstPass.nextParagraphTask = rs.Some(parseItem{
						start: afterMarkerIndex,
						end:   startIx + lineStart.bytesScanned(),
						body:  parseItemBody{kind: parseItemBody_TaskListMarker, b1: isChecked},
					})
				} else {
					firstPass.nextParagraphTask = rs.None[parseItem]()
				}
			}
		} else if lineStart.scanBlockquoteMarker() {
			firstPass.finishList(startIx)
			firstPass.tree.append(parseItem{
				start: containerStart,
				end:   0, // will get set later
				body:  parseItemBody{kind: parseItemBody_BlockQuote},
			})
			firstPass.tree.push()
		} else {
			break
		}
	}

	ix := startIx + lineStart.bytesScanned()

	if n, ok := scannersScanBlankLine(bytes[ix:]); ok {
		if nodeIx := firstPass.tree.peekUp(); nodeIx != treeNone {
			switch body := &firstPass.tree.nodes[nodeIx].item.body; {
			case body.kind == parseItemBody_BlockQuote:
			case body.kind == parseItemBody_ListItem && firstPass.beginListItem.IsSome():
				firstPass.lastLineBlank = true
				// This is a blank list item.
				// While the list itself can be continued no matter how many blank lines
				// there are between this one and the next one, it cannot nest anything
				// else, so its indentation should not be subtracted from the line start.
				body.n = 0
			default:
				firstPass.lastLineBlank = true
			}
		}
		return ix + n
	}

	firstPass.finishList(startIx)

	// Save `remaining_space` here to avoid needing to backtrack `line_start` for HTML blocks
	remainingSpace := lineStart.remainingSpace()

	indent := lineStart.scanSpaceUpto(4)
	if indent == 4 {
		ix := startIx + lineStart.bytesScanned()
		remainingSpace := lineStart.remainingSpace()
		return firstPass.parseIndentedCodeBlock(ix, remainingSpace)
	}

	ix = startIx + lineStart.bytesScanned()

	if _, metadataBlockCh, ok := scannersScanMetadataBlock(
		bytes[ix:],
		firstPass.options.
			Contains(OptionsEnableYamlStyleMetadataBlocks),
		firstPass.options.
			Contains(OptionsEnablePlusesDelimitedMetadataBlocks),
	); ok {
		return firstPass.parseMetadataBlock(ix, indent, metadataBlockCh)
	}

	// HTML Blocks
	if bytes[ix] == '<' {
		// Types 1-5 are all detected by one function and all end with the same
		// pattern
		if htmlEndTag, ok := firstpassGetHtmlEndTag(bytes[(ix + 1):]); ok {
			return firstPass.parseHtmlBlockType1To5(
				ix,
				htmlEndTag,
				remainingSpace,
				indent,
			)
		}

		// Detect type 6
		if scannersStartsHtmlBlockType6(bytes[(ix + 1):]) {
			return firstPass.parseHtmlBlockType6Or7(ix, remainingSpace, indent)
		}

		// Detect type 7
		if _, ok := scannersScanHtmlType7(bytes[ix:]); ok {
			return firstPass.parseHtmlBlockType6Or7(ix, remainingSpace, indent)
		}
	}

	if n, ok := scannersScanHrule(bytes[ix:]); ok {
		return firstPass.parseHrule(n, ix)
	}

	if atxSize, ok := scannersScanAtxHeading(bytes[ix:]); ok {
		return firstPass.parseAtxHeading(ix, atxSize)
	}

	if n, fenceCh, ok := scannersScanCodeFence(bytes[ix:]); ok {
		return firstPass.parseFencedCodeBlock(ix, indent, fenceCh, n)
	}

	// parse refdef
	for {
		bytecount, label, linkDef, ok := firstPass.parseRefdefTotal(startIx + lineStart.bytesScanned())
		if !ok {
			break
		}
		firstPass.allocs.refdefs.entryOrInsert(label, linkDef)
		containerStart := startIx + lineStart.bytesScanned()
		ix := containerStart + bytecount
		// Refdefs act as if they were contained within a paragraph, for purposes of lazy
		// continuations. For example:
		//
		// ```
		// > [foo]: http://example.com
		// bar
		// ```
		//
		// is equivalent to
		//
		// ```
		// > bar
		//
		// [foo]: http://example.com
		// ```
		if nl, ok := scannersScanBlankLine(bytes[ix:]); ok {
			ix += nl
			lazyLineStart := scannersLineStartNew(bytes[ix:])
			currentContainer := parseScanContainers(
				&firstPass.tree,
				&lazyLineStart,
				firstPass.options.hasGfmFootnotes(),
			) == firstPass.tree.spineLen()
			if !lazyLineStart.scanSpace(4) &&
				firstPass.scanParagraphInterrupt(
					bytes[ix+lazyLineStart.bytesScanned():],
					currentContainer,
				) {
				return ix
			} else {
				lineStart = lazyLineStart
				lineStart.scanAllSpace()
				startIx = ix
			}
		} else {
			return ix
		}
	}

	ix = startIx + lineStart.bytesScanned()

	return firstPass.parseParagraph(ix)
}

// parseTable returns the offset of the first line after the table.
// Assumptions: current focus is a table element and the table header
// matches the separator line (same number of columns).
func (firstPass *firstpassFirstPass) parseTable(
	tableCols int,
	headStart int,
	bodyStart int,
) (int, bool) {
	// filled empty cells are limited to protect against quadratic growth
	// https://github.com/raphlinus/pulldown-cmark/issues/832
	missingEmptyCells := 0
	// parse header. this shouldn't fail because we made sure the table header is ok
	_, theadIx, ok := firstPass.parseTableRowInner(
		headStart,
		tableCols,
		&missingEmptyCells,
	)
	if !ok {
		return 0, false
	}
	firstPass.tree.nodes[theadIx].item.body = parseItemBody{kind: parseItemBody_TableHead}

	// parse body
	ix := bodyStart
	for {
		nextIx, _, ok := firstPass.parseTableRow(
			ix,
			tableCols,
			&missingEmptyCells,
		)
		if !ok {
			break
		}
		ix = nextIx
	}

	firstPass.pop(ix)
	return ix, true
}

// parseTableRowInner: call this when containers are taken care of.
// Returns bytes scanned, row_ix
func (firstPass *firstpassFirstPass) parseTableRowInner(
	ix int,
	rowCells int,
	missingEmptyCells *int,
) (int, treeTreeIndex, bool) {
	// Limit to prevent a malicious input from causing a denial of service.
	const maxAutocompletedCells = 1 << 18 // = 0x40000

	bytes := firstPass.text
	cells := 0
	finalCellIx := treeNone

	oldCur := firstPass.tree.cur()
	rowIx := firstPass.tree.append(parseItem{
		start: ix,
		end:   0, // set at end of this function
		body:  parseItemBody{kind: parseItemBody_TableRow},
	})
	firstPass.tree.push()

	for {
		ix += scannersScanCh(bytes[ix:], '|')
		startIx := ix
		ix += scannersScanWhitespaceNoNl(bytes[ix:])

		if eolBytes, ok := scannersScanEol(bytes[ix:]); ok {
			ix += eolBytes
			break
		}

		cellIx := firstPass.tree.append(parseItem{
			start: startIx,
			end:   ix,
			body:  parseItemBody{kind: parseItemBody_TableCell},
		})
		firstPass.tree.push()
		nextIx, _ := firstPass.parseLine(ix, rs.None[int](), firstpassTableParseMode_Active)

		firstPass.tree.nodes[cellIx].item.end = nextIx
		firstPass.tree.pop()

		ix = nextIx
		cells += 1

		if cells == rowCells {
			finalCellIx = cellIx
		}
	}

	if cur := oldCur; cur != treeNone && cells == 0 {
		firstPass.pop(ix)
		firstPass.tree.nodes[cur].next = treeNone
		return 0, treeNone, false
	}

	// fill empty cells if needed
	// note: this is where GFM and commonmark-extra diverge. we follow
	// GFM here
	for range usizeRange(cells, rowCells) {
		if *missingEmptyCells >= maxAutocompletedCells {
			return 0, treeNone, false
		}
		*missingEmptyCells += 1
		firstPass.tree.append(parseItem{
			start: ix,
			end:   ix,
			body:  parseItemBody{kind: parseItemBody_TableCell},
		})
	}

	// drop excess cells
	if cellIx := finalCellIx; cellIx != treeNone {
		firstPass.tree.nodes[cellIx].next = treeNone
	}

	firstPass.pop(ix)

	return ix, rowIx, true
}

// parseTableRow returns first offset after the row and the tree index of the row.
func (firstPass *firstpassFirstPass) parseTableRow(
	ix int,
	rowCells int,
	missingEmptyCells *int,
) (int, treeTreeIndex, bool) {
	bytes := firstPass.text
	lineStart := scannersLineStartNew(bytes[ix:])
	currentContainer := parseScanContainers(
		&firstPass.tree,
		&lineStart,
		firstPass.options.hasGfmFootnotes(),
	) == firstPass.tree.spineLen()
	if !currentContainer {
		return 0, treeNone, false
	}
	lineStart.scanAllSpace()
	ix += lineStart.bytesScanned()
	if firstpassScanParagraphInterruptNoTable(
		bytes[ix:],
		currentContainer,
		firstPass.options.hasGfmFootnotes(),
		&firstPass.tree,
	) {
		return 0, treeNone, false
	}

	ix, rowIx, ok := firstPass.parseTableRowInner(ix, rowCells, missingEmptyCells)
	if !ok {
		return 0, treeNone, false
	}
	return ix, rowIx, true
}

// parseParagraph returns offset of line start after paragraph.
func (firstPass *firstpassFirstPass) parseParagraph(startIx int) int {
	nodeIx := firstPass.tree.append(parseItem{
		start: startIx,
		end:   0, // will get set later
		body:  parseItemBody{kind: parseItemBody_Paragraph},
	})
	firstPass.tree.push()

	if item, ok := firstPass.nextParagraphTask.Get(); ok {
		firstPass.tree.append(item)
		firstPass.nextParagraphTask = rs.None[parseItem]()
	}

	bytes := firstPass.text
	ix := startIx
	for {
		scanMode := firstpassTableParseMode_Disabled
		if firstPass.options.Contains(OptionsEnableTables) && ix == startIx {
			scanMode = firstpassTableParseMode_Scan
		}
		nextIx, brk := firstPass.parseLine(ix, rs.None[int](), scanMode)

		// break out when we find a table
		if item, ok := brk.Get(); ok && item.body.kind == parseItemBody_Table {
			alignmentIx := parseAlignmentIndex(item.body.n)
			tableCols := len(firstPass.allocs.alignments[alignmentIx])
			firstPass.tree.nodes[nodeIx].item.body = parseItemBody{kind: parseItemBody_Table, n: int(alignmentIx)}
			// this clears out any stuff we may have appended - but there may
			// be a cleaner way
			firstPass.tree.nodes[nodeIx].child = treeNone
			firstPass.tree.pop()
			firstPass.tree.push()
			if ix, ok := firstPass.parseTable(tableCols, ix, nextIx); ok {
				return ix
			}
		}

		ix = nextIx
		lineStart := scannersLineStartNew(bytes[ix:])
		currentContainer := parseScanContainers(
			&firstPass.tree,
			&lineStart,
			firstPass.options.hasGfmFootnotes(),
		) == firstPass.tree.spineLen()
		trailingBackslashPos := rs.None[int]()
		if item, ok := brk.Get(); ok && item.body.kind == parseItemBody_HardBreak && item.body.b1 && bytes[item.start] == '\\' {
			trailingBackslashPos = rs.Some(item.start)
		}
		if !lineStart.scanSpace(4) {
			ixNew := ix + lineStart.bytesScanned()
			if currentContainer {
				if ixSetext, ok := firstPass.parseSetextHeading(ixNew, nodeIx, trailingBackslashPos.IsSome()); ok {
					if pos, ok := trailingBackslashPos.Get(); ok {
						firstPass.tree.appendText(pos, pos+1, false)
					}
					ix = ixSetext
					break
				}
			}
			// first check for non-empty lists, then for other interrupts
			suffix := bytes[ixNew:]
			if firstPass.scanParagraphInterrupt(suffix, currentContainer) {
				if pos, ok := trailingBackslashPos.Get(); ok {
					firstPass.tree.appendText(pos, pos+1, false)
				}
				break
			}
		}
		lineStart.scanAllSpace()
		if lineStart.isAtEol() {
			if pos, ok := trailingBackslashPos.Get(); ok {
				firstPass.tree.appendText(pos, pos+1, false)
			}
			break
		}
		ix = nextIx + lineStart.bytesScanned()
		if item, ok := brk.Get(); ok {
			firstPass.tree.append(item)
		}
	}

	firstPass.pop(ix)
	return ix
}

// parseSetextHeading returns end ix of setext_heading on success.
func (firstPass *firstpassFirstPass) parseSetextHeading(
	ix int,
	nodeIx treeTreeIndex,
	hasTrailingContent bool,
) (int, bool) {
	bytes := firstPass.text
	n, level, ok := scannersScanSetextHeading(bytes[ix:])
	if !ok {
		return 0, false
	}
	attrs := rs.None[parseHeadingAttributes]()

	if curIx := firstPass.tree.cur(); curIx != treeNone {
		parentIx := firstPass.tree.peekUp().unwrap()
		headerStart := firstPass.tree.nodes[parentIx].item.start
		// Note that `self.tree[parent_ix].item.end` might be zero at this point.
		// Use the end position of the current node (i.e. the last known child
		// of the parent) instead.
		headerEnd := firstPass.tree.nodes[curIx].item.end

		// extract the trailing attribute block
		contentEnd, attrs_ := firstPass.extractAndParseHeadingAttributeBlock(headerStart, headerEnd)
		attrs = attrs_

		// strip trailing whitespace
		var newEnd int
		if hasTrailingContent {
			newEnd = contentEnd
		} else {
			trailingWs := scannersScanRevWhile(bytes[headerStart:contentEnd], scannersIsAsciiWhitespaceNoNl)
			newEnd = contentEnd - trailingWs
		}

		if attrs.IsSome() {
			// remove trailing block attributes
			firstPass.tree.truncateSiblings(newEnd)
		}

		if curIx := firstPass.tree.cur(); curIx != treeNone {
			firstPass.tree.nodes[curIx].item.end = newEnd
		}
	}

	headingIx := parseHeadingIndex(0)
	if attrs, ok := attrs.Get(); ok {
		headingIx = firstPass.allocs.allocateHeading(attrs)
	}
	firstPass.tree.nodes[nodeIx].item.body = parseItemBody{
		kind: parseItemBody_Heading,
		c:    byte(level),
		n:    int(headingIx),
	}

	return ix + n, true
}

// parseLine parses a line of input, appending text and items to tree.
//
// Returns: index after line and an item representing the break.
func (firstPass *firstpassFirstPass) parseLine(
	start int,
	end rs.Option[int],
	mode firstpassTableParseMode,
) (int, rs.Option[parseItem]) {
	bytes := firstPass.text
	if end, ok := end.Get(); ok {
		bytes = bytes[:end]
	}
	bytesLen := len(bytes)
	pipes := 0
	lastPipeIx := start
	beginText := start
	backslashEscaped := false

	finalIx, brk := firstpassIterateSpecialBytes(firstPass.lookupTable, bytes, start, func(ix int, byte_ byte) firstpassLoopInstruction {
		switch {
		case byte_ == '\n' || byte_ == '\r':
			if mode == firstpassTableParseMode_Active {
				return firstpassBreakAtWith(ix, rs.None[parseItem]())
			}

			i := ix
			eolBytes, ok := scannersScanEol(bytes[ix:])
			if !ok {
				rs.PanicAt(0, optionUnwrapNone)
			}

			endIx := ix + eolBytes
			trailingBackslashes := scannersScanRevWhile(bytes[:ix], func(b byte) bool { return b == '\\' })
			if trailingBackslashes%2 == 1 && endIx < bytesLen {
				i -= 1
				firstPass.tree.appendText(beginText, i, backslashEscaped)
				backslashEscaped = false
				return firstpassBreakAtWith(
					endIx,
					rs.Some(parseItem{
						start: i,
						end:   endIx,
						body:  parseItemBody{kind: parseItemBody_HardBreak, b1: true},
					}),
				)
			}

			if mode == firstpassTableParseMode_Scan && pipes > 0 {
				// check if we may be parsing a table
				nextLineIx := ix + eolBytes
				lineStart := scannersLineStartNew(bytes[nextLineIx:])
				if parseScanContainers(
					&firstPass.tree,
					&lineStart,
					firstPass.options.hasGfmFootnotes(),
				) == firstPass.tree.spineLen() {
					tableHeadIx := nextLineIx + lineStart.bytesScanned()
					tableHeadBytes, alignment := scannersScanTableHead(bytes[tableHeadIx:])

					if tableHeadBytes > 0 {
						// computing header count from number of pipes
						headerCount := firstpassCountHeaderCols(bytes, pipes, start, lastPipeIx)

						// make sure they match the number of columns we find in separator line
						if len(alignment) == headerCount {
							alignmentIx := firstPass.allocs.allocateAlignment(alignment)
							endIx := tableHeadIx + tableHeadBytes
							return firstpassBreakAtWith(
								endIx,
								rs.Some(parseItem{
									start: i,
									end:   endIx, // must update later
									body:  parseItemBody{kind: parseItemBody_Table, n: int(alignmentIx)},
								}),
							)
						}
					}
				}
			}

			trailingWhitespace := scannersScanRevWhile(bytes[:ix], scannersIsAsciiWhitespaceNoNl)
			if trailingWhitespace >= 2 {
				i -= trailingWhitespace
				firstPass.tree.appendText(beginText, i, backslashEscaped)
				backslashEscaped = false
				return firstpassBreakAtWith(
					endIx,
					rs.Some(parseItem{
						start: i,
						end:   endIx,
						body:  parseItemBody{kind: parseItemBody_HardBreak, b1: false},
					}),
				)
			}

			firstPass.tree.
				appendText(beginText, ix-trailingWhitespace, backslashEscaped)
			backslashEscaped = false

			return firstpassBreakAtWith(
				endIx,
				rs.Some(parseItem{
					start: i,
					end:   endIx,
					body:  parseItemBody{kind: parseItemBody_SoftBreak},
				}),
			)
		case byte_ == '\\':
			if ix+1 < bytesLen && puncttableIsAsciiPunctuation(bytes[ix+1]) {
				firstPass.tree.appendText(beginText, ix, backslashEscaped)
				if bytes[ix+1] == '`' {
					count := 1 + scannersScanChRepeat(bytes[(ix+2):], '`')
					firstPass.tree.append(parseItem{
						start: ix + 1,
						end:   ix + count + 1,
						body:  parseItemBody{kind: parseItemBody_MaybeCode, n: count, b1: true},
					})
					beginText = ix + 1 + count
					backslashEscaped = false
					return firstpassContinueAndSkip(count)
				} else if bytes[ix+1] == '|' && firstpassTableParseMode_Active == mode {
					// Yeah, it's super weird that backslash escaped pipes in tables aren't "real"
					// backslash escapes.
					//
					// This tree structure is intended for the benefit of inline analysis, and it
					// is supposed to operate as-if backslash escaped pipes were stripped out in a
					// separate pass.
					beginText = ix + 1
					backslashEscaped = false
					return firstpassContinueAndSkip(1)
				} else if ix+2 < bytesLen &&
					bytes[ix+1] == '\\' &&
					bytes[ix+2] == '|' &&
					firstpassTableParseMode_Active == mode {
					// To parse `\\|`, discard the backslashes and parse the `|` that follows it.
					beginText = ix + 2
					backslashEscaped = true
					return firstpassContinueAndSkip(2)
				} else {
					beginText = ix + 1
					backslashEscaped = true
					return firstpassContinueAndSkip(1)
				}
			} else {
				return firstpassContinueAndSkip(0)
			}
		case byte_ == '*' || byte_ == '_' || byte_ == '~':
			c := byte_
			stringSuffix := firstPass.text[ix:]
			count := 1 + scannersScanChRepeat(stringSuffix[1:], c)
			canOpen := firstpassDelimRunCanOpen(
				firstPass.text[start:],
				stringSuffix,
				count,
				ix-start,
				mode,
			)
			canClose := firstpassDelimRunCanClose(
				firstPass.text[start:],
				stringSuffix,
				count,
				ix-start,
				mode,
			)
			isValidSeq := c != '~' || count <= 2

			if (canOpen || canClose) && isValidSeq {
				firstPass.tree.appendText(beginText, ix, backslashEscaped)
				backslashEscaped = false
				for i := 0; i < count; i++ {
					firstPass.tree.append(parseItem{
						start: ix + i,
						end:   ix + i + 1,
						body:  parseItemBody{kind: parseItemBody_MaybeEmphasis, n: count - i, b1: canOpen, b2: canClose},
					})
				}
				beginText = ix + count
			}
			return firstpassContinueAndSkip(count - 1)
		case byte_ == '`':
			firstPass.tree.appendText(beginText, ix, backslashEscaped)
			backslashEscaped = false
			count := 1 + scannersScanChRepeat(bytes[(ix+1):], '`')
			firstPass.tree.append(parseItem{
				start: ix,
				end:   ix + count,
				body:  parseItemBody{kind: parseItemBody_MaybeCode, n: count, b1: false},
			})
			beginText = ix + count
			return firstpassContinueAndSkip(count - 1)
		case byte_ == '<' && !(ix+1 < len(bytes) && bytes[ix+1] == '\\'):
			// Note: could detect some non-HTML cases and early escape here, but not
			// clear that's a win.
			firstPass.tree.appendText(beginText, ix, backslashEscaped)
			backslashEscaped = false
			firstPass.tree.append(parseItem{
				start: ix,
				end:   ix + 1,
				body:  parseItemBody{kind: parseItemBody_MaybeHtml},
			})
			beginText = ix + 1
			return firstpassContinueAndSkip(0)
		case byte_ == '!':
			if ix+1 < bytesLen && bytes[ix+1] == '[' {
				firstPass.tree.appendText(beginText, ix, backslashEscaped)
				backslashEscaped = false
				firstPass.tree.append(parseItem{
					start: ix,
					end:   ix + 2,
					body:  parseItemBody{kind: parseItemBody_MaybeImage},
				})
				beginText = ix + 2
				return firstpassContinueAndSkip(1)
			} else {
				return firstpassContinueAndSkip(0)
			}
		case byte_ == '[':
			firstPass.tree.appendText(beginText, ix, backslashEscaped)
			backslashEscaped = false
			firstPass.tree.append(parseItem{
				start: ix,
				end:   ix + 1,
				body:  parseItemBody{kind: parseItemBody_MaybeLinkOpen},
			})
			beginText = ix + 1
			return firstpassContinueAndSkip(0)
		case byte_ == ']':
			firstPass.tree.appendText(beginText, ix, backslashEscaped)
			backslashEscaped = false
			firstPass.tree.append(parseItem{
				start: ix,
				end:   ix + 1,
				body:  parseItemBody{kind: parseItemBody_MaybeLinkClose, b1: true},
			})
			beginText = ix + 1
			return firstpassContinueAndSkip(0)
		case byte_ == '&':
			if n, value, ok := scannersScanEntity(bytes[ix:]); ok {
				firstPass.tree.appendText(beginText, ix, backslashEscaped)
				backslashEscaped = false
				firstPass.tree.append(parseItem{
					start: ix,
					end:   ix + n,
					body:  parseItemBody{kind: parseItemBody_SynthesizeText, n: int(firstPass.allocs.allocateCow(value))},
				})
				beginText = ix + n
				return firstpassContinueAndSkip(n - 1)
			}
			return firstpassContinueAndSkip(0)
		case byte_ == '|':
			if ix != 0 && bytes[ix-1] == '\\' {
				return firstpassContinueAndSkip(0)
			} else if mode == firstpassTableParseMode_Active {
				return firstpassBreakAtWith(ix, rs.None[parseItem]())
			} else {
				lastPipeIx = ix
				pipes += 1
				return firstpassContinueAndSkip(0)
			}
		case byte_ == '.':
			if ix+2 < len(bytes) && bytes[ix+1] == '.' && bytes[ix+2] == '.' {
				firstPass.tree.appendText(beginText, ix, backslashEscaped)
				backslashEscaped = false
				firstPass.tree.append(parseItem{
					start: ix,
					end:   ix + 3,
					body:  parseItemBodySynthesizeChar('…'),
				})
				beginText = ix + 3
				return firstpassContinueAndSkip(2)
			} else {
				return firstpassContinueAndSkip(0)
			}
		case byte_ == '-':
			count := 1 + scannersScanChRepeat(bytes[(ix+1):], '-')
			if count == 1 {
				return firstpassContinueAndSkip(0)
			} else {
				var itembody parseItemBody
				if count == 2 {
					itembody = parseItemBodySynthesizeChar('–')
				} else if count == 3 {
					itembody = parseItemBodySynthesizeChar('—')
				} else {
					var ems, ens int
					switch count % 6 {
					case 0, 3:
						ems, ens = count/3, 0
					case 2, 4:
						ems, ens = 0, count/2
					case 1:
						ems, ens = count/3-1, 2
					default:
						ems, ens = count/3, 1
					}
					// – and — are 3 bytes each in utf8
					var buf strings.Builder
					buf.Grow(3 * (ems + ens))
					for range ems {
						buf.WriteString("—")
					}
					for range ens {
						buf.WriteString("–")
					}
					itembody = parseItemBody{kind: parseItemBody_SynthesizeText, n: int(firstPass.allocs.allocateCow(buf.String()))}
				}

				firstPass.tree.appendText(beginText, ix, backslashEscaped)
				backslashEscaped = false
				firstPass.tree.append(parseItem{
					start: ix,
					end:   ix + count,
					body:  itembody,
				})
				beginText = ix + count
				return firstpassContinueAndSkip(count - 1)
			}
		case byte_ == '\'' || byte_ == '"':
			c := byte_
			stringSuffix := firstPass.text[ix:]
			canOpen := firstpassDelimRunCanOpen(firstPass.text[start:], stringSuffix, 1, ix-start, mode)
			canClose := firstpassDelimRunCanClose(
				firstPass.text[start:],
				stringSuffix,
				1,
				ix-start,
				mode,
			)

			firstPass.tree.appendText(beginText, ix, backslashEscaped)
			backslashEscaped = false
			firstPass.tree.append(parseItem{
				start: ix,
				end:   ix + 1,
				body:  parseItemBody{kind: parseItemBody_MaybeSmartQuote, c: c, b1: canOpen, b2: canClose},
			})
			beginText = ix + 1

			return firstpassContinueAndSkip(0)
		default:
			return firstpassContinueAndSkip(0)
		}
	})

	if brk.IsNone() {
		trailingWhitespace := scannersScanRevWhile(bytes[beginText:finalIx], scannersIsAsciiWhitespaceNoNl)
		// need to close text at eof
		firstPass.tree.appendText(
			beginText,
			finalIx-trailingWhitespace,
			backslashEscaped,
		)
	}
	return finalIx, brk
}

// parseHtmlBlockType1To5: when start_ix is at the beginning of an HTML block
// of type 1 to 5, this will find the end of the block, adding the block
// itself to the tree and also keeping track of the lines of HTML within the
// block.
//
// The html_end_tag is the tag that must be found on a line to end the block.
func (firstPass *firstpassFirstPass) parseHtmlBlockType1To5(
	startIx int,
	htmlEndTag string,
	remainingSpace int,
	indent int,
) int {
	firstPass.tree.append(parseItem{
		start: startIx,
		end:   0, // set later
		body:  parseItemBody{kind: parseItemBody_HtmlBlock},
	})
	firstPass.tree.push()

	bytes := firstPass.text
	ix := startIx
	var endIx int
	for {
		lineStartIx := ix
		ix += scannersScanNextline(bytes[ix:])
		firstPass.appendHtmlLine(max(remainingSpace, indent), lineStartIx, ix)

		lineStart := scannersLineStartNew(bytes[ix:])
		nContainers := parseScanContainers(
			&firstPass.tree,
			&lineStart,
			firstPass.options.hasGfmFootnotes(),
		)
		if nContainers < firstPass.tree.spineLen() {
			endIx = ix
			break
		}

		if strings.Contains(firstPass.text[lineStartIx:ix], htmlEndTag) {
			endIx = ix
			break
		}

		nextLineIx := ix + lineStart.bytesScanned()
		if nextLineIx == len(firstPass.text) {
			endIx = nextLineIx
			break
		}
		ix = nextLineIx
		remainingSpace = lineStart.remainingSpace()
		indent = 0
	}
	firstPass.pop(endIx)
	return ix
}

// parseHtmlBlockType6Or7: when start_ix is at the beginning of an HTML block
// of type 6 or 7, this will consume lines until there is a blank line and
// keep track of the HTML within the block.
func (firstPass *firstpassFirstPass) parseHtmlBlockType6Or7(
	startIx int,
	remainingSpace int,
	indent int,
) int {
	firstPass.tree.append(parseItem{
		start: startIx,
		end:   0, // set later
		body:  parseItemBody{kind: parseItemBody_HtmlBlock},
	})
	firstPass.tree.push()

	bytes := firstPass.text
	ix := startIx
	var endIx int
	for {
		lineStartIx := ix
		ix += scannersScanNextline(bytes[ix:])
		firstPass.appendHtmlLine(max(remainingSpace, indent), lineStartIx, ix)

		lineStart := scannersLineStartNew(bytes[ix:])
		nContainers := parseScanContainers(
			&firstPass.tree,
			&lineStart,
			firstPass.options.hasGfmFootnotes(),
		)
		if nContainers < firstPass.tree.spineLen() || lineStart.isAtEol() {
			endIx = ix
			break
		}

		nextLineIx := ix + lineStart.bytesScanned()
		if _, blank := scannersScanBlankLine(bytes[nextLineIx:]); nextLineIx == len(firstPass.text) || blank {
			endIx = nextLineIx
			break
		}
		ix = nextLineIx
		remainingSpace = lineStart.remainingSpace()
		indent = 0
	}
	firstPass.pop(endIx)
	return ix
}

func (firstPass *firstpassFirstPass) parseIndentedCodeBlock(startIx int, remainingSpace int) int {
	firstPass.tree.append(parseItem{
		start: startIx,
		end:   0, // will get set later
		body:  parseItemBody{kind: parseItemBody_IndentCodeBlock},
	})
	firstPass.tree.push()
	bytes := firstPass.text
	lastNonblankChild := treeNone
	lastNonblankIx := 0
	endIx := 0
	firstPass.lastLineBlank = false

	ix := startIx
	for {
		lineStartIx := ix
		ix += scannersScanNextline(bytes[ix:])
		firstPass.appendCodeText(remainingSpace, lineStartIx, ix)
		// TODO(spec clarification): should we synthesize newline at EOF?

		if !firstPass.lastLineBlank {
			lastNonblankChild = firstPass.tree.cur()
			lastNonblankIx = ix
			endIx = ix
		}

		lineStart := scannersLineStartNew(bytes[ix:])
		nContainers := parseScanContainers(
			&firstPass.tree,
			&lineStart,
			firstPass.options.hasGfmFootnotes(),
		)
		if nContainers < firstPass.tree.spineLen() ||
			!(lineStart.scanSpace(4) || lineStart.isAtEol()) {
			break
		}
		nextLineIx := ix + lineStart.bytesScanned()
		if nextLineIx == len(firstPass.text) {
			break
		}
		ix = nextLineIx
		remainingSpace = lineStart.remainingSpace()
		_, firstPass.lastLineBlank = scannersScanBlankLine(bytes[ix:])
	}

	// Trim trailing blank lines.
	if child := lastNonblankChild; child != treeNone {
		firstPass.tree.nodes[child].next = treeNone
		firstPass.tree.nodes[child].item.end = lastNonblankIx
	}
	firstPass.pop(endIx)
	return ix
}

func (firstPass *firstpassFirstPass) parseFencedCodeBlock(
	startIx int,
	indent int,
	fenceCh byte,
	nFenceChar int,
) int {
	bytes := firstPass.text
	infoStart := startIx + nFenceChar
	infoStart += scannersScanWhitespaceNoNl(bytes[infoStart:])
	// TODO: info strings are typically very short. wouldn't it be faster
	// to just do a forward scan here?
	ix := infoStart + scannersScanNextline(bytes[infoStart:])
	infoEnd := ix - scannersScanRevWhile(bytes[infoStart:ix], scannersIsAsciiWhitespace)
	infoString := scannersUnescape(firstPass.text[infoStart:infoEnd], firstPass.tree.isInTable())
	firstPass.tree.append(parseItem{
		start: startIx,
		end:   0, // will get set later
		body:  parseItemBody{kind: parseItemBody_FencedCodeBlock, n: int(firstPass.allocs.allocateCow(infoString))},
	})
	firstPass.tree.push()
	for {
		lineStart := scannersLineStartNew(bytes[ix:])
		nContainers := parseScanContainers(
			&firstPass.tree,
			&lineStart,
			firstPass.options.hasGfmFootnotes(),
		)
		if nContainers < firstPass.tree.spineLen() {
			// this line will get parsed again as not being part of the code
			// if it's blank, it should be parsed as a blank line
			firstPass.pop(ix)
			return ix
		}
		lineStart.scanSpace(indent)
		closeLineStart := lineStart
		if !closeLineStart.scanSpace(4 - indent) {
			closeIx := ix + closeLineStart.bytesScanned()
			if n, ok := scannersScanClosingCodeFence(bytes[closeIx:], fenceCh, nFenceChar); ok {
				ix = closeIx + n
				firstPass.pop(ix)
				// try to read trailing whitespace or it will register as a completely blank line
				blank, _ := scannersScanBlankLine(bytes[ix:])
				return ix + blank
			}
		}
		remainingSpace := lineStart.remainingSpace()
		ix += lineStart.bytesScanned()
		nextIx := ix + scannersScanNextline(bytes[ix:])
		firstPass.appendCodeText(remainingSpace, ix, nextIx)
		ix = nextIx
	}
}

func (firstPass *firstpassFirstPass) parseMetadataBlock(
	startIx int,
	indent int,
	metadataBlockCh byte,
) int {
	// metadata blocks cannot be indented
	if indent > 0 {
		return 0
	}
	bytes := firstPass.text
	var metadataBlockKind MetadataBlockKind
	switch metadataBlockCh {
	case '-':
		metadataBlockKind = MetadataBlockKind_YamlStyle
	case '+':
		metadataBlockKind = MetadataBlockKind_PlusesStyle
	default:
		rs.PanicAt(0, "Erroneous metadata block character when parsing metadata block")
	}
	// 3 delimiter characters
	ix := startIx + 3 + scannersScanNextline(bytes[startIx+3:])
	firstPass.tree.append(parseItem{
		start: startIx,
		end:   0, // will get set later
		body:  parseItemBody{kind: parseItemBody_MetadataBlock, c: byte(metadataBlockKind)},
	})
	firstPass.tree.push()
	for {
		lineStart := scannersLineStartNew(bytes[ix:])
		nContainers := parseScanContainers(
			&firstPass.tree,
			&lineStart,
			firstPass.options.hasGfmFootnotes(),
		)
		if nContainers < firstPass.tree.spineLen() {
			break
		}
		if _, spaces := scannersCalcIndent(bytes[ix:], 4); spaces == 0 {
			if n, ok := scannersScanClosingMetadataBlock(bytes[ix:], metadataBlockCh); ok {
				ix += n
				break
			}
		}
		remainingSpace := lineStart.remainingSpace()
		ix += lineStart.bytesScanned()
		nextIx := ix + scannersScanNextline(bytes[ix:])
		firstPass.appendCodeText(remainingSpace, ix, nextIx)
		ix = nextIx
	}

	firstPass.pop(ix)

	// try to read trailing whitespace or it will register as a completely blank line
	blank, _ := scannersScanBlankLine(bytes[ix:])
	return ix + blank
}

func (firstPass *firstpassFirstPass) appendCodeText(remainingSpace int, start int, end int) {
	if remainingSpace > 0 {
		cowIx := firstPass.allocs.allocateCow("   "[:remainingSpace])
		firstPass.tree.append(parseItem{
			start: start,
			end:   start,
			body:  parseItemBody{kind: parseItemBody_SynthesizeText, n: int(cowIx)},
		})
	}
	if firstPass.text[end-2] == '\r' {
		// Normalize CRLF to LF
		firstPass.tree.appendText(start, end-2, false)
		firstPass.tree.appendText(end-1, end, false)
	} else {
		firstPass.tree.appendText(start, end, false)
	}
}

// appendHtmlLine appends a line of HTML to the tree.
func (firstPass *firstpassFirstPass) appendHtmlLine(remainingSpace int, start int, end int) {
	if remainingSpace > 0 {
		cowIx := firstPass.allocs.allocateCow("   "[:remainingSpace])
		firstPass.tree.append(parseItem{
			start: start,
			end:   start,
			body:  parseItemBody{kind: parseItemBody_SynthesizeText, n: int(cowIx)},
		})
	}
	if firstPass.text[end-2] == '\r' {
		// Normalize CRLF to LF
		firstPass.tree.append(parseItem{
			start: start,
			end:   end - 2,
			body:  parseItemBody{kind: parseItemBody_Html},
		})
		firstPass.tree.append(parseItem{
			start: end - 1,
			end:   end,
			body:  parseItemBody{kind: parseItemBody_Html},
		})
	} else {
		firstPass.tree.append(parseItem{
			start: start,
			end:   end,
			body:  parseItemBody{kind: parseItemBody_Html},
		})
	}
}

// pop pops a container, setting its end.
func (firstPass *firstpassFirstPass) pop(ix int) {
	curIx := firstPass.tree.pop().unwrap()
	firstPass.tree.nodes[curIx].item.end = ix
	if body := firstPass.tree.nodes[curIx].item.body; body.kind == parseItemBody_List && body.b1 {
		firstpassSurgerizeTightList(&firstPass.tree, curIx)
		firstPass.beginListItem = rs.None[int]()
	}
}

// finishList closes a list if it's open. Also set loose if last line was
// blank and end current list if it's a lone, empty item
func (firstPass *firstpassFirstPass) finishList(ix int) {
	firstPass.finishEmptyListItem()
	if nodeIx := firstPass.tree.peekUp(); nodeIx != treeNone {
		if firstPass.tree.nodes[nodeIx].item.body.kind == parseItemBody_List {
			firstPass.pop(ix)
		}
	}
	if firstPass.lastLineBlank {
		if nodeIx := firstPass.tree.peekGrandparent(); nodeIx != treeNone {
			if body := &firstPass.tree.nodes[nodeIx].item.body; body.kind == parseItemBody_List {
				// *is_tight = false
				body.b1 = false
			}
		}
		firstPass.lastLineBlank = false
	}
}

func (firstPass *firstpassFirstPass) finishEmptyListItem() {
	if beginListItem, ok := firstPass.beginListItem.Get(); ok {
		if firstPass.lastLineBlank {
			// A list item can begin with at most one blank line.
			if nodeIx := firstPass.tree.peekUp(); nodeIx != treeNone {
				if firstPass.tree.nodes[nodeIx].item.body.kind == parseItemBody_ListItem {
					firstPass.pop(beginListItem)
				}
			}
		}
	}
	firstPass.beginListItem = rs.None[int]()
}

// continueList continues an existing list or start a new one if there's not
// an open list that matches.
func (firstPass *firstpassFirstPass) continueList(start int, ch byte, index uint64) {
	firstPass.finishEmptyListItem()
	if nodeIx := firstPass.tree.peekUp(); nodeIx != treeNone {
		if body := &firstPass.tree.nodes[nodeIx].item.body; body.kind == parseItemBody_List {
			existingCh := body.c
			if existingCh == ch {
				if firstPass.lastLineBlank {
					// *is_tight = false
					body.b1 = false
					firstPass.lastLineBlank = false
				}
				return
			}
		}
		// TODO: this is not the best choice for end; maybe get end from last list item.
		firstPass.finishList(start)
	}
	firstPass.tree.append(parseItem{
		start: start,
		end:   0, // will get set later
		body:  parseItemBody{kind: parseItemBody_List, b1: true, c: ch, u: index},
	})
	firstPass.tree.push()
	firstPass.lastLineBlank = false
}

// parseHrule parses a thematic break.
//
// Returns index of start of next line.
func (firstPass *firstpassFirstPass) parseHrule(hruleSize int, ix int) int {
	firstPass.tree.append(parseItem{
		start: ix,
		end:   ix + hruleSize,
		body:  parseItemBody{kind: parseItemBody_Rule},
	})
	return ix + hruleSize
}

// parseAtxHeading parses an ATX heading.
//
// Returns index of start of next line.
func (firstPass *firstpassFirstPass) parseAtxHeading(start int, atxLevel HeadingLevel) int {
	ix := start
	headingIx := firstPass.tree.append(parseItem{
		start: start,
		end:   0,                      // set later
		body:  parseItemBodyDefault(), // set later
	})
	ix += int(atxLevel)
	// next char is space or eol (guaranteed by scan_atx_heading)
	bytes := firstPass.text
	if eolBytes, ok := scannersScanEol(bytes[ix:]); ok {
		firstPass.tree.nodes[headingIx].item.end = ix + eolBytes
		firstPass.tree.nodes[headingIx].item.body = parseItemBody{kind: parseItemBody_Heading, c: byte(atxLevel), n: 0}
		return ix + eolBytes
	}
	// skip leading spaces
	skipSpaces := scannersScanWhitespaceNoNl(bytes[ix:])
	ix += skipSpaces

	// now handle the header text
	headerStart := ix
	headerNodeIdx := firstPass.tree.push() // so that we can set the endpoint later

	// trim the trailing attribute block before parsing the entire line, if necessary
	var end, contentEnd int
	attrs := rs.None[parseHeadingAttributes]()
	if firstPass.options.Contains(OptionsEnableHeadingAttributes) {
		// the start of the next line is the end of the header since the
		// header cannot have line breaks
		headerEnd := headerStart + scannersScanNextline(bytes[headerStart:])
		contentEnd_, attrs_ := firstPass.extractAndParseHeadingAttributeBlock(headerStart, headerEnd)
		firstPass.parseLine(ix, rs.Some(contentEnd_), firstpassTableParseMode_Disabled)
		end, contentEnd, attrs = headerEnd, contentEnd_, attrs_
	} else {
		lineIx, lineBrk := firstPass.parseLine(ix, rs.None[int](), firstpassTableParseMode_Disabled)
		ix = lineIx
		// Backslash at end is actually hard line break
		if item, ok := lineBrk.Get(); ok && item.body.kind == parseItemBody_HardBreak && item.body.b1 {
			firstPass.tree.appendText(item.start, item.end, false)
		}
		end, contentEnd = ix, ix
	}
	firstPass.tree.nodes[headerNodeIdx].item.end = end

	// remove trailing matter from header text
	emptyTextNode := false
	if curIx := firstPass.tree.cur(); curIx != treeNone {
		// remove closing of the ATX heading
		headerText := bytes[headerStart:contentEnd]
		limit := 0
		for i := len(headerText) - 1; i >= 0; i-- {
			if b := headerText[i]; !(b == '\n' || b == '\r' || b == ' ') {
				limit = i + 1
				break
			}
		}
		closer := 0
		for i := limit - 1; i >= 0; i-- {
			if headerText[i] != '#' {
				closer = i + 1
				break
			}
		}
		if closer == 0 {
			limit = closer
		} else {
			spaces := scannersScanRevWhile(headerText[:closer], func(b byte) bool { return b == ' ' })
			if spaces > 0 {
				limit = closer - spaces
			}
		}
		// if text is only spaces, then remove them
		firstPass.tree.nodes[curIx].item.end = limit + headerStart

		// limit = 0 when text is empty after removing spaces
		if limit == 0 {
			emptyTextNode = true
		}
	}

	if emptyTextNode {
		firstPass.tree.removeNode()
	} else {
		firstPass.tree.pop()
	}
	headingAttrs := parseHeadingIndex(0)
	if attrs, ok := attrs.Get(); ok {
		headingAttrs = firstPass.allocs.allocateHeading(attrs)
	}
	firstPass.tree.nodes[headingIx].item.body = parseItemBody{
		kind: parseItemBody_Heading,
		c:    byte(atxLevel),
		n:    int(headingAttrs),
	}

	return end
}

// parseFootnote returns the number of bytes scanned on success.
func (firstPass *firstpassFirstPass) parseFootnote(start int) (int, bool) {
	bytes := firstPass.text[start:]
	if !strings.HasPrefix(bytes, "[^") {
		return 0, false
	}
	var i int
	var label string
	var ok bool
	if firstPass.options.hasGfmFootnotes() {
		// GitHub doesn't allow footnote definition labels to contain line breaks.
		// It actually does allow this for link definitions under certain circumstances,
		// but for this it's simpler to avoid it.
		i, label, ok = linklabelScanLinkLabelRest(
			firstPass.text[start+2:],
			func(string) (int, bool) {
				return 0, false
			},
			firstPass.tree.isInTable(),
		)
	} else {
		i, label, ok = firstPass.parseRefdefLabel(start + 2)
	}
	if !ok {
		return 0, false
	}
	if firstPass.options.hasGfmFootnotes() && strings.ContainsAny(label, "\r\n") {
		// GitHub doesn't allow footnote definition labels to contain line breaks,
		// even if they're escaped.
		return 0, false
	}
	i += 2
	if scannersScanCh(bytes[i:], ':') == 0 {
		return 0, false
	}
	i += 1
	firstPass.finishList(start)
	if firstPass.options.hasGfmFootnotes() {
		if nodeIx := firstPass.tree.peekUp(); nodeIx != treeNone {
			if firstPass.tree.nodes[nodeIx].item.body.kind == parseItemBody_FootnoteDefinition {
				// finish previous footnote if it's still open
				firstPass.pop(start)
			}
		}
		i += scannersScanWhitespaceNoNl(bytes[i:])
	}
	firstPass.allocs.footdefs.insert(
		label,
		parseFootnoteDef{useCount: 0},
	)
	firstPass.tree.append(parseItem{
		start: start,
		end:   0, // will get set later
		// TODO: check whether the label here is strictly necessary
		body: parseItemBody{kind: parseItemBody_FootnoteDefinition, n: int(firstPass.allocs.allocateCow(label))},
	})
	firstPass.tree.push()
	return i, true
}

// parseRefdefLabel tries to parse a reference label, which can be interrupted
// by new blocks. On success, returns the number of bytes of the label and the
// label itself.
func (firstPass *firstpassFirstPass) parseRefdefLabel(start int) (int, string, bool) {
	return linklabelScanLinkLabelRest(
		firstPass.text[start:],
		func(bytes string) (int, bool) {
			lineStart := scannersLineStartNew(bytes)
			currentContainer := parseScanContainers(
				&firstPass.tree,
				&lineStart,
				firstPass.options.hasGfmFootnotes(),
			) == firstPass.tree.spineLen()
			if lineStart.scanSpace(4) {
				return lineStart.bytesScanned(), true
			}
			bytesScanned := lineStart.bytesScanned()
			suffix := bytes[bytesScanned:]
			if firstPass.scanParagraphInterrupt(suffix, currentContainer) {
				return 0, false
			}
			if _, _, isSetext := scannersScanSetextHeading(suffix); currentContainer && isSetext {
				return 0, false
			}
			return bytesScanned, true
		},
		firstPass.tree.isInTable(),
	)
}

// parseRefdefTotal returns number of bytes scanned, label and definition on
// success.
func (firstPass *firstpassFirstPass) parseRefdefTotal(start int) (int, string, LinkDef, bool) {
	bytes := firstPass.text[start:]
	if scannersScanCh(bytes, '[') == 0 {
		return 0, "", LinkDef{}, false
	}
	i, label, ok := firstPass.parseRefdefLabel(start + 1)
	if !ok {
		return 0, "", LinkDef{}, false
	}
	i += 1
	if scannersScanCh(bytes[i:], ':') == 0 {
		return 0, "", LinkDef{}, false
	}
	i += 1
	bytecount, linkDef, ok := firstPass.scanRefdef(start, start+i)
	if !ok {
		return 0, "", LinkDef{}, false
	}
	return bytecount + i, label, linkDef, true
}

// scanRefdefSpace returns number of bytes and number of newlines
func (firstPass *firstpassFirstPass) scanRefdefSpace(bytes string, i int) (int, int, bool) {
	newlines := 0
	for {
		whitespaces := scannersScanWhitespaceNoNl(bytes[i:])
		i += whitespaces
		if eolBytes, ok := scannersScanEol(bytes[i:]); ok {
			i += eolBytes
			newlines += 1
			if newlines > 1 {
				return 0, 0, false
			}
		} else {
			break
		}
		lineStart := scannersLineStartNew(bytes[i:])
		currentContainer := parseScanContainers(
			&firstPass.tree,
			&lineStart,
			firstPass.options.hasGfmFootnotes(),
		) == firstPass.tree.spineLen()
		if !lineStart.scanSpace(4) {
			suffix := bytes[i+lineStart.bytesScanned():]
			if firstPass.scanParagraphInterrupt(suffix, currentContainer) {
				return 0, 0, false
			}
			if _, _, isSetext := scannersScanSetextHeading(suffix); isSetext {
				return 0, 0, false
			}
		}
		i += lineStart.bytesScanned()
	}
	return i, newlines, true
}

// returns (bytelength, title_str)
func (firstPass *firstpassFirstPass) scanRefdefTitle(text string) (int, string, bool) {
	bytes := text
	if len(bytes) == 0 {
		return 0, "", false
	}
	var closingDelim byte
	switch bytes[0] {
	case '\'':
		closingDelim = '\''
	case '"':
		closingDelim = '"'
	case '(':
		closingDelim = ')'
	default:
		return 0, "", false
	}
	bytecount := 1
	linestart := 1

	// linebuf is `Option<String>`: hasLinebuf tells None from an empty string.
	var linebuf strings.Builder
	hasLinebuf := false

	for bytecount < len(bytes) {
		c := bytes[bytecount]
		switch {
		case c == '(' && closingDelim == ')':
			// https://spec.commonmark.org/0.30/#link-title
			// a sequence of zero or more characters between matching parentheses ((...)),
			// including a ( or ) character only if it is backslash-escaped.
			return 0, "", false
		case c == '\n' || c == '\r':
			// push text to line buffer
			// this is used to strip the block formatting:
			//
			// > [first]: http://example.com "
			// > second"
			//
			// should get turned into `<a href="example.com" title=" second">first</a>`
			hasLinebuf = true
			linebuf.WriteString(text[linestart:bytecount])
			linebuf.WriteByte('\n') // normalize line breaks
			// skip line break
			bytecount += 1
			if c == '\r' && bytecount < len(bytes) && bytes[bytecount] == '\n' {
				bytecount += 1
			}
			lineStart := scannersLineStartNew(bytes[bytecount:])
			currentContainer := parseScanContainers(
				&firstPass.tree,
				&lineStart,
				firstPass.options.hasGfmFootnotes(),
			) == firstPass.tree.spineLen()
			if !lineStart.scanSpace(4) {
				suffix := bytes[bytecount+lineStart.bytesScanned():]
				if firstPass.scanParagraphInterrupt(suffix, currentContainer) {
					return 0, "", false
				}
				if _, _, isSetext := scannersScanSetextHeading(suffix); isSetext {
					return 0, "", false
				}
			}
			lineStart.scanAllSpace()
			bytecount += lineStart.bytesScanned()
			linestart = bytecount
			if _, blank := scannersScanBlankLine(bytes[bytecount:]); blank {
				// blank line - not allowed
				return 0, "", false
			}
		case c == '\\':
			bytecount += 1
			if bytecount < len(bytes) {
				if c := bytes[bytecount]; c != '\r' && c != '\n' {
					bytecount += 1
				}
			}
		case c == closingDelim:
			var cow string
			if hasLinebuf {
				linebuf.WriteString(text[linestart:bytecount])
				cow = linebuf.String()
			} else {
				cow = text[linestart:bytecount]
			}
			return bytecount + 1, cow, true
		default:
			bytecount += 1
		}
	}
	return 0, "", false
}

// scanRefdef returns # of bytes and definition.
// Assumes the label of the reference including colon has already been scanned.
func (firstPass *firstpassFirstPass) scanRefdef(spanStart int, start int) (int, LinkDef, bool) {
	bytes := firstPass.text

	// whitespace between label and url (including up to one newline)
	i, _, ok := firstPass.scanRefdefSpace(bytes, start)
	if !ok {
		return 0, LinkDef{}, false
	}

	// scan link dest
	destLength, dest, ok := scannersScanLinkDest(firstPass.text, i, parseLinkMaxNestedParens)
	if !ok {
		return 0, LinkDef{}, false
	}
	if destLength == 0 {
		return 0, LinkDef{}, false
	}
	dest = scannersUnescape(dest, firstPass.tree.isInTable())
	i += destLength

	// no title
	backupBytes := i - start
	backup := LinkDef{
		Dest:  dest,
		Title: rs.None[string](),
		Span:  Range{Start: spanStart, End: i},
	}

	// scan whitespace between dest and label
	newI, newlines, ok := firstPass.scanRefdefSpace(bytes, i)
	if !ok {
		return backupBytes, backup, true
	}
	if i == len(firstPass.text) {
		newlines += 1
	}
	if newI == i && newlines == 0 {
		return 0, LinkDef{}, false
	}
	if newlines > 1 {
		return backupBytes, backup, true
	}
	i = newI

	// scan title
	// if this fails but newline == 1, return also a refdef without title
	if titleLength, title, ok := firstPass.scanRefdefTitle(firstPass.text[i:]); ok {
		i += titleLength
		if _, blank := scannersScanBlankLine(bytes[i:]); blank {
			backupBytes = i - start
			backup.Span = Range{Start: spanStart, End: i}
			backup.Title = rs.Some(scannersUnescape(title, firstPass.tree.isInTable()))
			return backupBytes, backup, true
		}
	}
	if newlines > 0 {
		return backupBytes, backup, true
	} else {
		return 0, LinkDef{}, false
	}
}

// scanParagraphInterrupt checks whether we should break a paragraph on the
// given input.
func (firstPass *firstpassFirstPass) scanParagraphInterrupt(bytes string, currentContainer bool) bool {
	gfmFootnote := firstPass.options.hasGfmFootnotes()
	if firstpassScanParagraphInterruptNoTable(bytes, currentContainer, gfmFootnote, &firstPass.tree) {
		return true
	}
	// pulldown-cmark allows heavy tables, that have a `|` on the header row,
	// to interrupt paragraphs.
	//
	// ```markdown
	// This is a table
	// | a | b | c |
	// |---|---|---|
	// | d | e | f |
	//
	// This is not a table
	//  a | b | c
	// ---|---|---
	//  d | e | f
	// ```
	if !firstPass.options.Contains(OptionsEnableTables) || !strings.HasPrefix(bytes, "|") {
		return false
	}

	// Checking if something's a valid table or not requires looking at two lines.
	// First line, count unescaped pipes.
	pipes := 0
	nextLineIx := 0
	bsesc := false
	lastPipeIx := 0
loop:
	for i := 0; i < len(bytes); i++ {
		byte_ := bytes[i]
		switch {
		case byte_ == '\\':
			bsesc = true
			continue
		case byte_ == '|' && !bsesc:
			pipes += 1
			lastPipeIx = i
		case byte_ == '\r' || byte_ == '\n':
			eol, ok := scannersScanEol(bytes[i:])
			if !ok {
				rs.PanicAt(0, optionUnwrapNone)
			}
			nextLineIx = i + eol
			break loop
		default:
		}
		bsesc = false
	}

	// scan_eol can't return 0, so this can't be zero
	if nextLineIx == 0 {
		return false
	}

	// Scan the table head. The part that looks like:
	//
	//     |---|---|---|
	//
	// Also scan any containing items, since it's on its own line, and
	// might be nested inside a block quote or something
	//
	//     > Table: First
	//     > | first col | second col |
	//     > |-----------|------------|
	//     ^
	//     | need to skip over the `>` when checking for the table
	lineStart := scannersLineStartNew(bytes[nextLineIx:])
	if parseScanContainers(
		&firstPass.tree,
		&lineStart,
		firstPass.options.hasGfmFootnotes(),
	) != firstPass.tree.spineLen() {
		return false
	}
	tableHeadIx := nextLineIx + lineStart.bytesScanned()
	tableHeadBytes, alignment := scannersScanTableHead(bytes[tableHeadIx:])

	if tableHeadBytes == 0 {
		return false
	}

	// computing header count from number of pipes
	headerCount := firstpassCountHeaderCols(bytes, pipes, 0, lastPipeIx)

	// make sure they match the number of columns we find in separator line
	return len(alignment) == headerCount
}

// extractAndParseHeadingAttributeBlock extracts and parses a heading
// attribute block if exists.
//
// Returns `(end_offset_of_heading_content, (id, classes))`.
//
// If `header_end` is less than or equal to `header_start`, the given
// input is considered as empty.
func (firstPass *firstpassFirstPass) extractAndParseHeadingAttributeBlock(
	headerStart int,
	headerEnd int,
) (int, rs.Option[parseHeadingAttributes]) {
	if !firstPass.options.Contains(OptionsEnableHeadingAttributes) {
		return headerEnd, rs.None[parseHeadingAttributes]()
	}

	// extract the trailing attribute block
	headerBytes := firstPass.text[headerStart:headerEnd]
	contentLen, attrBlockRangeRel, hasAttrBlock := firstpassExtractAttributeBlockContentFromHeaderText(headerBytes)
	contentEnd := headerStart + contentLen
	attrs := rs.None[parseHeadingAttributes]()
	if hasAttrBlock {
		r := attrBlockRangeRel
		attrs = firstpassParseInsideAttributeBlock(
			firstPass.text[(headerStart + r.Start):(headerStart + r.End)],
		)
	}
	return contentEnd, attrs
}

// firstpassTableParseMode is the scanning modes for `Parser`'s `parse_line`
// method.
type firstpassTableParseMode uint8

const (
	// Inside a paragraph, scanning for table headers.
	firstpassTableParseMode_Scan firstpassTableParseMode = iota
	// Inside a table.
	firstpassTableParseMode_Active
	// Inside a paragraph, not scanning for table headers.
	firstpassTableParseMode_Disabled
)

// firstpassCountHeaderCols computes the number of header columns in a table
// line by computing the number of dividing pipes that aren't followed or
// preceded by whitespace.
func firstpassCountHeaderCols(
	bytes string,
	pipes int,
	start int,
	lastPipeIx int,
) int {
	// was first pipe preceded by whitespace? if so, subtract one
	start += scannersScanWhitespaceNoNl(bytes[start:])
	if bytes[start] == '|' {
		pipes -= 1
	}

	// was last pipe followed by whitespace? if so, sub one
	if _, blank := scannersScanBlankLine(bytes[(lastPipeIx + 1):]); blank {
		return pipes
	} else {
		return pipes + 1
	}
}

// firstpassScanParagraphInterruptNoTable checks whether we should break a
// paragraph on the given input.
//
// Use `FirstPass::scan_paragraph_interrupt` in any context that allows
// tables to interrupt the paragraph.
func firstpassScanParagraphInterruptNoTable(
	bytes string,
	currentContainer bool,
	gfmFootnote bool,
	tree *treeTree,
) bool {
	if _, ok := scannersScanEol(bytes); ok {
		return true
	}
	if _, ok := scannersScanHrule(bytes); ok {
		return true
	}
	if _, ok := scannersScanAtxHeading(bytes); ok {
		return true
	}
	if _, _, ok := scannersScanCodeFence(bytes); ok {
		return true
	}
	if _, ok := scannersScanBlockquoteStart(bytes); ok {
		return true
	}
	if ix, delim, index, _, ok := scannersScanListitem(bytes); ok {
		if !currentContainer ||
			tree.isInTable() ||
			// we don't allow interruption by either empty lists or
			// numbered lists starting at an index other than 1
			(delim == '*' || delim == '-' || delim == '+' || index == 1) &&
				!scannersIsBlankLine(bytes[ix:]) {
			return true
		}
	}
	if strings.HasPrefix(bytes, "<") {
		if _, ok := firstpassGetHtmlEndTag(bytes[1:]); ok || scannersStartsHtmlBlockType6(bytes[1:]) {
			return true
		}
	}
	if gfmFootnote && strings.HasPrefix(bytes, "[^") {
		if length, _, ok := linklabelScanLinkLabelRest(
			bytes[2:],
			func(string) (int, bool) { return 0, false },
			tree.isInTable(),
		); ok {
			return 2+length < len(bytes) && bytes[2+length] == ':'
		}
	}
	return false
}

// scannersIsBlankLine is `scan_blank_line(bytes).is_some()`.
func scannersIsBlankLine(bytes string) bool {
	_, ok := scannersScanBlankLine(bytes)
	return ok
}

// firstpassGetHtmlEndTag assumes `text_bytes` is preceded by `<`.
func firstpassGetHtmlEndTag(textBytes string) (string, bool) {
	beginTags := [4]string{"pre", "style", "script", "textarea"}
	stBeginTags := [3]string{"!--", "?", "![CDATA["}

	for i, endTag := range [4]string{"</pre>", "</style>", "</script>", "</textarea>"} {
		begTag := beginTags[i]
		tagLen := len(begTag)

		if len(textBytes) < tagLen {
			// begin tags are increasing in size
			break
		}

		if !strs.EqIgnoreAsciiCase(textBytes[:tagLen], begTag) {
			continue
		}

		// Must either be the end of the line...
		if len(textBytes) == tagLen {
			return endTag, true
		}

		// ...or be followed by whitespace, newline, or '>'.
		s := textBytes[tagLen]
		if scannersIsAsciiWhitespace(s) || s == '>' {
			return endTag, true
		}
	}

	for i, endTag := range [3]string{"-->", "?>", "]]>"} {
		begTag := stBeginTags[i]
		if strings.HasPrefix(textBytes, begTag) {
			return endTag, true
		}
	}

	if len(textBytes) > 1 && textBytes[0] == '!' && strs.IsAsciiAlphabetic(textBytes[1]) {
		return ">", true
	} else {
		return "", false
	}
}

// https://english.stackexchange.com/a/285573
func firstpassSurgerizeTightList(tree *treeTree, listIx treeTreeIndex) {
	listItem := tree.nodes[listIx].child
	for listItem != treeNone {
		listitemIx := listItem
		// first child is special, controls how we repoint list_item.child
		listItemFirstborn := tree.nodes[listitemIx].child

		// Check that list item has children - this is not necessarily the case!
		if firstbornIx := listItemFirstborn; firstbornIx != treeNone {
			if tree.nodes[firstbornIx].item.body.kind == parseItemBody_Paragraph {
				tree.nodes[listitemIx].child = tree.nodes[firstbornIx].child
			}

			listItemChild := firstbornIx
			nodeToRepoint := treeNone
			for listItemChild != treeNone {
				childIx := listItemChild
				// surgerize paragraphs
				var repointIx treeTreeIndex
				if tree.nodes[childIx].item.body.kind == parseItemBody_Paragraph {
					if childFirstborn := tree.nodes[childIx].child; childFirstborn != treeNone {
						if repointIx := nodeToRepoint; repointIx != treeNone {
							tree.nodes[repointIx].next = childFirstborn
						}
						childLastborn := childFirstborn
						for {
							lastbornNextIx := tree.nodes[childLastborn].next
							if lastbornNextIx == treeNone {
								break
							}
							childLastborn = lastbornNextIx
						}
						repointIx = childLastborn
					} else {
						repointIx = childIx
					}
				} else {
					repointIx = childIx
				}

				nodeToRepoint = repointIx
				tree.nodes[repointIx].next = tree.nodes[childIx].next
				listItemChild = tree.nodes[childIx].next
			}
		}

		listItem = tree.nodes[listitemIx].next
	}
}

// firstpassDelimRunCanOpen determines whether the delimiter run starting at
// given index is left-flanking, as defined by the commonmark spec (and isn't
// intraword for _ delims).
// suffix is &s[ix..], which is passed in as an optimization, since taking
// a string subslice is O(n).
func firstpassDelimRunCanOpen(
	s string,
	suffix string,
	runLen int,
	ix int,
	mode firstpassTableParseMode,
) bool {
	nextChar, ok := strCharsNth(suffix, runLen)
	if !ok {
		return false
	}
	if strs.IsWhitespace(nextChar) {
		return false
	}
	if ix == 0 {
		return true
	}
	if mode == firstpassTableParseMode_Active {
		if strings.HasSuffix(s[:ix], "|") && !strings.HasSuffix(s[:ix], `\|`) {
			return true
		}
		if nextChar == '|' {
			return false
		}
	}
	delim := rune(suffix[0])
	// `*` and `~~` can be intraword, `_` and `~` cannot
	if delim == '*' && !puncttableIsPunctuation(nextChar) {
		return true
	}
	if delim == '~' && runLen > 1 {
		return true
	}
	prevChar, ok := strCharsLast(s[:ix])
	if !ok {
		rs.PanicAt(0, optionUnwrapNone)
	}
	if delim == '~' && prevChar == '~' && !puncttableIsPunctuation(nextChar) {
		return true
	}

	return strs.IsWhitespace(prevChar) ||
		puncttableIsPunctuation(prevChar) && (delim != '\'' || !(prevChar == ']' || prevChar == ')'))
}

// firstpassDelimRunCanClose determines whether the delimiter run starting at
// given index is right-flanking, as defined by the commonmark spec (and isn't
// intraword for _ delims)
func firstpassDelimRunCanClose(
	s string,
	suffix string,
	runLen int,
	ix int,
	mode firstpassTableParseMode,
) bool {
	if ix == 0 {
		return false
	}
	prevChar, ok := strCharsLast(s[:ix])
	if !ok {
		rs.PanicAt(0, optionUnwrapNone)
	}
	if strs.IsWhitespace(prevChar) {
		return false
	}
	nextChar, ok := strCharsNth(suffix, runLen)
	if !ok {
		return true
	}
	if mode == firstpassTableParseMode_Active {
		if strings.HasSuffix(s[:ix], "|") && !strings.HasSuffix(s[:ix], `\|`) {
			return false
		}
		if nextChar == '|' {
			return true
		}
	}
	delim := rune(suffix[0])
	// `*` and `~~` can be intraword, `_` and `~` cannot
	if (delim == '*' || (delim == '~' && runLen > 1)) && !puncttableIsPunctuation(prevChar) {
		return true
	}
	if delim == '~' && prevChar == '~' {
		return true
	}

	return strs.IsWhitespace(nextChar) || puncttableIsPunctuation(nextChar)
}

func firstpassCreateLut(options *Options) firstpassLookupTable {
	return firstpassSpecialBytes(options)
}

func firstpassSpecialBytes(options *Options) [256]bool {
	var bytes [256]bool
	standardBytes := [...]byte{
		'\n', '\r', '*', '_', '&', '\\', '[', ']', '<', '!', '`',
	}

	for _, byte_ := range standardBytes {
		bytes[byte_] = true
	}
	if options.Contains(OptionsEnableTables) {
		bytes['|'] = true
	}
	if options.Contains(OptionsEnableStrikethrough) {
		bytes['~'] = true
	}
	if options.Contains(OptionsEnableSmartPunctuation) {
		for _, byte_ := range [...]byte{'.', '-', '"', '\''} {
			bytes[byte_] = true
		}
	}

	return bytes
}

// firstpassLoopInstruction is the crate's `LoopInstruction<Option<Item>>`:
// either ContinueAndSkip(n) or BreakAtWith(n, val).
type firstpassLoopInstruction struct {
	isBreak bool
	n       int
	val     rs.Option[parseItem]
}

// firstpassContinueAndSkip: continue looking for more special bytes, but skip
// next few bytes.
func firstpassContinueAndSkip(skip int) firstpassLoopInstruction {
	return firstpassLoopInstruction{isBreak: false, n: skip}
}

// firstpassBreakAtWith: break looping immediately, returning with the given
// index and value.
func firstpassBreakAtWith(ix int, val rs.Option[parseItem]) firstpassLoopInstruction {
	return firstpassLoopInstruction{isBreak: true, n: ix, val: val}
}

// firstpassLookupTable is the scalar lookup table; the crate's SIMD variant
// (feature "simd") is not part of the build Mjolnir uses.
type firstpassLookupTable = [256]bool

// firstpassIterateSpecialBytes walks the byte slices from the given index and
// calls the callback function on all bytes (and their indices) that are in the following set:
// “ ` “, `\`, `&`, `*`, `_`, `~`, `!`, `<`, `[`, `]`, `|`, `\r`, `\n`
// It is guaranteed not call the callback on other bytes.
// Whenever `callback(ix, byte)` returns a `ContinueAndSkip(n)` value, the callback
// will not be called with an index that is less than `ix + n + 1`.
// When the callback returns a `BreakAtWith(end_ix, opt+val)`, no more callbacks will be
// called and the function returns immediately with the return value `(end_ix, opt_val)`.
// If `BreakAtWith(..)` is never returned, this function will return the first
// index that is outside the byteslice bound and a `None` value.
func firstpassIterateSpecialBytes(
	lut *firstpassLookupTable,
	bytes string,
	ix int,
	callback func(int, byte) firstpassLoopInstruction,
) (int, rs.Option[parseItem]) {
	return firstpassScalarIterateSpecialBytes(lut, bytes, ix, callback)
}

func firstpassScalarIterateSpecialBytes(
	lut *[256]bool,
	bytes string,
	ix int,
	callback func(int, byte) firstpassLoopInstruction,
) (int, rs.Option[parseItem]) {
	for ix < len(bytes) {
		b := bytes[ix]
		if lut[b] {
			instruction := callback(ix, b)
			if instruction.isBreak {
				return instruction.n, instruction.val
			}
			ix += instruction.n
		}
		ix += 1
	}

	return ix, rs.None[parseItem]()
}

// firstpassExtractAttributeBlockContentFromHeaderText splits the usual
// heading content range and the content inside the trailing attribute block.
//
// Returns `(leading_content_len, Option<trailing_attr_block_range>)`.
//
// Note that `trailing_attr_block_range` will be empty range when the block
// is `{}`, since the range is content inside the wrapping `{` and `}`.
//
// The closing `}` of an attribute block can have trailing whitespaces.
// They are automatically trimmed when the attribute block is being searched.
//
// However, this method does not trim the trailing whitespaces of heading content.
// It is callers' responsibility to trim them if necessary.
func firstpassExtractAttributeBlockContentFromHeaderText(
	heading string,
) (int, Range, bool) {
	headingLen := len(heading)
	ix := headingLen
	ix -= scannersScanRevWhile(heading, func(b byte) bool {
		return b == '\n' || b == '\r' || b == ' ' || b == '\t'
	})
	if ix == 0 {
		return headingLen, Range{}, false
	}

	attrBlockClose := ix - 1
	if heading[attrBlockClose] != '}' {
		// The last character is not `}`. No attribute blocks found.
		return headingLen, Range{}, false
	}
	// move cursor before the closing right brace (`}`)
	ix -= 1

	ix -= scannersScanRevWhile(heading[:ix], func(b byte) bool {
		// Characters to be excluded:
		//  * `{` and `}`: special characters to open and close an attribute block.
		//  * `\\`: a special character to escape many characters and disable some syntaxes.
		//      + Handling of this escape character differs among markdown processors.
		//      + Escaped characters will be separate text node from neighbors, so
		//        it is not easy to handle unescaped string and trim the trailing block.
		//  * `<` and `>`: special characters to start and end HTML tag.
		//      + No known processors converts `{#<i>foo</i>}` into
		//        `id="&lt;i&gt;foo&lt;/&gt;"` as of this writing, so hopefully
		//        this restriction won't cause compatibility issues.
		//  * `\n` and `\r`: a newline character.
		//      + Setext heading can have multiple lines. However it is hard to support
		//        attribute blocks that have newline inside, since the parsing proceeds line by
		//        line and lines will be separate nodes even they are logically a single text.
		switch b {
		case '{', '}', '<', '>', '\\', '\n', '\r':
			return false
		default:
			return true
		}
	})
	if ix == 0 {
		// `{` is not found. No attribute blocks available.
		return headingLen, Range{}, false
	}
	attrBlockOpen := ix - 1
	if heading[attrBlockOpen] != '{' {
		// `{` is not found. No attribute blocks available.
		return headingLen, Range{}, false
	}

	return attrBlockOpen, Range{Start: ix, End: attrBlockClose}, true
}

// firstpassParseInsideAttributeBlock parses an attribute block content, such
// as `.class1 #id .class2`.
//
// Returns `(id, classes)`.
//
// It is callers' responsibility to find opening and closing characters of the attribute
// block. Usually [`extract_attribute_block_content_from_header_text`] function does it for you.
//
// Note that this parsing requires explicit whitespace separators between
// attributes. This is intentional design with the reasons below:
//
//   - to keep conversion simple and easy to understand for any possible input,
//   - to avoid adding less obvious conversion rule that can reduce compatibility
//     with other implementations more, and
//   - to follow the major design of implementations with the support for the
//     attribute blocks extension (as of this writing).
//
// See also: OptionsEnableHeadingAttributes.
func firstpassParseInsideAttributeBlock(insideAttrBlock string) rs.Option[parseHeadingAttributes] {
	id := rs.None[string]()
	classes := []string{}
	attrs := []rs.Tuple2[string, rs.Option[string]]{}

	for _, attr := range strs.SplitAsciiWhitespace(insideAttrBlock) {
		// iterator returned by `str::split_ascii_whitespace` never emits empty
		// strings, so taking first byte won't panic.
		if len(attr) > 1 {
			firstByte := attr[0]
			if firstByte == '#' {
				id = rs.Some(attr[1:])
			} else if firstByte == '.' {
				classes = append(classes, attr[1:])
			} else {
				if key, value, ok := strings.Cut(attr, "="); ok {
					attrs = append(attrs, rs.T2(key, rs.Some(value)))
				} else {
					attrs = append(attrs, rs.T2(attr, rs.None[string]()))
				}
			}
		}
	}

	return rs.Some(parseHeadingAttributes{id: id, classes: classes, attrs: attrs})
}

// usizeRange is the number of iterations of `for _ in from..to`: the bounds
// are read once, before the loop, and an empty range runs no iteration.
func usizeRange(from int, to int) int {
	if to <= from {
		return 0
	}
	return to - from
}
