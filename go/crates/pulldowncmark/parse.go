// Port of pulldown-cmark 0.10.3 src/parse.rs

// Copyright 2017 Google Inc. All rights reserved.
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
	"iter"
	"strconv"
	"strings"

	"github.com/BrokkAi/mjolnir/go/rs"
)

// Tree-based two pass parser.

// Allowing arbitrary depth nested parentheses inside link destinations
// can create denial of service vulnerabilities if we're not careful.
// The simplest countermeasure is to limit their depth, which is
// explicitly allowed by the spec as long as the limit is at least 3:
// https://spec.commonmark.org/0.29/#link-destination
const parseLinkMaxNestedParens = 5

type parseItem struct {
	start int
	end   int
	body  parseItemBody
}

// parseItemDefault is `Item::default()`.
func parseItemDefault() parseItem {
	return parseItem{start: 0, end: 0, body: parseItemBodyDefault()}
}

// parseItemBodyKind is the variant of an ItemBody.
type parseItemBodyKind uint8

const (
	parseItemBody_Paragraph parseItemBodyKind = iota
	parseItemBody_Text                        // b1: backslash_escaped
	parseItemBody_SoftBreak
	// true = is backlash
	parseItemBody_HardBreak // b1

	// These are possible inline items, need to be resolved in second pass.

	// repeats, can_open, can_close
	parseItemBody_MaybeEmphasis // n, b1, b2
	// quote byte, can_open, can_close
	parseItemBody_MaybeSmartQuote // c, b1, b2
	parseItemBody_MaybeCode       // n: number of backticks, b1: preceded by backslash
	parseItemBody_MaybeHtml
	parseItemBody_MaybeLinkOpen
	// bool indicates whether or not the preceding section could be a reference
	parseItemBody_MaybeLinkClose // b1
	parseItemBody_MaybeImage

	// These are inline items after resolution.
	parseItemBody_Emphasis
	parseItemBody_Strong
	parseItemBody_Strikethrough
	parseItemBody_Code              // n: CowIndex
	parseItemBody_Link              // n: LinkIndex
	parseItemBody_Image             // n: LinkIndex
	parseItemBody_FootnoteReference // n: CowIndex
	parseItemBody_TaskListMarker    // b1: true for checked

	parseItemBody_Rule
	parseItemBody_Heading         // c: heading level, n: Option<HeadingIndex> (0 is None)
	parseItemBody_FencedCodeBlock // n: CowIndex
	parseItemBody_IndentCodeBlock
	parseItemBody_HtmlBlock
	parseItemBody_InlineHtml
	parseItemBody_Html
	parseItemBody_OwnedHtml // n: CowIndex
	parseItemBody_BlockQuote
	parseItemBody_List               // b1: is_tight, c: list character, u: list start index
	parseItemBody_ListItem           // n: indent level
	parseItemBody_SynthesizeText     // n: CowIndex
	parseItemBody_SynthesizeChar     // n: char
	parseItemBody_FootnoteDefinition // n: CowIndex
	parseItemBody_MetadataBlock      // c: MetadataBlockKind

	// Tables
	parseItemBody_Table // n: AlignmentIndex
	parseItemBody_TableHead
	parseItemBody_TableRow
	parseItemBody_TableCell

	// Dummy node at the top of the tree - should not be used otherwise!
	parseItemBody_Root
)

// parseItemBody is the crate's ItemBody enum. It is a `Copy` value that the
// parser stores in every tree node and changes in place, so it is held as
// one flat struct instead of the interface the port uses for public enums:
// kind names the variant, and the variant's fields are in b1, b2, c, n and
// u as noted beside each kind above.
type parseItemBody struct {
	kind parseItemBodyKind
	b1   bool
	b2   bool
	c    byte
	n    int
	u    uint64
}

// parseItemBodyDefault is `ItemBody::default()`: the `#[default]` variant Root.
func parseItemBodyDefault() parseItemBody {
	return parseItemBody{kind: parseItemBody_Root}
}

func parseItemBodyText(backslashEscaped bool) parseItemBody {
	return parseItemBody{kind: parseItemBody_Text, b1: backslashEscaped}
}

func parseItemBodySynthesizeChar(c rune) parseItemBody {
	return parseItemBody{kind: parseItemBody_SynthesizeChar, n: int(c)}
}

func (body *parseItemBody) isInline() bool {
	switch body.kind {
	case parseItemBody_MaybeEmphasis,
		parseItemBody_MaybeSmartQuote,
		parseItemBody_MaybeHtml,
		parseItemBody_MaybeCode,
		parseItemBody_MaybeLinkOpen,
		parseItemBody_MaybeLinkClose,
		parseItemBody_MaybeImage:
		return true
	default:
		return false
	}
}

func (body *parseItemBody) isBlock() bool {
	switch body.kind {
	case parseItemBody_Paragraph,
		parseItemBody_BlockQuote,
		parseItemBody_List,
		parseItemBody_ListItem,
		parseItemBody_HtmlBlock,
		parseItemBody_Table,
		parseItemBody_TableHead,
		parseItemBody_TableRow,
		parseItemBody_TableCell,
		parseItemBody_Heading,
		parseItemBody_Rule:
		return true
	default:
		return false
	}
}

// debugString is the derived Debug form of an ItemBody, which the panics for
// an unexpected body quote.
func (body *parseItemBody) debugString() string {
	boolean := strconv.FormatBool
	index := func(name string) string {
		return name + "(" + strconv.Itoa(body.n) + ")"
	}
	switch body.kind {
	case parseItemBody_Paragraph:
		return "Paragraph"
	case parseItemBody_Text:
		return "Text { backslash_escaped: " + boolean(body.b1) + " }"
	case parseItemBody_SoftBreak:
		return "SoftBreak"
	case parseItemBody_HardBreak:
		return "HardBreak(" + boolean(body.b1) + ")"
	case parseItemBody_MaybeEmphasis:
		return "MaybeEmphasis(" + strconv.Itoa(body.n) + ", " + boolean(body.b1) + ", " + boolean(body.b2) + ")"
	case parseItemBody_MaybeSmartQuote:
		return "MaybeSmartQuote(" + strconv.Itoa(int(body.c)) + ", " + boolean(body.b1) + ", " + boolean(body.b2) + ")"
	case parseItemBody_MaybeCode:
		return "MaybeCode(" + strconv.Itoa(body.n) + ", " + boolean(body.b1) + ")"
	case parseItemBody_MaybeHtml:
		return "MaybeHtml"
	case parseItemBody_MaybeLinkOpen:
		return "MaybeLinkOpen"
	case parseItemBody_MaybeLinkClose:
		return "MaybeLinkClose(" + boolean(body.b1) + ")"
	case parseItemBody_MaybeImage:
		return "MaybeImage"
	case parseItemBody_Emphasis:
		return "Emphasis"
	case parseItemBody_Strong:
		return "Strong"
	case parseItemBody_Strikethrough:
		return "Strikethrough"
	case parseItemBody_Code:
		return "Code(" + index("CowIndex") + ")"
	case parseItemBody_Link:
		return "Link(" + index("LinkIndex") + ")"
	case parseItemBody_Image:
		return "Image(" + index("LinkIndex") + ")"
	case parseItemBody_FootnoteReference:
		return "FootnoteReference(" + index("CowIndex") + ")"
	case parseItemBody_TaskListMarker:
		return "TaskListMarker(" + boolean(body.b1) + ")"
	case parseItemBody_Rule:
		return "Rule"
	case parseItemBody_Heading:
		heading := "None"
		if body.n != 0 {
			heading = "Some(" + index("HeadingIndex") + ")"
		}
		return "Heading(H" + strconv.Itoa(int(body.c)) + ", " + heading + ")"
	case parseItemBody_FencedCodeBlock:
		return "FencedCodeBlock(" + index("CowIndex") + ")"
	case parseItemBody_IndentCodeBlock:
		return "IndentCodeBlock"
	case parseItemBody_HtmlBlock:
		return "HtmlBlock"
	case parseItemBody_InlineHtml:
		return "InlineHtml"
	case parseItemBody_Html:
		return "Html"
	case parseItemBody_OwnedHtml:
		return "OwnedHtml(" + index("CowIndex") + ")"
	case parseItemBody_BlockQuote:
		return "BlockQuote"
	case parseItemBody_List:
		return "List(" + boolean(body.b1) + ", " + strconv.Itoa(int(body.c)) + ", " + strconv.FormatUint(body.u, 10) + ")"
	case parseItemBody_ListItem:
		return "ListItem(" + strconv.Itoa(body.n) + ")"
	case parseItemBody_SynthesizeText:
		return "SynthesizeText(" + index("CowIndex") + ")"
	case parseItemBody_SynthesizeChar:
		// The parser synthesizes only printable characters, of which the
		// backslash and the single quote are the two `{:?}` escapes.
		c := string(rune(body.n))
		if c == "\\" || c == "'" {
			c = "\\" + c
		}
		return "SynthesizeChar('" + c + "')"
	case parseItemBody_FootnoteDefinition:
		return "FootnoteDefinition(" + index("CowIndex") + ")"
	case parseItemBody_MetadataBlock:
		kind := "YamlStyle"
		if MetadataBlockKind(body.c) == MetadataBlockKind_PlusesStyle {
			kind = "PlusesStyle"
		}
		return "MetadataBlock(" + kind + ")"
	case parseItemBody_Table:
		return "Table(" + index("AlignmentIndex") + ")"
	case parseItemBody_TableHead:
		return "TableHead"
	case parseItemBody_TableRow:
		return "TableRow"
	case parseItemBody_TableCell:
		return "TableCell"
	case parseItemBody_Root:
		return "Root"
	default:
		panic(rs.Unreachable())
	}
}

// BrokenLink is a link whose reference has no definition, as it is passed to
// the broken link callback.
type BrokenLink struct {
	Span      Range    `rs:"span"`
	LinkType  LinkType `rs:"link_type"`
	Reference string   `rs:"reference"`
}

var _ = rs.RegisterStruct[BrokenLink]("BrokenLink")

// Parser is the Markdown event iterator.
//
// Rust's Parser is generic over the type of its broken link callback; here
// the callback is held as the BrokenLinkCallback interface.
type Parser struct {
	text               string
	options            Options
	tree               treeTree
	allocs             parseAllocations
	brokenLinkCallback rs.Option[BrokenLinkCallback]
	htmlScanGuard      parseHtmlScanGuard

	// https://github.com/pulldown-cmark/pulldown-cmark/issues/844
	// Consider this example:
	//
	//     [x]: xxx...
	//     [x]
	//     [x]
	//     [x]
	//
	// Which expands to this HTML:
	//
	//     <a href="xxx...">x</a>
	//     <a href="xxx...">x</a>
	//     <a href="xxx...">x</a>
	//
	// This is quadratic growth, because it's filling in the area of a square.
	// To prevent this, track how much it's expanded and limit it.
	linkRefExpansionLimit int

	// used by inline passes. store them here for reuse
	inlineStack parseInlineStack
	linkStack   parseLinkStack
}

// parseRangeFull is `..` (core::ops::RangeFull), which Parser's Debug impl
// prints in place of the callback.
type parseRangeFull struct{}

func (parseRangeFull) FmtDebug(f *rs.Formatter) error { return f.WriteStr("..") }

// FmtDebug is `impl Debug for Parser`.
func (parser *Parser) FmtDebug(f *rs.Formatter) error {
	// Only print the fields that have public types.
	return f.DebugStruct("Parser").
		Field("text", parser.text).
		Field("options", parser.options).
		Field(
			"broken_link_callback",
			rs.OptionMap(parser.brokenLinkCallback, func(BrokenLinkCallback) parseRangeFull { return parseRangeFull{} }),
		).
		Finish()
}

// IntoStatic moves the link into version with a static lifetime. A Go string
// has no lifetime to shed, so this is a copy of the link.
func (link *BrokenLink) IntoStatic() BrokenLink {
	return BrokenLink{
		Span:      link.Span,
		LinkType:  link.LinkType,
		Reference: link.Reference,
	}
}

// ParserNew creates a new event iterator for a markdown string without any
// options enabled.
func ParserNew(text string) *Parser {
	return ParserNewExt(text, OptionsEmpty())
}

// ParserNewExt creates a new event iterator for a markdown string with given
// options.
func ParserNewExt(text string, options Options) *Parser {
	return ParserNewWithBrokenLinkCallback(text, options, rs.None[BrokenLinkCallback]())
}

// ParserNewWithBrokenLinkCallback creates a parser with a callback for broken
// references. In case the parser encounters any potential links that have a
// broken reference (e.g `[foo]` when there is no `[foo]: ` entry at the
// bottom) the provided callback will be called with the reference name, and
// the returned pair will be used as the link URL and title if it is not
// `None`.
func ParserNewWithBrokenLinkCallback(
	text string,
	options Options,
	brokenLinkCallback rs.Option[BrokenLinkCallback],
) *Parser {
	tree, allocs := firstpassRunFirstPass(text, options)
	tree.reset()
	return &Parser{
		text:               text,
		options:            options,
		tree:               tree,
		allocs:             allocs,
		brokenLinkCallback: brokenLinkCallback,
		inlineStack:        parseInlineStack{},
		linkStack:          parseLinkStack{},
		htmlScanGuard:      parseHtmlScanGuard{},
		// always allow 100KiB
		linkRefExpansionLimit: max(len(text), 100_000),
	}
}

// ReferenceDefinitions returns a reference to the internal `RefDefs` object,
// which provides access to the internal map of reference definitions.
func (parser *Parser) ReferenceDefinitions() *RefDefs {
	return &parser.allocs.refdefs
}

// fetchLinkTypeUrlTitle uses a link label to fetch a type, url, and title.
//
// This function enforces the [`link_ref_expansion_limit`].
// If it returns Some, it also consumes some of the fuel.
// If we're out of fuel, it immediately returns None.
//
// The URL and title are found in the [`RefDefs`] map.
// If they're not there, and a callback was provided by the user,
// the [`broken_link_callback`] will be invoked and given the opportunity
// to provide a fallback.
//
// The link type (that's "link" or "image") depends on the usage site, and
// is provided by the caller of this function.
// This function returns a new one because, if it has to invoke a callback
// to find the information, the link type is [mapped to an unknown type].
func (parser *Parser) fetchLinkTypeUrlTitle(
	linkLabel string,
	span Range,
	linkType LinkType,
) (LinkType, string, string, bool) {
	if parser.linkRefExpansionLimit <= 0 {
		return 0, "", "", false
	}

	var url, title string
	if matchingDef, ok := parser.allocs.refdefs.Get(linkLabel).Get(); ok {
		// found a matching definition!
		title = matchingDef.Title.UnwrapOr("")
		url = matchingDef.Dest
	} else {
		callback, ok := parser.brokenLinkCallback.Get()
		if !ok {
			return 0, "", "", false
		}
		// Construct a BrokenLink struct, which will be passed to the callback
		brokenLink := BrokenLink{
			Span:      span,
			LinkType:  linkType,
			Reference: linkLabel,
		}

		pair, ok := callback.HandleBrokenLink(brokenLink).Get()
		if !ok {
			return 0, "", "", false
		}
		linkType, url, title = linkType.toUnknown(), pair.F0, pair.F1
	}

	// Limit expansion from link references.
	// This isn't a problem for footnotes, because multiple references to the same one
	// reuse the same node, but links/images get their HREF/SRC copied.
	parser.linkRefExpansionLimit = usizeSaturatingSub(parser.linkRefExpansionLimit, len(url)+len(title))

	return linkType, url, title, true
}

// handleInline handles inline markup.
//
// When the parser encounters any item indicating potential inline markup, all
// inline markup passes are run on the remainder of the chain.
//
// Note: there's some potential for optimization here, but that's future work.
func (parser *Parser) handleInline() {
	parser.handleInlinePass1()
	parser.handleEmphasisAndHardBreak()
}

// handleInlinePass1 handles inline HTML, code spans, and links.
//
// This function handles both inline HTML and code spans, because they have
// the same precedence. It also handles links, even though they have lower
// precedence, because the URL of links must not be processed.
func (parser *Parser) handleInlinePass1() {
	codeDelims := parseCodeDelimsNew()
	cur := parser.tree.cur()
	prev := treeNone

	blockEnd := parser.tree.nodes[parser.tree.peekUp().unwrap()].item.end
	blockText := parser.text[:blockEnd]

	for cur != treeNone {
		curIx := cur
		switch body := parser.tree.nodes[curIx].item.body; body.kind {
		case parseItemBody_MaybeHtml:
			next := parser.tree.nodes[curIx].next
			var autolinkIx int
			var autolinkUri string
			var autolinkType LinkType
			autolink := false
			if nextIx := next; nextIx != treeNone {
				autolinkIx, autolinkUri, autolinkType, autolink = scannersScanAutolink(blockText, parser.tree.nodes[nextIx].item.start)
			}

			if autolink {
				ix, uri, linkType := autolinkIx, autolinkUri, autolinkType
				node := parseScanNodesToIx(&parser.tree, next, ix)
				textNode := parser.tree.createNode(parseItem{
					start: parser.tree.nodes[curIx].item.start + 1,
					end:   ix - 1,
					body:  parseItemBodyText(false),
				})
				linkIx := parser.allocs.allocateLink(linkType, uri, "", "")
				parser.tree.nodes[curIx].item.body = parseItemBody{kind: parseItemBody_Link, n: int(linkIx)}
				parser.tree.nodes[curIx].item.end = ix
				parser.tree.nodes[curIx].next = node
				parser.tree.nodes[curIx].child = textNode
				prev = cur
				cur = node
				if nodeIx := cur; nodeIx != treeNone {
					parser.tree.nodes[nodeIx].item.start = max(parser.tree.nodes[nodeIx].item.start, ix)
				}
				continue
			} else {
				var span []byte
				var ix int
				inlineHtml := false
				if nextIx := next; nextIx != treeNone {
					span, ix, inlineHtml = parser.scanInlineHtml(blockText, parser.tree.nodes[nextIx].item.start)
				}
				if inlineHtml {
					node := parseScanNodesToIx(&parser.tree, next, ix)
					if len(span) != 0 {
						// String::from_utf8(span).expect("invalid utf8")
						convertedString := string(span)
						parser.tree.nodes[curIx].item.body = parseItemBody{
							kind: parseItemBody_OwnedHtml,
							n:    int(parser.allocs.allocateCow(convertedString)),
						}
					} else {
						parser.tree.nodes[curIx].item.body = parseItemBody{kind: parseItemBody_InlineHtml}
					}
					parser.tree.nodes[curIx].item.end = ix
					parser.tree.nodes[curIx].next = node
					prev = cur
					cur = node
					if nodeIx := cur; nodeIx != treeNone {
						parser.tree.nodes[nodeIx].item.start = max(parser.tree.nodes[nodeIx].item.start, ix)
					}
					continue
				}
			}
			parser.tree.nodes[curIx].item.body = parseItemBodyText(false)
		case parseItemBody_MaybeCode:
			searchCount, precededByBackslash := body.n, body.b1
			if precededByBackslash {
				searchCount -= 1
				if searchCount == 0 {
					parser.tree.nodes[curIx].item.body = parseItemBodyText(false)
					prev = cur
					cur = parser.tree.nodes[curIx].next
					continue
				}
			}

			if codeDelims.isPopulated() {
				// we have previously scanned all codeblock delimiters,
				// so we can reuse that work
				if scanIx := codeDelims.find(curIx, searchCount); scanIx != treeNone {
					parser.makeCodeSpan(curIx, scanIx, precededByBackslash)
				} else {
					parser.tree.nodes[curIx].item.body = parseItemBodyText(false)
				}
			} else {
				// we haven't previously scanned all codeblock delimiters,
				// so walk the AST
				scan := treeNone
				if searchCount > 0 {
					scan = parser.tree.nodes[curIx].next
				}
				for scan != treeNone {
					scanIx := scan
					if scanBody := parser.tree.nodes[scanIx].item.body; scanBody.kind == parseItemBody_MaybeCode {
						delimCount := scanBody.n
						if searchCount == delimCount {
							parser.makeCodeSpan(curIx, scanIx, precededByBackslash)
							codeDelims.clear()
							break
						} else {
							codeDelims.insert(delimCount, scanIx)
						}
					}
					if parser.tree.nodes[scanIx].item.body.isBlock() {
						// If this is a tight list, blocks and inlines might be
						// siblings. Inlines can't cross the boundary like that.
						scan = treeNone
						break
					}
					scan = parser.tree.nodes[scanIx].next
				}
				if scan == treeNone {
					parser.tree.nodes[curIx].item.body = parseItemBodyText(false)
				}
			}
		case parseItemBody_MaybeLinkOpen:
			parser.tree.nodes[curIx].item.body = parseItemBodyText(false)
			parser.linkStack.push(parseLinkStackEl{
				node: curIx,
				ty:   parseLinkStackTy_Link,
			})
		case parseItemBody_MaybeImage:
			parser.tree.nodes[curIx].item.body = parseItemBodyText(false)
			parser.linkStack.push(parseLinkStackEl{
				node: curIx,
				ty:   parseLinkStackTy_Image,
			})
		case parseItemBody_MaybeLinkClose:
			couldBeRef := body.b1
			parser.tree.nodes[curIx].item.body = parseItemBodyText(false)
			if tos, ok := parser.linkStack.pop(); ok {
				if tos.ty == parseLinkStackTy_Disabled {
					continue
				}
				next := parser.tree.nodes[curIx].next
				if nextIx, url, title, ok := parser.scanInlineLink(blockText, parser.tree.nodes[curIx].item.end, next); ok {
					nextNode := parseScanNodesToIx(&parser.tree, next, nextIx)
					if prevIx := prev; prevIx != treeNone {
						parser.tree.nodes[prevIx].next = treeNone
					}
					cur = tos.node
					curIx = tos.node
					linkIx := parser.allocs.allocateLink(LinkType_Inline, url, title, "")
					if tos.ty == parseLinkStackTy_Image {
						parser.tree.nodes[curIx].item.body = parseItemBody{kind: parseItemBody_Image, n: int(linkIx)}
					} else {
						parser.tree.nodes[curIx].item.body = parseItemBody{kind: parseItemBody_Link, n: int(linkIx)}
					}
					parser.tree.nodes[curIx].child = parser.tree.nodes[curIx].next
					parser.tree.nodes[curIx].next = nextNode
					parser.tree.nodes[curIx].item.end = nextIx
					if nextNodeIx := nextNode; nextNodeIx != treeNone {
						parser.tree.nodes[nextNodeIx].item.start = max(parser.tree.nodes[nextNodeIx].item.start, nextIx)
					}

					if tos.ty == parseLinkStackTy_Link {
						parser.linkStack.disableAllLinks()
					}
				} else {
					// ok, so its not an inline link. maybe it is a reference
					// to a defined link?
					scanResult := parseScanReference(
						&parser.tree,
						blockText,
						next,
						parser.options.Contains(OptionsEnableFootnotes),
						parser.options.hasGfmFootnotes(),
					)
					var nodeAfterLink treeTreeIndex
					var linkType LinkType
					switch scanResult.kind {
					// [label][reference]
					case parseRefScan_LinkLabel:
						endIx := scanResult.endIx
						// Toggle reference viability of the last closing bracket,
						// so that we can skip it on future iterations in case
						// it fails in this one. In particular, we won't call
						// the broken link callback twice on one reference.
						referenceCloseNode := parseScanNodesToIx(&parser.tree, next, endIx-1)
						if referenceCloseNode == treeNone {
							continue
						}
						parser.tree.nodes[referenceCloseNode].item.body = parseItemBody{kind: parseItemBody_MaybeLinkClose, b1: false}
						nextNode := parser.tree.nodes[referenceCloseNode].next

						nodeAfterLink, linkType = nextNode, LinkType_Reference
					// [reference][]
					case parseRefScan_Collapsed:
						nextNode := scanResult.nextNode
						// This reference has already been tried, and it's not
						// valid. Skip it.
						if !couldBeRef {
							continue
						}
						nodeAfterLink, linkType = nextNode, LinkType_Collapsed
					// [shortcut]
					//
					// [shortcut]: /blah
					case parseRefScan_Failed:
						if !couldBeRef {
							continue
						}
						nodeAfterLink, linkType = next, LinkType_Shortcut
					case parseRefScan_UnexpectedFootnote:
						continue
					default:
						panic(rs.Unreachable())
					}

					// FIXME: references and labels are mixed in the naming of variables
					// below. Disambiguate!

					// (label, source_ix end)
					var label linklabelReferenceLabel
					var labelEnd_ int
					hasLabel := false
					switch scanResult.kind {
					case parseRefScan_LinkLabel:
						label = linklabelReferenceLabel{kind: linklabelReferenceLabel_Link, label: scanResult.label}
						labelEnd_ = scanResult.endIx
						hasLabel = true
					case parseRefScan_Collapsed, parseRefScan_Failed:
						// No label? maybe it is a shortcut reference
						labelStart := parser.tree.nodes[tos.node].item.end - 1
						labelEnd := parser.tree.nodes[curIx].item.end
						if ix, scanned, ok := parseScanLinkLabel(
							&parser.tree,
							parser.text[labelStart:labelEnd],
							parser.options.Contains(OptionsEnableFootnotes),
							parser.options.hasGfmFootnotes(),
						); ok {
							if end := labelStart + ix; end == labelEnd {
								label, labelEnd_, hasLabel = scanned, end, true
							}
						}
					case parseRefScan_UnexpectedFootnote:
						continue
					default:
						panic(rs.Unreachable())
					}

					id := ""
					if hasLabel {
						id = label.label
					}

					// see if it's a footnote reference
					if hasLabel && label.kind == linklabelReferenceLabel_Footnote {
						l, end := label.label, labelEnd_
						footref := parser.allocs.allocateCow(l)
						if def := parser.allocs.footdefs.getMut(parser.allocs.cows[footref]); def != nil {
							def.useCount += 1
						}
						if !parser.options.hasGfmFootnotes() ||
							parser.allocs.footdefs.contains(parser.allocs.cows[footref]) {
							// If this came from a MaybeImage, then the `!` prefix
							// isn't part of the footnote reference.
							var footnoteIx treeTreeIndex
							if tos.ty == parseLinkStackTy_Image {
								parser.tree.nodes[tos.node].next = curIx
								parser.tree.nodes[tos.node].child = treeNone
								parser.tree.nodes[tos.node].item.body = parseItemBodySynthesizeChar('!')
								footnoteIx = curIx
							} else {
								footnoteIx = tos.node
							}
							// use `next` instead of `node_after_link` because
							// node_after_link is calculated for a [collapsed][] link,
							// which footnotes don't support.
							parser.tree.nodes[footnoteIx].next = next
							parser.tree.nodes[footnoteIx].child = treeNone
							parser.tree.nodes[footnoteIx].item.body = parseItemBody{kind: parseItemBody_FootnoteReference, n: int(footref)}
							parser.tree.nodes[footnoteIx].item.end = end
							prev = footnoteIx
							cur = next
							parser.linkStack.clear()
							continue
						}
					} else if hasLabel && label.kind == linklabelReferenceLabel_Link {
						linkLabel, end := label.label, labelEnd_
						if defLinkType, url, title, ok := parser.fetchLinkTypeUrlTitle(
							linkLabel,
							Range{Start: parser.tree.nodes[tos.node].item.start, End: end},
							linkType,
						); ok {
							linkIx := parser.allocs.allocateLink(defLinkType, url, title, id)
							if tos.ty == parseLinkStackTy_Image {
								parser.tree.nodes[tos.node].item.body = parseItemBody{kind: parseItemBody_Image, n: int(linkIx)}
							} else {
								parser.tree.nodes[tos.node].item.body = parseItemBody{kind: parseItemBody_Link, n: int(linkIx)}
							}
							labelNode := parser.tree.nodes[tos.node].next

							// lets do some tree surgery to add the link to the tree
							// 1st: skip the label node and close node
							parser.tree.nodes[tos.node].next = nodeAfterLink

							// then, if it exists, add the label node as a child to the link node
							if labelNode != cur {
								parser.tree.nodes[tos.node].child = labelNode

								// finally: disconnect list of children
								if prevIx := prev; prevIx != treeNone {
									parser.tree.nodes[prevIx].next = treeNone
								}
							}

							parser.tree.nodes[tos.node].item.end = end

							// set up cur so next node will be node_after_link
							cur = tos.node
							curIx = tos.node

							if tos.ty == parseLinkStackTy_Link {
								parser.linkStack.disableAllLinks()
							}
						}
					}
				}
			}
		default:
			// If this is a tight list, blocks and inlines might be
			// siblings. Inlines can't cross the boundary like that.
			if curIx := cur; curIx != treeNone {
				if parser.tree.nodes[curIx].item.body.isBlock() {
					parser.linkStack.clear()
				}
			}
		}
		prev = cur
		cur = parser.tree.nodes[curIx].next
	}
	parser.linkStack.clear()
}

func (parser *Parser) handleEmphasisAndHardBreak() {
	prev := treeNone
	var prevIx treeTreeIndex
	cur := parser.tree.cur()

	singleQuoteOpen := treeNone
	doubleQuoteOpen := false

	for cur != treeNone {
		curIx := cur
		switch body := parser.tree.nodes[curIx].item.body; {
		case body.kind == parseItemBody_MaybeEmphasis:
			count, canOpen, canClose := body.n, body.b1, body.b2
			runLength := count
			c := parser.text[parser.tree.nodes[curIx].item.start]
			both := canOpen && canClose
			if canClose {
				for {
					el, ok := parser.inlineStack.findMatch(&parser.tree, c, runLength, both)
					if !ok {
						break
					}
					// have a match!
					if prevIx := prev; prevIx != treeNone {
						parser.tree.nodes[prevIx].next = treeNone
					}
					matchCount := min(count, el.count)
					// start, end are tree node indices
					end := curIx.sub(1)
					start := el.start.add(el.count)

					// work from the inside out
					for start > el.start.add(el.count).sub(matchCount) {
						inc := 1
						if start > el.start.add(el.count).sub(matchCount).add(1) {
							inc = 2
						}
						var ty parseItemBody
						if c == '~' {
							ty = parseItemBody{kind: parseItemBody_Strikethrough}
						} else if inc == 2 {
							ty = parseItemBody{kind: parseItemBody_Strong}
						} else {
							ty = parseItemBody{kind: parseItemBody_Emphasis}
						}

						root := start.sub(inc)
						end = end.add(inc)
						parser.tree.nodes[root].item.body = ty
						parser.tree.nodes[root].item.end = parser.tree.nodes[end].item.end
						parser.tree.nodes[root].child = start
						parser.tree.nodes[root].next = treeNone
						start = root
					}

					// set next for top most emph level
					prevIx = el.start.add(el.count).sub(matchCount)
					prev = prevIx
					cur = parser.tree.nodes[curIx.add(matchCount).sub(1)].next
					parser.tree.nodes[prevIx].next = cur

					if el.count > matchCount {
						parser.inlineStack.push(parseInlineEl{
							start:     el.start,
							count:     el.count - matchCount,
							runLength: el.runLength,
							c:         el.c,
							both:      el.both,
						})
					}
					count -= matchCount
					if count > 0 {
						curIx = cur.unwrap()
					} else {
						break
					}
				}
			}
			if count > 0 {
				if canOpen {
					parser.inlineStack.push(parseInlineEl{
						start:     curIx,
						runLength: runLength,
						count:     count,
						c:         c,
						both:      both,
					})
				} else {
					for i := 0; i < count; i++ {
						parser.tree.nodes[curIx.add(i)].item.body = parseItemBodyText(false)
					}
				}
				prevIx = curIx.add(count).sub(1)
				prev = prevIx
				cur = parser.tree.nodes[prevIx].next
			}
		case body.kind == parseItemBody_MaybeSmartQuote:
			c, canOpen, canClose := body.c, body.b1, body.b2
			var newBody parseItemBody
			switch c {
			case '\'':
				if openIx := singleQuoteOpen; openIx != treeNone && canClose {
					parser.tree.nodes[openIx].item.body = parseItemBodySynthesizeChar('‘')
					singleQuoteOpen = treeNone
				} else if canOpen {
					singleQuoteOpen = curIx
				}
				newBody = parseItemBodySynthesizeChar('’')
			default: /* double quote */
				if canClose && doubleQuoteOpen {
					doubleQuoteOpen = false
					newBody = parseItemBodySynthesizeChar('”')
				} else {
					if canOpen && !doubleQuoteOpen {
						doubleQuoteOpen = true
					}
					newBody = parseItemBodySynthesizeChar('“')
				}
			}
			parser.tree.nodes[curIx].item.body = newBody
			prev = cur
			cur = parser.tree.nodes[curIx].next
		case body.kind == parseItemBody_HardBreak && body.b1:
			if parser.tree.nodes[curIx].next == treeNone {
				parser.tree.nodes[curIx].item.body = parseItemBodySynthesizeChar('\\')
			}
			prev = cur
			cur = parser.tree.nodes[curIx].next
		default:
			prev = cur
			// If this is a tight list, blocks and inlines might be
			// siblings. Inlines can't cross the boundary like that.
			if curIx := cur; curIx != treeNone {
				if parser.tree.nodes[curIx].item.body.isBlock() {
					parser.inlineStack.popAll(&parser.tree)
				}
			}
			cur = parser.tree.nodes[curIx].next
		}
	}
	parser.inlineStack.popAll(&parser.tree)
}

// scanInlineLink returns next byte index, url and title.
func (parser *Parser) scanInlineLink(
	underlying string,
	ix int,
	node treeTreeIndex,
) (int, string, string, bool) {
	if scannersScanCh(underlying[ix:], '(') == 0 {
		return 0, "", "", false
	}
	ix += 1

	scanSeparator := func(ix *int) {
		*ix += scannersScanWhile(underlying[*ix:], scannersIsAsciiWhitespaceNoNl)
		if bl, ok := scannersScanEol(underlying[*ix:]); ok {
			*ix += bl
			lineStart := scannersLineStartNew(underlying[*ix:])
			_ = parseScanContainers(
				&parser.tree,
				&lineStart,
				parser.options.hasGfmFootnotes(),
			)
			*ix += lineStart.bytesScanned()
		}
		*ix += scannersScanWhile(underlying[*ix:], scannersIsAsciiWhitespaceNoNl)
	}

	scanSeparator(&ix)

	destLength, dest, ok := scannersScanLinkDest(underlying, ix, parseLinkMaxNestedParens)
	if !ok {
		return 0, "", "", false
	}
	dest = scannersUnescape(dest, parser.tree.isInTable())
	ix += destLength

	scanSeparator(&ix)

	title := ""
	if bytesScanned, t, ok := parser.scanLinkTitle(underlying, ix, node); ok {
		ix += bytesScanned
		scanSeparator(&ix)
		title = t
	}
	if scannersScanCh(underlying[ix:], ')') == 0 {
		return 0, "", "", false
	}
	ix += 1

	return ix, dest, title, true
}

// scanLinkTitle returns (bytes scanned, title cow)
func (parser *Parser) scanLinkTitle(
	text string,
	startIx int,
	node treeTreeIndex,
) (int, string, bool) {
	bytes := text
	if startIx >= len(bytes) {
		return 0, "", false
	}
	var open byte
	switch b := bytes[startIx]; b {
	case '\'', '"', '(':
		open = b
	default:
		return 0, "", false
	}
	close_ := open
	if open == '(' {
		close_ = ')'
	}

	var title strings.Builder
	mark := startIx + 1
	i := startIx + 1

	for i < len(bytes) {
		c := bytes[i]

		if c == close_ {
			if mark == 1 {
				return i - startIx + 1, text[mark:i], true
			}
			title.WriteString(text[mark:i])
			return i - startIx + 1, title.String(), true
		}
		if c == open {
			return 0, "", false
		}

		if c == '\n' || c == '\r' {
			if nodeIx := parseScanNodesToIx(&parser.tree, node, i+1); nodeIx != treeNone {
				if parser.tree.nodes[nodeIx].item.start > i {
					title.WriteString(text[mark:i])
					title.WriteByte('\n')
					i = parser.tree.nodes[nodeIx].item.start
					mark = i
					continue
				}
			}
		}
		if c == '&' {
			if n, value, ok := scannersScanEntity(bytes[i:]); ok {
				title.WriteString(text[mark:i])
				title.WriteString(value)
				i += n
				mark = i
				continue
			}
		}
		if parser.tree.isInTable() && c == '\\' && i+2 < len(bytes) && bytes[i+1] == '\\' && bytes[i+2] == '|' {
			// this runs if there are an even number of pipes in a table
			// if it's odd, then it gets parsed as normal
			title.WriteString(text[mark:i])
			i += 2
			mark = i
		}
		if c == '\\' && i+1 < len(bytes) && puncttableIsAsciiPunctuation(bytes[i+1]) {
			title.WriteString(text[mark:i])
			i += 1
			mark = i
		}

		i += 1
	}

	return 0, "", false
}

// makeCodeSpan makes a code span.
//
// Both `open` and `close` are matching MaybeCode items.
func (parser *Parser) makeCodeSpan(open treeTreeIndex, close_ treeTreeIndex, precedingBackslash bool) {
	bytes := parser.text
	spanStart := parser.tree.nodes[open].item.end
	spanEnd := parser.tree.nodes[close_].item.start
	// buf is `Option<String>`: hasBuf tells None from an empty string.
	var buf []byte
	hasBuf := false

	startIx := spanStart
	ix := spanStart
	for ix < spanEnd {
		c := bytes[ix]
		if c == '\r' || c == '\n' {
			hasBuf = true
			buf = append(buf, parser.text[startIx:ix]...)
			buf = append(buf, ' ')
			ix += 1
			lineStart := scannersLineStartNew(bytes[ix:])
			_ = parseScanContainers(
				&parser.tree,
				&lineStart,
				parser.options.hasGfmFootnotes(),
			)
			ix += lineStart.bytesScanned()
			startIx = ix
		} else if c == '\\' && ix+1 < len(bytes) && bytes[ix+1] == '|' && parser.tree.isInTable() {
			hasBuf = true
			buf = append(buf, parser.text[startIx:ix]...)
			buf = append(buf, '|')
			ix += 2
			startIx = ix
		} else {
			ix += 1
		}
	}

	var s string
	if hasBuf {
		buf = append(buf, parser.text[startIx:spanEnd]...)
		s = string(buf)
	} else {
		s = parser.text[spanStart:spanEnd]
	}
	opening := len(s) > 0 && s[0] == ' '
	closing := len(s) > 0 && s[len(s)-1] == ' '
	allSpaces := true
	for i := 0; i < len(s); i++ {
		if s[i] != ' ' {
			allSpaces = false
			break
		}
	}

	var cow string
	if !allSpaces && opening && closing {
		if hasBuf {
			// buf.remove(0); buf.pop();
			cow = s[1 : len(s)-1]
		} else {
			lo := spanStart + 1
			hi := max(spanEnd-1, lo)
			cow = parser.text[lo:hi]
		}
	} else if hasBuf {
		cow = s
	} else {
		cow = parser.text[spanStart:spanEnd]
	}

	if precedingBackslash {
		parser.tree.nodes[open].item.body = parseItemBodyText(true)
		parser.tree.nodes[open].item.end = parser.tree.nodes[open].item.start + 1
		parser.tree.nodes[open].next = close_
		parser.tree.nodes[close_].item.body = parseItemBody{kind: parseItemBody_Code, n: int(parser.allocs.allocateCow(cow))}
		parser.tree.nodes[close_].item.start = parser.tree.nodes[open].item.start + 1
	} else {
		parser.tree.nodes[open].item.body = parseItemBody{kind: parseItemBody_Code, n: int(parser.allocs.allocateCow(cow))}
		parser.tree.nodes[open].item.end = parser.tree.nodes[close_].item.end
		parser.tree.nodes[open].next = parser.tree.nodes[close_].next
	}
}

// scanInlineHtml, on success, returns a buffer containing the inline html and
// byte offset. When no bytes were skipped, the buffer will be empty and the
// html can be represented as a subslice of the input string.
func (parser *Parser) scanInlineHtml(bytes string, ix int) ([]byte, int, bool) {
	if ix >= len(bytes) {
		return nil, 0, false
	}
	c := bytes[ix]
	if c == '!' {
		end, ok := scannersScanInlineHtmlComment(bytes, ix+1, &parser.htmlScanGuard)
		if !ok {
			return nil, 0, false
		}
		return nil, end, true
	} else if c == '?' {
		end, ok := scannersScanInlineHtmlProcessing(bytes, ix+1, &parser.htmlScanGuard)
		if !ok {
			return nil, 0, false
		}
		return nil, end, true
	} else {
		span, i, ok := scannersScanHtmlBlockInner(
			// Subtract 1 to include the < character
			bytes[(ix-1):],
			func(bytes string) int {
				lineStart := scannersLineStartNew(bytes)
				_ = parseScanContainers(
					&parser.tree,
					&lineStart,
					parser.options.hasGfmFootnotes(),
				)
				return lineStart.bytesScanned()
			},
		)
		if !ok {
			return nil, 0, false
		}
		return span, i + ix - 1, true
	}
}

// IntoOffsetIter consumes the event iterator and produces an iterator that
// produces `(Event, Range)` pairs, where the `Range` value maps to the
// corresponding range in the markdown source.
func (parser *Parser) IntoOffsetIter() *OffsetIter {
	return &OffsetIter{inner: parser}
}

// parseScanContainers returns number of containers scanned.
func parseScanContainers(
	tree *treeTree,
	lineStart *scannersLineStart,
	gfmFootnotes bool,
) int {
	i := 0
loop:
	for _, nodeIx := range tree.walkSpine() {
		switch body := tree.nodes[nodeIx].item.body; {
		case body.kind == parseItemBody_BlockQuote:
			// `scan_blockquote_marker` saves & restores internally
			if !lineStart.scanBlockquoteMarker() {
				break loop
			}
		case body.kind == parseItemBody_ListItem:
			indent := body.n
			save := *lineStart
			if !lineStart.scanSpace(indent) && !lineStart.isAtEol() {
				*lineStart = save
				break loop
			}
		case body.kind == parseItemBody_FootnoteDefinition && gfmFootnotes:
			save := *lineStart
			if !lineStart.scanSpace(4) && !lineStart.isAtEol() {
				*lineStart = save
				break loop
			}
		default:
		}
		i += 1
	}
	return i
}

func (tree *treeTree) appendText(start int, end int, backslashEscaped bool) {
	if end > start {
		if ix := tree.cur(); ix != treeNone {
			if tree.nodes[ix].item.body.kind == parseItemBody_Text && tree.nodes[ix].item.end == start {
				tree.nodes[ix].item.end = end
				return
			}
		}
		tree.append(parseItem{
			start: start,
			end:   end,
			body:  parseItemBodyText(backslashEscaped),
		})
	}
}

// isInTable returns true if the current node is inside a table.
//
// If `cur` is an ItemBody::Table, it would return false,
// but since the `TableRow` and `TableHead` and `TableCell`
// are children of the table, anything doing inline parsing
// doesn't need to care about that.
func (tree *treeTree) isInTable() bool {
	mightBeInTable := func(item *parseItem) bool {
		return item.body.isInline() ||
			item.body.kind == parseItemBody_TableHead ||
			item.body.kind == parseItemBody_TableRow ||
			item.body.kind == parseItemBody_TableCell
	}
	spine := tree.walkSpine()
	for i := len(spine) - 1; i >= 0; i-- {
		ix := spine[i]
		if tree.nodes[ix].item.body.kind == parseItemBody_Table {
			return true
		}
		if !mightBeInTable(&tree.nodes[ix].item) {
			return false
		}
	}
	return false
}

type parseInlineEl struct {
	// offset of tree node
	start treeTreeIndex
	// number of delimiters available for matching
	count int
	// length of the run that these delimiters came from
	runLength int
	// b'*', b'_', or b'~'
	c byte
	// can both open and close
	both bool
}

type parseInlineStack struct {
	stack []parseInlineEl
	// Lower bounds for matching indices in the stack. For example
	// a strikethrough delimiter will never match with any element
	// in the stack with index smaller than
	// `lower_bounds[InlineStack::TILDES]`.
	lowerBounds [9]int
}

// These are indices into the lower bounds array.
// Not both refers to the property that the delimiter can not both
// be opener as a closer.
const (
	parseInlineStackUnderscoreNotBoth = 0
	parseInlineStackAsteriskNotBoth   = 1
	parseInlineStackAsteriskBase      = 2
	parseInlineStackTildes            = 5
	parseInlineStackUnderscoreBase    = 6
)

func (stack *parseInlineStack) popAll(tree *treeTree) {
	for _, el := range stack.stack {
		for i := 0; i < el.count; i++ {
			tree.nodes[el.start.add(i)].item.body = parseItemBodyText(false)
		}
	}
	stack.stack = stack.stack[:0]
	stack.lowerBounds = [9]int{}
}

func (stack *parseInlineStack) getLowerbound(c byte, count int, both bool) int {
	if c == '_' {
		mod3Lower := stack.lowerBounds[parseInlineStackUnderscoreBase+count%3]
		if both {
			return mod3Lower
		} else {
			return min(
				mod3Lower,
				stack.lowerBounds[parseInlineStackUnderscoreNotBoth],
			)
		}
	} else if c == '*' {
		mod3Lower := stack.lowerBounds[parseInlineStackAsteriskBase+count%3]
		if both {
			return mod3Lower
		} else {
			return min(
				mod3Lower,
				stack.lowerBounds[parseInlineStackAsteriskNotBoth],
			)
		}
	} else {
		return stack.lowerBounds[parseInlineStackTildes]
	}
}

func (stack *parseInlineStack) setLowerbound(c byte, count int, both bool, newBound int) {
	if c == '_' {
		if both {
			stack.lowerBounds[parseInlineStackUnderscoreBase+count%3] = newBound
		} else {
			stack.lowerBounds[parseInlineStackUnderscoreNotBoth] = newBound
		}
	} else if c == '*' {
		stack.lowerBounds[parseInlineStackAsteriskBase+count%3] = newBound
		if !both {
			stack.lowerBounds[parseInlineStackAsteriskNotBoth] = newBound
		}
	} else {
		stack.lowerBounds[parseInlineStackTildes] = newBound
	}
}

func (stack *parseInlineStack) truncate(newBound int) {
	if newBound < len(stack.stack) {
		stack.stack = stack.stack[:newBound]
	}
	for i := range stack.lowerBounds {
		lowerBound := &stack.lowerBounds[i]
		if *lowerBound > newBound {
			*lowerBound = newBound
		}
	}
}

func (stack *parseInlineStack) findMatch(
	tree *treeTree,
	c byte,
	runLength int,
	both bool,
) (parseInlineEl, bool) {
	lowerbound := min(len(stack.stack), stack.getLowerbound(c, runLength, both))
	matchingIx := -1
	for i := len(stack.stack) - 1; i >= lowerbound; i-- {
		el := &stack.stack[i]
		if c == '~' && runLength != el.runLength {
			continue
		}
		if el.c == c && (!both && !el.both || (runLength+el.runLength)%3 != 0 || runLength%3 == 0) {
			matchingIx = i
			break
		}
	}

	if matchingIx >= 0 {
		matchingEl := stack.stack[matchingIx]
		for _, el := range stack.stack[(matchingIx + 1):] {
			for i := 0; i < el.count; i++ {
				tree.nodes[el.start.add(i)].item.body = parseItemBodyText(false)
			}
		}
		stack.truncate(matchingIx)
		return matchingEl, true
	} else {
		stack.setLowerbound(c, runLength, both, len(stack.stack))
		return parseInlineEl{}, false
	}
}

func (stack *parseInlineStack) trimLowerBound(ix int) {
	stack.lowerBounds[ix] = min(stack.lowerBounds[ix], len(stack.stack))
}

func (stack *parseInlineStack) push(el parseInlineEl) {
	if el.c == '~' {
		stack.trimLowerBound(parseInlineStackTildes)
	}
	stack.stack = append(stack.stack, el)
}

type parseRefScanKind uint8

const (
	// label, source ix of label end
	parseRefScan_LinkLabel parseRefScanKind = iota
	// contains next node index
	parseRefScan_Collapsed
	parseRefScan_UnexpectedFootnote
	parseRefScan_Failed
)

// parseRefScan is the crate's RefScan enum, held flat: label and endIx
// belong to LinkLabel, nextNode to Collapsed.
type parseRefScan struct {
	kind     parseRefScanKind
	label    string
	endIx    int
	nextNode treeTreeIndex
}

// parseScanNodesToIx skips forward within a block to a node which spans (ends
// inclusive) the given index into the source.
func parseScanNodesToIx(
	tree *treeTree,
	node treeTreeIndex,
	ix int,
) treeTreeIndex {
	for node != treeNone {
		nodeIx := node
		if tree.nodes[nodeIx].item.end <= ix {
			node = tree.nodes[nodeIx].next
		} else {
			break
		}
	}
	return node
}

// parseScanLinkLabel scans an inline link label, which cannot be interrupted.
// Returns number of bytes (including brackets) and label on success.
func parseScanLinkLabel(
	tree *treeTree,
	text string,
	allowFootnoteRefs bool,
	gfmFootnotes bool,
) (int, linklabelReferenceLabel, bool) {
	bytes := text
	if len(bytes) < 2 || bytes[0] != '[' {
		return 0, linklabelReferenceLabel{}, false
	}
	linebreakHandler := func(bytes string) (int, bool) {
		lineStart := scannersLineStartNew(bytes)
		_ = parseScanContainers(tree, &lineStart, gfmFootnotes)
		return lineStart.bytesScanned(), true
	}
	if allowFootnoteRefs && '^' == bytes[1] && !(len(bytes) > 2 && bytes[2] == ']') {
		linebreakHandler := linebreakHandler
		if gfmFootnotes {
			linebreakHandler = func(string) (int, bool) { return 0, false }
		}
		if byteIndex, cow, ok := linklabelScanLinkLabelRest(text[2:], linebreakHandler, tree.isInTable()); ok {
			return byteIndex + 2, linklabelReferenceLabel{kind: linklabelReferenceLabel_Footnote, label: cow}, true
		}
	}
	byteIndex, cow, ok := linklabelScanLinkLabelRest(text[1:], linebreakHandler, tree.isInTable())
	if !ok {
		return 0, linklabelReferenceLabel{}, false
	}
	return byteIndex + 1, linklabelReferenceLabel{kind: linklabelReferenceLabel_Link, label: cow}, true
}

func parseScanReference(
	tree *treeTree,
	text string,
	cur treeTreeIndex,
	allowFootnoteRefs bool,
	gfmFootnotes bool,
) parseRefScan {
	if cur == treeNone {
		return parseRefScan{kind: parseRefScan_Failed}
	}
	curIx := cur
	start := tree.nodes[curIx].item.start
	tail := text[start:]

	if strings.HasPrefix(tail, "[]") {
		// TODO: this unwrap is sus and should be looked at closer
		closingNode := tree.nodes[curIx].next.unwrap()
		return parseRefScan{kind: parseRefScan_Collapsed, nextNode: tree.nodes[closingNode].next}
	} else {
		ix, label, ok := parseScanLinkLabel(tree, text[start:], allowFootnoteRefs, gfmFootnotes)
		switch {
		case ok && label.kind == linklabelReferenceLabel_Link:
			return parseRefScan{kind: parseRefScan_LinkLabel, label: label.label, endIx: start + ix}
		case ok && label.kind == linklabelReferenceLabel_Footnote:
			return parseRefScan{kind: parseRefScan_UnexpectedFootnote}
		default:
			return parseRefScan{kind: parseRefScan_Failed}
		}
	}
}

type parseLinkStack struct {
	inner      []parseLinkStackEl
	disabledIx int
}

func (stack *parseLinkStack) push(el parseLinkStackEl) {
	stack.inner = append(stack.inner, el)
}

func (stack *parseLinkStack) pop() (parseLinkStackEl, bool) {
	var el parseLinkStackEl
	ok := len(stack.inner) > 0
	if ok {
		el = stack.inner[len(stack.inner)-1]
		stack.inner = stack.inner[:len(stack.inner)-1]
	}
	stack.disabledIx = min(stack.disabledIx, len(stack.inner))
	return el, ok
}

func (stack *parseLinkStack) clear() {
	stack.inner = stack.inner[:0]
	stack.disabledIx = 0
}

func (stack *parseLinkStack) disableAllLinks() {
	for i := stack.disabledIx; i < len(stack.inner); i++ {
		el := &stack.inner[i]
		if el.ty == parseLinkStackTy_Link {
			el.ty = parseLinkStackTy_Disabled
		}
	}
	stack.disabledIx = len(stack.inner)
}

type parseLinkStackEl struct {
	node treeTreeIndex
	ty   parseLinkStackTy
}

type parseLinkStackTy uint8

const (
	parseLinkStackTy_Link parseLinkStackTy = iota
	parseLinkStackTy_Image
	parseLinkStackTy_Disabled
)

// LinkDef contains the destination URL, title and source span of a reference
// definition.
type LinkDef struct {
	Dest  string            `rs:"dest"`
	Title rs.Option[string] `rs:"title"`
	Span  Range             `rs:"span"`
}

var _ = rs.RegisterStruct[LinkDef]("LinkDef")

// parseFootnoteDef contains the destination URL, title and source span of a
// reference definition.
type parseFootnoteDef struct {
	useCount int
}

// parseCodeDelims tracks tree indices of code span delimiters of each length.
// It should prevent quadratic scanning behaviours by providing (amortized)
// constant time lookups.
type parseCodeDelims struct {
	inner     map[int][]treeTreeIndex
	seenFirst bool
}

func parseCodeDelimsNew() parseCodeDelims {
	return parseCodeDelims{
		inner:     map[int][]treeTreeIndex{},
		seenFirst: false,
	}
}

func (delims *parseCodeDelims) insert(count int, ix treeTreeIndex) {
	if delims.seenFirst {
		delims.inner[count] = append(delims.inner[count], ix)
	} else {
		// Skip the first insert, since that delimiter will always
		// be an opener and not a closer.
		delims.seenFirst = true
	}
}

func (delims *parseCodeDelims) isPopulated() bool {
	return len(delims.inner) != 0
}

func (delims *parseCodeDelims) find(openIx treeTreeIndex, count int) treeTreeIndex {
	for {
		queue, ok := delims.inner[count]
		if !ok || len(queue) == 0 {
			return treeNone
		}
		// pop_front
		ix := queue[0]
		delims.inner[count] = queue[1:]
		if ix > openIx {
			return ix
		}
	}
}

func (delims *parseCodeDelims) clear() {
	clear(delims.inner)
	delims.seenFirst = false
}

type parseLinkIndex int

type parseCowIndex int

type parseAlignmentIndex int

// parseHeadingIndex is a NonZeroUsize in Rust: zero is the None of
// `Option<HeadingIndex>`.
type parseHeadingIndex int

// parseLink is the tuple `(LinkType, CowStr, CowStr, CowStr)` that the
// allocations hold for a link: its type, destination URL, title and id.
type parseLink struct {
	linkType LinkType
	destUrl  string
	title    string
	id       string
}

type parseAllocations struct {
	refdefs    RefDefs
	footdefs   parseFootnoteDefs
	links      []parseLink
	cows       []string
	alignments [][]Alignment
	headings   []parseHeadingAttributes
}

// parseHeadingAttributes is used by the heading attributes extension.
type parseHeadingAttributes struct {
	id      rs.Option[string]
	classes []string
	attrs   []rs.Tuple2[string, rs.Option[string]]
}

// RefDefs keeps track of the reference definitions defined in the document.
//
// Rust holds them in a `HashMap<UniCase<CowStr>, LinkDef>`; see unicase.go
// for why a map keyed by the folded label is the same collection.
type RefDefs struct {
	// entries holds the definitions in the order the document defines them.
	entries []refDefsEntry
	// index maps a label's case folding to its position in entries.
	index map[string]int
}

type refDefsEntry struct {
	label string
	def   LinkDef
}

// parseFootnoteDefs keeps track of the footnote definitions defined in the
// document.
type parseFootnoteDefs struct {
	inner map[string]*parseFootnoteDef
}

// Get performs a lookup on reference label using unicode case folding.
func (defs *RefDefs) Get(key string) rs.Option[*LinkDef] {
	if position, ok := defs.index[unicaseToFoldedCase(key)]; ok {
		return rs.Some(&defs.entries[position].def)
	}
	return rs.None[*LinkDef]()
}

// Iter provides an iterator over all the document's reference definitions:
// each label as the document wrote it, with its definition.
//
// Rust iterates a HashMap here, in an order that changes from run to run.
// This iterator yields the definitions in document order.
func (defs *RefDefs) Iter() iter.Seq2[string, *LinkDef] {
	return func(yield func(string, *LinkDef) bool) {
		for i := range defs.entries {
			if !yield(defs.entries[i].label, &defs.entries[i].def) {
				return
			}
		}
	}
}

// entryOrInsert is `self.0.entry(label).or_insert(link_def)`: the first
// definition of a label stays.
func (defs *RefDefs) entryOrInsert(label string, linkDef LinkDef) {
	key := unicaseToFoldedCase(label)
	if _, ok := defs.index[key]; ok {
		return
	}
	if defs.index == nil {
		defs.index = map[string]int{}
	}
	defs.index[key] = len(defs.entries)
	defs.entries = append(defs.entries, refDefsEntry{label: label, def: linkDef})
}

// contains performs a lookup on reference label using unicode case folding.
func (defs *parseFootnoteDefs) contains(key string) bool {
	_, ok := defs.inner[unicaseToFoldedCase(key)]
	return ok
}

// getMut performs a lookup on reference label using unicode case folding. It
// returns nil for None.
func (defs *parseFootnoteDefs) getMut(key string) *parseFootnoteDef {
	return defs.inner[unicaseToFoldedCase(key)]
}

// insert is `self.0.insert(UniCase::new(label), def)`.
func (defs *parseFootnoteDefs) insert(label string, def parseFootnoteDef) {
	if defs.inner == nil {
		defs.inner = map[string]*parseFootnoteDef{}
	}
	defs.inner[unicaseToFoldedCase(label)] = &def
}

func parseAllocationsNew() parseAllocations {
	return parseAllocations{
		refdefs:    RefDefs{},
		footdefs:   parseFootnoteDefs{},
		links:      make([]parseLink, 0, 128),
		cows:       nil,
		alignments: nil,
		headings:   nil,
	}
}

func (allocs *parseAllocations) allocateCow(cow string) parseCowIndex {
	ix := len(allocs.cows)
	allocs.cows = append(allocs.cows, cow)
	return parseCowIndex(ix)
}

func (allocs *parseAllocations) allocateLink(
	ty LinkType,
	url string,
	title string,
	id string,
) parseLinkIndex {
	ix := len(allocs.links)
	allocs.links = append(allocs.links, parseLink{linkType: ty, destUrl: url, title: title, id: id})
	return parseLinkIndex(ix)
}

func (allocs *parseAllocations) allocateAlignment(alignment []Alignment) parseAlignmentIndex {
	ix := len(allocs.alignments)
	allocs.alignments = append(allocs.alignments, alignment)
	return parseAlignmentIndex(ix)
}

func (allocs *parseAllocations) allocateHeading(attrs parseHeadingAttributes) parseHeadingIndex {
	ix := len(allocs.headings)
	allocs.headings = append(allocs.headings, attrs)
	// This won't panic. `self.headings.len()` can't be `usize::MAX` since
	// such a long Vec cannot fit in memory.
	return parseHeadingIndex(ix + 1)
}

func (allocs *parseAllocations) takeCow(ix parseCowIndex) string {
	cow := allocs.cows[ix]
	allocs.cows[ix] = ""
	return cow
}

func (allocs *parseAllocations) takeLink(ix parseLinkIndex) parseLink {
	defaultLink := parseLink{linkType: LinkType_ShortcutUnknown, destUrl: "", title: "", id: ""}
	link := allocs.links[ix]
	allocs.links[ix] = defaultLink
	return link
}

func (allocs *parseAllocations) takeAlignment(ix parseAlignmentIndex) []Alignment {
	alignment := allocs.alignments[ix]
	allocs.alignments[ix] = nil
	return alignment
}

// heading is `impl Index<HeadingIndex> for Allocations`.
func (allocs *parseAllocations) heading(ix parseHeadingIndex) *parseHeadingAttributes {
	return &allocs.headings[int(ix)-1]
}

// parseHtmlScanGuard is a struct containing information on the reachability
// of certain inline HTML elements. In particular, for cdata elements
// (`<![CDATA[`), processing elements (`<?`) and declarations
// (`<!DECLARATION`). The respectives usizes represent the indices before
// which a scan will always fail and can hence be skipped.
type parseHtmlScanGuard struct {
	cdata       int
	processing  int
	declaration int
}

// BrokenLinkCallback is the trait for broken link callbacks.
//
// See ParserNewWithBrokenLinkCallback. A function with the appropriate
// signature implements it as a BrokenLinkCallbackFunc.
type BrokenLinkCallback interface {
	// HandleBrokenLink returns the URL and the title to use for the link, or
	// None to leave the reference as text.
	HandleBrokenLink(link BrokenLink) rs.Option[rs.Tuple2[string, string]]
}

// BrokenLinkCallbackFunc is the blanket impl of BrokenLinkCallback for
// closures: `FnMut(BrokenLink) -> Option<(CowStr, CowStr)>`.
type BrokenLinkCallbackFunc func(link BrokenLink) rs.Option[rs.Tuple2[string, string]]

// HandleBrokenLink calls the function.
func (callback BrokenLinkCallbackFunc) HandleBrokenLink(link BrokenLink) rs.Option[rs.Tuple2[string, string]] {
	return callback(link)
}

// DefaultBrokenLinkCallback is the broken link callback that does nothing.
type DefaultBrokenLinkCallback struct{}

var _ = rs.RegisterUnitStruct[DefaultBrokenLinkCallback]("DefaultBrokenLinkCallback")

// HandleBrokenLink returns None.
func (DefaultBrokenLinkCallback) HandleBrokenLink(_ BrokenLink) rs.Option[rs.Tuple2[string, string]] {
	return rs.None[rs.Tuple2[string, string]]()
}

// OffsetIter is the Markdown event and source range iterator.
//
// Generates tuples where the first element is the markdown event and the second
// is a the corresponding range in the source string.
//
// Constructed from a `Parser` using its IntoOffsetIter method.
type OffsetIter struct {
	inner *Parser `rs:"inner"`
}

var _ = rs.RegisterStruct[OffsetIter]("OffsetIter")

// ReferenceDefinitions returns a reference to the internal reference
// definition tracker.
func (offsetIter *OffsetIter) ReferenceDefinitions() *RefDefs {
	return offsetIter.inner.ReferenceDefinitions()
}

// Next is `impl Iterator for OffsetIter`.
func (offsetIter *OffsetIter) Next() rs.Option[rs.Tuple2[Event, Range]] {
	curIx := offsetIter.inner.tree.cur()
	if curIx == treeNone {
		ix := offsetIter.inner.tree.pop()
		if ix == treeNone {
			return rs.None[rs.Tuple2[Event, Range]]()
		}
		tagEnd := parseBodyToTagEnd(&offsetIter.inner.tree.nodes[ix].item.body)
		offsetIter.inner.tree.nextSibling(ix)
		span := Range{Start: offsetIter.inner.tree.nodes[ix].item.start, End: offsetIter.inner.tree.nodes[ix].item.end}
		debugAssert(span.Start <= span.End, "assertion failed: span.start <= span.end")
		return rs.Some(rs.T2[Event, Range](&Event_End{F0: tagEnd}, span))
	}
	if offsetIter.inner.tree.nodes[curIx].item.body.isInline() {
		offsetIter.inner.handleInline()
	}

	node := offsetIter.inner.tree.nodes[curIx]
	item := node.item
	event := parseItemToEvent(item, offsetIter.inner.text, &offsetIter.inner.allocs)
	if rs.Is[*Event_Start](event) {
		offsetIter.inner.tree.push()
	} else {
		offsetIter.inner.tree.nextSibling(curIx)
	}
	debugAssert(item.start <= item.end, "assertion failed: item.start <= item.end")
	return rs.Some(rs.T2(event, Range{Start: item.start, End: item.end}))
}

// All ranges over the remaining events and their source ranges; it is `for
// (event, range) in offset_iter`.
func (offsetIter *OffsetIter) All() iter.Seq2[Event, Range] {
	return func(yield func(Event, Range) bool) {
		for {
			next, ok := offsetIter.Next().Get()
			if !ok || !yield(next.F0, next.F1) {
				return
			}
		}
	}
}

func parseBodyToTagEnd(body *parseItemBody) TagEnd {
	switch body.kind {
	case parseItemBody_Paragraph:
		return &TagEnd_Paragraph{}
	case parseItemBody_Emphasis:
		return &TagEnd_Emphasis{}
	case parseItemBody_Strong:
		return &TagEnd_Strong{}
	case parseItemBody_Strikethrough:
		return &TagEnd_Strikethrough{}
	case parseItemBody_Link:
		return &TagEnd_Link{}
	case parseItemBody_Image:
		return &TagEnd_Image{}
	case parseItemBody_Heading:
		return &TagEnd_Heading{F0: HeadingLevel(body.c)}
	case parseItemBody_IndentCodeBlock, parseItemBody_FencedCodeBlock:
		return &TagEnd_CodeBlock{}
	case parseItemBody_BlockQuote:
		return &TagEnd_BlockQuote{}
	case parseItemBody_HtmlBlock:
		return &TagEnd_HtmlBlock{}
	case parseItemBody_List:
		c := body.c
		isOrdered := c == '.' || c == ')'
		return &TagEnd_List{F0: isOrdered}
	case parseItemBody_ListItem:
		return &TagEnd_Item{}
	case parseItemBody_TableHead:
		return &TagEnd_TableHead{}
	case parseItemBody_TableCell:
		return &TagEnd_TableCell{}
	case parseItemBody_TableRow:
		return &TagEnd_TableRow{}
	case parseItemBody_Table:
		return &TagEnd_Table{}
	case parseItemBody_FootnoteDefinition:
		return &TagEnd_FootnoteDefinition{}
	case parseItemBody_MetadataBlock:
		return &TagEnd_MetadataBlock{F0: MetadataBlockKind(body.c)}
	default:
		rs.PanicAt(0, "unexpected item body "+body.debugString())
		panic(rs.Unreachable())
	}
}

func parseItemToEvent(item parseItem, text string, allocs *parseAllocations) Event {
	var tag Tag
	switch item.body.kind {
	case parseItemBody_Text:
		return &Event_Text{F0: strSlice(text, item.start, item.end)}
	case parseItemBody_Code:
		return &Event_Code{F0: allocs.takeCow(parseCowIndex(item.body.n))}
	case parseItemBody_SynthesizeText:
		return &Event_Text{F0: allocs.takeCow(parseCowIndex(item.body.n))}
	case parseItemBody_SynthesizeChar:
		return &Event_Text{F0: string(rune(item.body.n))}
	case parseItemBody_HtmlBlock:
		tag = &Tag_HtmlBlock{}
	case parseItemBody_Html:
		return &Event_Html{F0: strSlice(text, item.start, item.end)}
	case parseItemBody_InlineHtml:
		return &Event_InlineHtml{F0: strSlice(text, item.start, item.end)}
	case parseItemBody_OwnedHtml:
		return &Event_Html{F0: allocs.takeCow(parseCowIndex(item.body.n))}
	case parseItemBody_SoftBreak:
		return &Event_SoftBreak{}
	case parseItemBody_HardBreak:
		return &Event_HardBreak{}
	case parseItemBody_FootnoteReference:
		return &Event_FootnoteReference{F0: allocs.takeCow(parseCowIndex(item.body.n))}
	case parseItemBody_TaskListMarker:
		return &Event_TaskListMarker{F0: item.body.b1}
	case parseItemBody_Rule:
		return &Event_Rule{}
	case parseItemBody_Paragraph:
		tag = &Tag_Paragraph{}
	case parseItemBody_Emphasis:
		tag = &Tag_Emphasis{}
	case parseItemBody_Strong:
		tag = &Tag_Strong{}
	case parseItemBody_Strikethrough:
		tag = &Tag_Strikethrough{}
	case parseItemBody_Link:
		link := allocs.takeLink(parseLinkIndex(item.body.n))
		tag = &Tag_Link{
			LinkType: link.linkType,
			DestUrl:  link.destUrl,
			Title:    link.title,
			Id:       link.id,
		}
	case parseItemBody_Image:
		link := allocs.takeLink(parseLinkIndex(item.body.n))
		tag = &Tag_Image{
			LinkType: link.linkType,
			DestUrl:  link.destUrl,
			Title:    link.title,
			Id:       link.id,
		}
	case parseItemBody_Heading:
		level := HeadingLevel(item.body.c)
		if headingIx := parseHeadingIndex(item.body.n); headingIx != 0 {
			attributes := allocs.heading(headingIx)
			tag = &Tag_Heading{
				Level:   level,
				Id:      attributes.id,
				Classes: append([]string{}, attributes.classes...),
				Attrs:   append([]rs.Tuple2[string, rs.Option[string]]{}, attributes.attrs...),
			}
		} else {
			tag = &Tag_Heading{
				Level:   level,
				Id:      rs.None[string](),
				Classes: []string{},
				Attrs:   []rs.Tuple2[string, rs.Option[string]]{},
			}
		}
	case parseItemBody_FencedCodeBlock:
		tag = &Tag_CodeBlock{F0: &CodeBlockKind_Fenced{F0: allocs.takeCow(parseCowIndex(item.body.n))}}
	case parseItemBody_IndentCodeBlock:
		tag = &Tag_CodeBlock{F0: &CodeBlockKind_Indented{}}
	case parseItemBody_BlockQuote:
		tag = &Tag_BlockQuote{}
	case parseItemBody_List:
		c, listitemStart := item.body.c, item.body.u
		if c == '.' || c == ')' {
			tag = &Tag_List{F0: rs.Some(listitemStart)}
		} else {
			tag = &Tag_List{F0: rs.None[uint64]()}
		}
	case parseItemBody_ListItem:
		tag = &Tag_Item{}
	case parseItemBody_TableHead:
		tag = &Tag_TableHead{}
	case parseItemBody_TableCell:
		tag = &Tag_TableCell{}
	case parseItemBody_TableRow:
		tag = &Tag_TableRow{}
	case parseItemBody_Table:
		alignment := allocs.takeAlignment(parseAlignmentIndex(item.body.n))
		if alignment == nil {
			alignment = []Alignment{}
		}
		tag = &Tag_Table{F0: alignment}
	case parseItemBody_FootnoteDefinition:
		tag = &Tag_FootnoteDefinition{F0: allocs.takeCow(parseCowIndex(item.body.n))}
	case parseItemBody_MetadataBlock:
		tag = &Tag_MetadataBlock{F0: MetadataBlockKind(item.body.c)}
	default:
		rs.PanicAt(0, "unexpected item body "+item.body.debugString())
		panic(rs.Unreachable())
	}

	return &Event_Start{F0: tag}
}

// Next is `impl Iterator for Parser`. After it has returned None it keeps
// returning None (the Rust type is a FusedIterator).
func (parser *Parser) Next() rs.Option[Event] {
	curIx := parser.tree.cur()
	if curIx == treeNone {
		ix := parser.tree.pop()
		if ix == treeNone {
			return rs.None[Event]()
		}
		tagEnd := parseBodyToTagEnd(&parser.tree.nodes[ix].item.body)
		parser.tree.nextSibling(ix)
		return rs.Some[Event](&Event_End{F0: tagEnd})
	}
	if parser.tree.nodes[curIx].item.body.isInline() {
		parser.handleInline()
	}

	node := parser.tree.nodes[curIx]
	item := node.item
	event := parseItemToEvent(item, parser.text, &parser.allocs)
	if rs.Is[*Event_Start](event) {
		parser.tree.push()
	} else {
		parser.tree.nextSibling(curIx)
	}
	return rs.Some(event)
}

// All ranges over the remaining events; it is `for event in parser`.
func (parser *Parser) All() iter.Seq[Event] {
	return func(yield func(Event) bool) {
		for {
			event, ok := parser.Next().Get()
			if !ok || !yield(event) {
				return
			}
		}
	}
}

// usizeSaturatingSub is `usize::saturating_sub`.
func usizeSaturatingSub(a int, b int) int {
	if a < b {
		return 0
	}
	return a - b
}
