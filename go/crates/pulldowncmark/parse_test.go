// Port of pulldown-cmark 0.10.3 src/parse.rs

package pulldowncmark

import (
	"testing"

	"github.com/BrokkAi/mjolnir/go/rs"
)

// TODO: move these tests to tests/html.rs?

func parseTestParserWithExtensions(text string) *Parser {
	opts := OptionsEmpty()
	opts.Insert(OptionsEnableTables)
	opts.Insert(OptionsEnableFootnotes)
	opts.Insert(OptionsEnableStrikethrough)
	opts.Insert(OptionsEnableTasklists)

	return ParserNewExt(text, opts)
}

// parseTestCount is `Iterator::count`.
func parseTestCount(parser *Parser) int {
	count := 0
	for range parser.All() {
		count++
	}
	return count
}

// parseTestRanges is `.into_offset_iter().map(|(_ev, range)| range).collect()`.
func parseTestRanges(parser *Parser) []Range {
	var ranges []Range
	for _, span := range parser.IntoOffsetIter().All() {
		ranges = append(ranges, span)
	}
	return ranges
}

func TestParse_node_size(t *testing.T) {
	t.Skip("measures std::mem::size_of::<Node<Item>>(), Rust's memory layout of a tree node, which has no Go counterpart")
}

func TestParse_body_size(t *testing.T) {
	t.Skip("measures std::mem::size_of::<ItemBody>(), Rust's memory layout of an item body, which has no Go counterpart")
}

func TestParse_single_open_fish_bracket(t *testing.T) {
	// dont crash
	assertEq(t, 3, parseTestCount(ParserNew("<")))
}

func TestParse_lone_hashtag(t *testing.T) {
	// dont crash
	assertEq(t, 2, parseTestCount(ParserNew("#")))
}

func TestParse_lots_of_backslashes(t *testing.T) {
	// dont crash
	parseTestCount(ParserNew("\\\\\r\r"))
	parseTestCount(ParserNew("\\\r\r\\.\\\\\r\r\\.\\"))
}

func TestParse_issue_320(t *testing.T) {
	// dont crash
	parseTestCount(parseTestParserWithExtensions(":\r\t> |\r:\r\t> |\r"))
}

func TestParse_issue_319(t *testing.T) {
	// dont crash
	parseTestCount(parseTestParserWithExtensions("|\r-]([^|\r-]([^"))
	parseTestCount(parseTestParserWithExtensions("|\r\r=][^|\r\r=][^car"))
}

func TestParse_issue_303(t *testing.T) {
	// dont crash
	parseTestCount(parseTestParserWithExtensions("[^\r\ra]"))
	parseTestCount(parseTestParserWithExtensions("\r\r]Z[^\x00\r\r]Z[^\x00"))
}

func TestParse_issue_313(t *testing.T) {
	// dont crash
	parseTestCount(parseTestParserWithExtensions("*]0[^\r\r*]0[^"))
	parseTestCount(parseTestParserWithExtensions("[^\r> `][^\r> `][^\r> `]["))
}

func TestParse_issue_311(t *testing.T) {
	// dont crash
	parseTestCount(parseTestParserWithExtensions("\\\u000d-\u0009\\\u000d-\u0009"))
}

func TestParse_issue_283(t *testing.T) {
	input := string([]byte("\xf0\x9b\xb2\x9f<td:^\xf0\x9b\xb2\x9f"))
	// dont crash
	parseTestCount(parseTestParserWithExtensions(input))
}

func TestParse_issue_289(t *testing.T) {
	// dont crash
	parseTestCount(parseTestParserWithExtensions("> - \\\n> - "))
	parseTestCount(parseTestParserWithExtensions("- \n\n"))
}

func TestParse_issue_306(t *testing.T) {
	// dont crash
	parseTestCount(parseTestParserWithExtensions("*\r_<__*\r_<__*\r_<__*\r_<__"))
}

func TestParse_issue_305(t *testing.T) {
	// dont crash
	parseTestCount(parseTestParserWithExtensions("_6**6*_*"))
}

func TestParse_another_emphasis_panic(t *testing.T) {
	parseTestCount(parseTestParserWithExtensions("*__#_#__*"))
}

func TestParse_offset_iter(t *testing.T) {
	eventOffsets := parseTestRanges(ParserNew("*hello* world"))
	expectedOffsets := []Range{{0, 13}, {0, 7}, {1, 6}, {0, 7}, {7, 13}, {0, 13}}
	assertEq(t, expectedOffsets, eventOffsets)
}

func TestParse_reference_link_offsets(t *testing.T) {
	var found rs.Option[Range]
	for ev, span := range ParserNew("# H1\n[testing][Some reference]\n\n[Some reference]: https://github.com").IntoOffsetIter().All() {
		if start, ok := ev.(*Event_Start); ok {
			if link, ok := start.F0.(*Tag_Link); ok && link.LinkType == LinkType_Reference {
				found = rs.Some(span)
				break
			}
		}
	}
	span := found.Unwrap()
	assertEq(t, Range{5, 30}, span)
}

func TestParse_footnote_offsets(t *testing.T) {
	var found rs.Option[Range]
	for ev, span := range parseTestParserWithExtensions("Testing this[^1] out.\n\n[^1]: Footnote.").IntoOffsetIter().All() {
		if _, ok := ev.(*Event_FootnoteReference); ok {
			found = rs.Some(span)
			break
		}
	}
	span := found.Unwrap()
	assertEq(t, Range{12, 16}, span)
}

func TestParse_table_offset(t *testing.T) {
	markdown := "a\n\nTesting|This|Outtt\n--|:--:|--:\nSome Data|Other data|asdf"
	eventOffset := parseTestRanges(parseTestParserWithExtensions(markdown))[3]
	expectedOffset := Range{3, 59}
	assertEq(t, expectedOffset, eventOffset)
}

func TestParse_table_cell_span(t *testing.T) {
	markdown := "a|b|c\n--|--|--\na|  |c"
	var cells []Range
	for ev, span := range parseTestParserWithExtensions(markdown).IntoOffsetIter().All() {
		if start, ok := ev.(*Event_Start); ok {
			if _, ok := start.F0.(*Tag_TableCell); ok {
				cells = append(cells, span)
			}
		}
	}
	eventOffset := cells[4]
	expectedOffsetStart := len("a|b|c\n--|--|--\na|")
	assertEq(t, Range{expectedOffsetStart, expectedOffsetStart + 2}, eventOffset)
}

func TestParse_offset_iter_issue_378(t *testing.T) {
	eventOffsets := parseTestRanges(ParserNew("a [b](c) d"))
	expectedOffsets := []Range{{0, 10}, {0, 2}, {2, 8}, {3, 4}, {2, 8}, {8, 10}, {0, 10}}
	assertEq(t, expectedOffsets, eventOffsets)
}

func TestParse_offset_iter_issue_404(t *testing.T) {
	eventOffsets := parseTestRanges(ParserNew("###\n"))
	expectedOffsets := []Range{{0, 4}, {0, 4}}
	assertEq(t, expectedOffsets, eventOffsets)
}

// FIXME: add this one regression suite
func TestParse_link_def_at_eof(t *testing.T) {
	testStr := "[My site][world]\n\n[world]: https://vincentprouillet.com"
	expected := "<p><a href=\"https://vincentprouillet.com\">My site</a></p>\n"

	buf := ""
	HtmlPushHtml(&buf, ParserNew(testStr))
	assertEq(t, expected, buf)
}

func TestParse_no_footnote_refs_without_option(t *testing.T) {
	testStr := "a [^a]\n\n[^a]: yolo"
	expected := "<p>a <a href=\"yolo\">^a</a></p>\n"

	buf := ""
	HtmlPushHtml(&buf, ParserNew(testStr))
	assertEq(t, expected, buf)
}

func TestParse_ref_def_at_eof(t *testing.T) {
	testStr := "[test]:\\"
	expected := ""

	buf := ""
	HtmlPushHtml(&buf, ParserNew(testStr))
	assertEq(t, expected, buf)
}

func TestParse_ref_def_cr_lf(t *testing.T) {
	testStr := "[a]: /u\r\n\n[a]"
	expected := "<p><a href=\"/u\">a</a></p>\n"

	buf := ""
	HtmlPushHtml(&buf, ParserNew(testStr))
	assertEq(t, expected, buf)
}

func TestParse_no_dest_refdef(t *testing.T) {
	testStr := "[a]:"
	expected := "<p>[a]:</p>\n"

	buf := ""
	HtmlPushHtml(&buf, ParserNew(testStr))
	assertEq(t, expected, buf)
}

func TestParse_broken_links_called_only_once(t *testing.T) {
	for _, c := range []struct {
		markdown string
		expected int
	}{
		{"See also [`g()`][crate::g].", 1},
		{"See also [`g()`][crate::g][].", 1},
		{"[brokenlink1] some other node [brokenlink2]", 2},
	} {
		markdown, expected := c.markdown, c.expected
		timesCalled := 0
		callback := BrokenLinkCallbackFunc(func(_ BrokenLink) rs.Option[rs.Tuple2[string, string]] {
			timesCalled += 1
			return rs.None[rs.Tuple2[string, string]]()
		})
		parser := ParserNewWithBrokenLinkCallback(markdown, OptionsEmpty(), rs.Some[BrokenLinkCallback](callback))
		for range parser.All() {
		}
		assertEq(t, timesCalled, expected)
	}
}

func TestParse_simple_broken_link_callback(t *testing.T) {
	testStr := "This is a link w/o def: [hello][world]"
	callback := BrokenLinkCallbackFunc(func(brokenLink BrokenLink) rs.Option[rs.Tuple2[string, string]] {
		assertEq(t, "world", brokenLink.Reference)
		assertEq(t, testStr[brokenLink.Span.Start:brokenLink.Span.End], "[hello][world]")
		url := "YOLO"
		title := "SWAG"
		return rs.Some(rs.T2(url, title))
	})
	parser := ParserNewWithBrokenLinkCallback(testStr, OptionsEmpty(), rs.Some[BrokenLinkCallback](callback))
	linkTagCount := 0
	for event := range parser.All() {
		start, ok := event.(*Event_Start)
		if !ok {
			continue
		}
		link, ok := start.F0.(*Tag_Link)
		if !ok {
			continue
		}
		typ, url, title, id := link.LinkType, link.DestUrl, link.Title, link.Id
		linkTagCount += 1
		assertEq(t, typ, LinkType_ReferenceUnknown)
		assertEq(t, url, "YOLO")
		assertEq(t, title, "SWAG")
		assertEq(t, id, "world")
	}
	assert(t, linkTagCount > 0)
}

func TestParse_code_block_kind_check_fenced(t *testing.T) {
	parser := ParserNew("hello\n```test\ntadam\n```")
	found := 0
	for ev := range parser.IntoOffsetIter().All() {
		if start, ok := ev.(*Event_Start); ok {
			if codeBlock, ok := start.F0.(*Tag_CodeBlock); ok {
				if fenced, ok := codeBlock.F0.(*CodeBlockKind_Fenced); ok {
					syntax := fenced.F0
					assertEq(t, syntax, "test")
					found += 1
				}
			}
		}
	}
	assertEq(t, found, 1)
}

func TestParse_code_block_kind_check_indented(t *testing.T) {
	parser := ParserNew("hello\n\n    ```test\n    tadam\nhello")
	found := 0
	for ev := range parser.IntoOffsetIter().All() {
		if start, ok := ev.(*Event_Start); ok {
			if codeBlock, ok := start.F0.(*Tag_CodeBlock); ok {
				if _, ok := codeBlock.F0.(*CodeBlockKind_Indented); ok {
					found += 1
				}
			}
		}
	}
	assertEq(t, found, 1)
}

func TestParse_ref_defs(t *testing.T) {
	input := "[a B c]: http://example.com\n" +
		"[another]: https://google.com\n" +
		"\n" +
		"text\n" +
		"\n" +
		"[final ONE]: http://wikipedia.org\n"
	parser := ParserNew(input)

	assert(t, parser.ReferenceDefinitions().Get("a b c").IsSome())
	assert(t, parser.ReferenceDefinitions().Get("nope").IsNone())

	if _, ok := parser.Next().Get(); ok {
		// testing keys with shorter lifetimes than parser and its input
		s := "final one"
		linkDef := parser.ReferenceDefinitions().Get(s).Unwrap()
		span := input[linkDef.Span.Start:linkDef.Span.End]
		assertEq(t, span, "[final ONE]: http://wikipedia.org")
	}
}

func TestParse_common_lifetime_patterns_allowed(t *testing.T) {
	temporaryStr := "xyz"

	// NOTE: this is a limitation of Rust, it doesn't allow putting lifetime parameters on the closure itself.
	// Hack it by attaching the lifetime to the test function instead.
	// TODO: why is the `'b` lifetime required at all? Changing it to `'_` breaks things :(
	closure := BrokenLinkCallbackFunc(func(link BrokenLink) rs.Option[rs.Tuple2[string, string]] {
		return rs.Some(rs.T2("#", link.Reference))
	})

	function := func(link BrokenLink) rs.Option[rs.Tuple2[string, string]] {
		return rs.Some(rs.T2("#", link.Reference))
	}

	for range ParserNewWithBrokenLinkCallback(
		"static lifetime",
		OptionsEmpty(),
		rs.Some[BrokenLinkCallback](closure),
	).All() {
	}
	/* This fails to compile. Because the closure can't say `for <'a> fn(BrokenLink<'a>) ->
	 * CowStr<'a>` and has to use the enclosing `'b` lifetime parameter, `temporary_str` lives
	 * shorter than `'b`. I think this is unlikely to occur in real life, and if it does, the
	 * fix is simple: move it out to a function that allows annotating the lifetimes.
	 */
	//for _ in Parser::new_with_broken_link_callback(&temporary_str, Options::empty(), Some(&mut callback)) {
	//}

	for range ParserNewWithBrokenLinkCallback(
		"static lifetime",
		OptionsEmpty(),
		rs.Some[BrokenLinkCallback](BrokenLinkCallbackFunc(function)),
	).All() {
	}
	for range ParserNewWithBrokenLinkCallback(
		temporaryStr,
		OptionsEmpty(),
		rs.Some[BrokenLinkCallback](BrokenLinkCallbackFunc(function)),
	).All() {
	}
}
