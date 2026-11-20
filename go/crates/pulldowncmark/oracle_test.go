package pulldowncmark

import (
	"encoding/json"
	"go/ast"
	"go/parser"
	"go/token"
	"os"
	"path/filepath"
	"sort"
	"strconv"
	"strings"
	"testing"
	"unicode"

	"github.com/BrokkAi/mjolnir/go/rs"
	"github.com/BrokkAi/mjolnir/go/rs/rstest/oracle"
	"github.com/BrokkAi/mjolnir/go/rs/strs"
)

// The differential tests hold the port against pulldown-cmark 0.10.3 itself,
// through the oracle module go/port/oracle/src/pulldown_cmark_oracle.rs.
//
// Both sides write what a caller can observe as the same JSON text: every
// event of the offset iterator with its source range, the reference
// definitions, and whether the plain iterator yields the same events. The
// `events` operation compares that text; the `digest` operation compares a
// hash of it for several option sets at once, which is what lets a corpus of
// more than ten thousand documents stay small on disk.
//
//	MJ_ORACLE=record go/port/gotest.sh ./go/crates/pulldowncmark/   rewrite the corpora
//	MJ_ORACLE=live go/port/gotest.sh ./go/crates/pulldowncmark/     compare with Rust again;
//	                                    the generated documents are then compared event by
//	                                    event instead of by digest
//
// MJ_PULLDOWN_CMARK_FUZZ=<count>[:<seed>] adds that many more generated
// documents to a live run.

// withoutDebugAssertions switches the crate's debug assertions off for the
// rest of the test. The oracle is a release build of the Rust crate, in which
// they are off; the ported tests of the crate run with them on, as `cargo
// test` does. Where pulldown-cmark breaks one of its own debug assertions, a
// release build goes on and fails later (or not at all), and the comparison
// must see what the release build does.
func withoutDebugAssertions(t *testing.T) {
	saved := debugAssertions
	debugAssertions = false
	t.Cleanup(func() { debugAssertions = saved })
}

// Option sets.
const (
	// optionsMjolnir is what mj-chat renders agent messages with.
	optionsMjolnir = OptionsEnableStrikethrough | OptionsEnableTables
	// optionsAllGfmFootnotes is every option except the old footnote syntax,
	// which replaces the GitHub one when it is set.
	optionsAllGfmFootnotes = optionsAll &^ (1 << 9)
	// optionsAll is Options::all(): everything, with the old footnote syntax.
	optionsAll = OptionsEnableTables | OptionsEnableFootnotes | OptionsEnableStrikethrough |
		OptionsEnableTasklists | OptionsEnableSmartPunctuation | OptionsEnableHeadingAttributes |
		OptionsEnableYamlStyleMetadataBlocks | OptionsEnablePlusesDelimitedMetadataBlocks |
		OptionsEnableOldFootnotes
	optionsMetadataBlocks = OptionsEnableYamlStyleMetadataBlocks | OptionsEnablePlusesDelimitedMetadataBlocks
)

// digestOptionSets are the option sets every generated document is parsed
// with: none, each combination of the options Mjolnir enables, and all of
// them with either footnote syntax.
var digestOptionSets = []Options{
	OptionsEmpty(),
	OptionsEnableTables,
	OptionsEnableStrikethrough,
	optionsMjolnir,
	optionsAllGfmFootnotes,
	optionsAll,
}

// metadataBlockMayNotReturn reports whether text has a line that
// pulldown-cmark 0.10.3 may never return from when metadata blocks are
// enabled: three `-` or `+` after a space or a tab, with only whitespace
// after them.
//
// Such a line can pass scan_metadata_block with an indent of one to three
// columns. parse_metadata_block answers an indented block with `return 0`,
// which FirstPass::run takes as the offset to continue from, so the first
// pass starts the document over, forever. The port does the same. The
// documents this reports are compared without the metadata options.
func metadataBlockMayNotReturn(text string) bool {
	for i := 1; i+3 <= len(text); i++ {
		c := text[i]
		if (c != '-' && c != '+') || text[i+1] != c || text[i+2] != c {
			continue
		}
		if text[i-1] != ' ' && text[i-1] != '\t' {
			continue
		}
		j := i + 3
		for j < len(text) && text[j] != '\n' && strs.IsAsciiWhitespace(text[j]) {
			j++
		}
		if j == len(text) || text[j] == '\n' {
			return true
		}
	}
	return false
}

// optionSetsFor returns the option sets a document is compared with.
func optionSetsFor(text string) []Options {
	if !metadataBlockMayNotReturn(text) {
		return digestOptionSets
	}
	sets := make([]Options, len(digestOptionSets))
	for i, set := range digestOptionSets {
		sets[i] = set &^ optionsMetadataBlocks
	}
	return sets
}

// --- the JSON both sides write -------------------------------------------

// appendJsonString appends s as serde_json writes a string.
func appendJsonString(out []byte, s string) []byte {
	const hex = "0123456789abcdef"
	out = append(out, '"')
	start := 0
	for i := 0; i < len(s); i++ {
		c := s[i]
		var escape string
		switch {
		case c == '"':
			escape = `\"`
		case c == '\\':
			escape = `\\`
		case c == '\b':
			escape = `\b`
		case c == '\f':
			escape = `\f`
		case c == '\n':
			escape = `\n`
		case c == '\r':
			escape = `\r`
		case c == '\t':
			escape = `\t`
		case c < 0x20:
			escape = `\u00` + string([]byte{hex[c>>4], hex[c&0xF]})
		default:
			continue
		}
		out = append(out, s[start:i]...)
		out = append(out, escape...)
		start = i + 1
	}
	out = append(out, s[start:]...)
	return append(out, '"')
}

func appendJsonOptionString(out []byte, s rs.Option[string]) []byte {
	if value, ok := s.Get(); ok {
		return appendJsonString(out, value)
	}
	return append(out, "null"...)
}

func linkTypeName(linkType LinkType) string {
	switch linkType {
	case LinkType_Inline:
		return "Inline"
	case LinkType_Reference:
		return "Reference"
	case LinkType_ReferenceUnknown:
		return "ReferenceUnknown"
	case LinkType_Collapsed:
		return "Collapsed"
	case LinkType_CollapsedUnknown:
		return "CollapsedUnknown"
	case LinkType_Shortcut:
		return "Shortcut"
	case LinkType_ShortcutUnknown:
		return "ShortcutUnknown"
	case LinkType_Autolink:
		return "Autolink"
	case LinkType_Email:
		return "Email"
	default:
		panic(rs.Unreachable(linkType))
	}
}

func alignmentName(alignment Alignment) string {
	switch alignment {
	case Alignment_None:
		return "None"
	case Alignment_Left:
		return "Left"
	case Alignment_Center:
		return "Center"
	case Alignment_Right:
		return "Right"
	default:
		panic(rs.Unreachable(alignment))
	}
}

func metadataBlockKindName(kind MetadataBlockKind) string {
	switch kind {
	case MetadataBlockKind_YamlStyle:
		return "YamlStyle"
	case MetadataBlockKind_PlusesStyle:
		return "PlusesStyle"
	default:
		panic(rs.Unreachable(kind))
	}
}

// appendEventJson appends one event as the oracle writes it: an array of the
// source range (when there is one), the event's name and its fields.
func appendEventJson(out []byte, event Event, span rs.Option[Range]) []byte {
	out = append(out, '[')
	if span, ok := span.Get(); ok {
		out = strconv.AppendInt(out, int64(span.Start), 10)
		out = append(out, ',')
		out = strconv.AppendInt(out, int64(span.End), 10)
		out = append(out, ',')
	}
	text := func(name string, value string) {
		out = append(out, '"')
		out = append(out, name...)
		out = append(out, `",`...)
		out = appendJsonString(out, value)
	}
	switch event := event.(type) {
	case *Event_Start:
		out = append(out, `"Start",`...)
		out = appendTagJson(out, event.F0)
	case *Event_End:
		out = append(out, `"End",`...)
		out = appendTagEndJson(out, event.F0)
	case *Event_Text:
		text("Text", event.F0)
	case *Event_Code:
		text("Code", event.F0)
	case *Event_Html:
		text("Html", event.F0)
	case *Event_InlineHtml:
		text("InlineHtml", event.F0)
	case *Event_FootnoteReference:
		text("FootnoteReference", event.F0)
	case *Event_SoftBreak:
		out = append(out, `"SoftBreak"`...)
	case *Event_HardBreak:
		out = append(out, `"HardBreak"`...)
	case *Event_Rule:
		out = append(out, `"Rule"`...)
	case *Event_TaskListMarker:
		out = append(out, `"TaskListMarker",`...)
		out = strconv.AppendBool(out, event.F0)
	default:
		panic(rs.Unreachable(event))
	}
	return append(out, ']')
}

func appendTagJson(out []byte, tag Tag) []byte {
	name := func(name string) {
		out = append(out, '"')
		out = append(out, name...)
		out = append(out, '"')
	}
	link := func(kind string, linkType LinkType, destUrl string, title string, id string) {
		name(kind)
		out = append(out, ',')
		name(linkTypeName(linkType))
		out = append(out, ',')
		out = appendJsonString(out, destUrl)
		out = append(out, ',')
		out = appendJsonString(out, title)
		out = append(out, ',')
		out = appendJsonString(out, id)
	}
	switch tag := tag.(type) {
	case *Tag_Paragraph:
		name("Paragraph")
	case *Tag_Heading:
		name("Heading")
		out = append(out, ',')
		name(tag.Level.String())
		out = append(out, ',')
		out = appendJsonOptionString(out, tag.Id)
		out = append(out, ",["...)
		for i, class := range tag.Classes {
			if i > 0 {
				out = append(out, ',')
			}
			out = appendJsonString(out, class)
		}
		out = append(out, "],["...)
		for i, attr := range tag.Attrs {
			if i > 0 {
				out = append(out, ',')
			}
			out = append(out, '[')
			out = appendJsonString(out, attr.F0)
			out = append(out, ',')
			out = appendJsonOptionString(out, attr.F1)
			out = append(out, ']')
		}
		out = append(out, ']')
	case *Tag_BlockQuote:
		name("BlockQuote")
	case *Tag_CodeBlock:
		name("CodeBlock")
		out = append(out, ',')
		switch kind := tag.F0.(type) {
		case *CodeBlockKind_Indented:
			name("Indented")
		case *CodeBlockKind_Fenced:
			name("Fenced")
			out = append(out, ',')
			out = appendJsonString(out, kind.F0)
		default:
			panic(rs.Unreachable(kind))
		}
	case *Tag_HtmlBlock:
		name("HtmlBlock")
	case *Tag_List:
		name("List")
		out = append(out, ',')
		if start, ok := tag.F0.Get(); ok {
			out = strconv.AppendUint(out, start, 10)
		} else {
			out = append(out, "null"...)
		}
	case *Tag_Item:
		name("Item")
	case *Tag_FootnoteDefinition:
		name("FootnoteDefinition")
		out = append(out, ',')
		out = appendJsonString(out, tag.F0)
	case *Tag_Table:
		name("Table")
		out = append(out, ",["...)
		for i, alignment := range tag.F0 {
			if i > 0 {
				out = append(out, ',')
			}
			name(alignmentName(alignment))
		}
		out = append(out, ']')
	case *Tag_TableHead:
		name("TableHead")
	case *Tag_TableRow:
		name("TableRow")
	case *Tag_TableCell:
		name("TableCell")
	case *Tag_Emphasis:
		name("Emphasis")
	case *Tag_Strong:
		name("Strong")
	case *Tag_Strikethrough:
		name("Strikethrough")
	case *Tag_Link:
		link("Link", tag.LinkType, tag.DestUrl, tag.Title, tag.Id)
	case *Tag_Image:
		link("Image", tag.LinkType, tag.DestUrl, tag.Title, tag.Id)
	case *Tag_MetadataBlock:
		name("MetadataBlock")
		out = append(out, ',')
		name(metadataBlockKindName(tag.F0))
	default:
		panic(rs.Unreachable(tag))
	}
	return out
}

func appendTagEndJson(out []byte, tag TagEnd) []byte {
	name := func(name string) {
		out = append(out, '"')
		out = append(out, name...)
		out = append(out, '"')
	}
	switch tag := tag.(type) {
	case *TagEnd_Paragraph:
		name("Paragraph")
	case *TagEnd_Heading:
		name("Heading")
		out = append(out, ',')
		name(tag.F0.String())
	case *TagEnd_BlockQuote:
		name("BlockQuote")
	case *TagEnd_CodeBlock:
		name("CodeBlock")
	case *TagEnd_HtmlBlock:
		name("HtmlBlock")
	case *TagEnd_List:
		name("List")
		out = append(out, ',')
		out = strconv.AppendBool(out, tag.F0)
	case *TagEnd_Item:
		name("Item")
	case *TagEnd_FootnoteDefinition:
		name("FootnoteDefinition")
	case *TagEnd_Table:
		name("Table")
	case *TagEnd_TableHead:
		name("TableHead")
	case *TagEnd_TableRow:
		name("TableRow")
	case *TagEnd_TableCell:
		name("TableCell")
	case *TagEnd_Emphasis:
		name("Emphasis")
	case *TagEnd_Strong:
		name("Strong")
	case *TagEnd_Strikethrough:
		name("Strikethrough")
	case *TagEnd_Link:
		name("Link")
	case *TagEnd_Image:
		name("Image")
	case *TagEnd_MetadataBlock:
		name("MetadataBlock")
		out = append(out, ',')
		name(metadataBlockKindName(tag.F0))
	default:
		panic(rs.Unreachable(tag))
	}
	return out
}

// appendRefdefsJson appends the reference definitions, sorted by label as
// the oracle sorts them (Rust's map has no order of its own).
func appendRefdefsJson(out []byte, refdefs *RefDefs) []byte {
	type entry struct {
		label string
		def   *LinkDef
	}
	var entries []entry
	for label, def := range refdefs.Iter() {
		entries = append(entries, entry{label, def})
	}
	sort.SliceStable(entries, func(i, j int) bool { return entries[i].label < entries[j].label })
	out = append(out, '[')
	for i, e := range entries {
		if i > 0 {
			out = append(out, ',')
		}
		out = append(out, '[')
		out = appendJsonString(out, e.label)
		out = append(out, ',')
		out = appendJsonString(out, e.def.Dest)
		out = append(out, ',')
		out = appendJsonOptionString(out, e.def.Title)
		out = append(out, ',')
		out = strconv.AppendInt(out, int64(e.def.Span.Start), 10)
		out = append(out, ',')
		out = strconv.AppendInt(out, int64(e.def.Span.End), 10)
		out = append(out, ']')
	}
	return append(out, ']')
}

// parseReply is the oracle's reply to `events` without a callback, as text.
func parseReply(text string, options Options) []byte {
	out := make([]byte, 0, 256+8*len(text))
	out = append(out, `{"events":[`...)
	offsetIter := ParserNewExt(text, options).IntoOffsetIter()
	// The events without their ranges, to hold against the plain iterator.
	var offsetEvents []byte
	first := true
	for event, span := range offsetIter.All() {
		if !first {
			out = append(out, ',')
		}
		first = false
		out = appendEventJson(out, event, rs.Some(span))
		offsetEvents = appendEventJson(offsetEvents, event, rs.None[Range]())
	}
	out = append(out, `],"refdefs":`...)
	out = appendRefdefsJson(out, offsetIter.ReferenceDefinitions())

	var plainEvents []byte
	for event := range ParserNewExt(text, options).All() {
		plainEvents = appendEventJson(plainEvents, event, rs.None[Range]())
	}
	out = append(out, `,"same_plain":`...)
	out = strconv.AppendBool(out, string(plainEvents) == string(offsetEvents))
	return append(out, '}')
}

// parseWithCallbackReply is the oracle's reply to `events` with
// `"callback": true`.
func parseWithCallbackReply(text string, options Options) []byte {
	var calls []byte
	callback := BrokenLinkCallbackFunc(func(link BrokenLink) rs.Option[rs.Tuple2[string, string]] {
		if len(calls) > 0 {
			calls = append(calls, ',')
		}
		calls = append(calls, '[')
		calls = strconv.AppendInt(calls, int64(link.Span.Start), 10)
		calls = append(calls, ',')
		calls = strconv.AppendInt(calls, int64(link.Span.End), 10)
		calls = append(calls, ',')
		calls = appendJsonString(calls, linkTypeName(link.LinkType))
		calls = append(calls, ',')
		calls = appendJsonString(calls, link.Reference)
		calls = append(calls, ']')
		// References that start with `x` stay broken, so that both answers
		// of a callback are exercised.
		if strings.HasPrefix(link.Reference, "x") || strings.HasPrefix(link.Reference, "X") {
			return rs.None[rs.Tuple2[string, string]]()
		}
		return rs.Some(rs.T2("/broken/"+link.Reference, linkTypeName(link.LinkType)))
	})
	offsetIter := ParserNewWithBrokenLinkCallback(text, options, rs.Some[BrokenLinkCallback](callback)).IntoOffsetIter()
	out := []byte(`{"events":[`)
	first := true
	for event, span := range offsetIter.All() {
		if !first {
			out = append(out, ',')
		}
		first = false
		out = appendEventJson(out, event, rs.Some(span))
	}
	out = append(out, `],"refdefs":`...)
	out = appendRefdefsJson(out, offsetIter.ReferenceDefinitions())
	out = append(out, `,"broken":[`...)
	out = append(out, calls...)
	return append(out, "]}"...)
}

// fnv1a32 is FNV-1a over 64 bits, folded to 32.
func fnv1a32(bytes []byte) uint32 {
	hash := uint64(0xcbf29ce484222325)
	for _, b := range bytes {
		hash ^= uint64(b)
		hash *= 0x100000001b3
	}
	return uint32(hash>>32) ^ uint32(hash)
}

// digestOf is one entry of the oracle's reply to `digest`: the hash of the
// parse's JSON text, or the panic the parse ended in.
func digestOf(text string, options Options) (digest string) {
	defer func() {
		if recovered := recover(); recovered != nil {
			digest = "panic: " + rs.PanicInfoOf(recovered).Message
		}
	}()
	const hex = "0123456789abcdef"
	hash := fnv1a32(parseReply(text, options))
	var out [8]byte
	for i := range out {
		out[i] = hex[(hash>>(28-4*i))&0xF]
	}
	return string(out[:])
}

// digestLine is the digests of one document, one for each option set,
// separated by spaces.
func digestLine(text string, sets []uint32) string {
	digests := make([]string, len(sets))
	for j, bits := range sets {
		digests[j] = digestOf(text, OptionsFromBitsTruncate(bits))
	}
	return strings.Join(digests, " ")
}

// codepointsDocument is the document of a `codepoints` request: template
// once for each character from from up to to, with `{c}` replaced by the
// character. Surrogates are not characters and are skipped, as
// char::from_u32 skips them in the oracle.
func codepointsDocument(template string, from int, to int) string {
	var out strings.Builder
	for code := from; code < to; code++ {
		if (0xD800 <= code && code <= 0xDFFF) || code > unicode.MaxRune {
			continue
		}
		out.WriteString(strings.ReplaceAll(template, "{c}", string(rune(code))))
	}
	return out.String()
}

// oracleRequest is a request to the oracle module.
type oracleRequest struct {
	Op       string   `json:"op"`
	Text     string   `json:"text,omitempty"`
	Texts    []string `json:"texts,omitempty"`
	Options  any      `json:"options"`
	Callback bool     `json:"callback,omitempty"`
	// Template, From and To are the `codepoints` operation's document.
	Template string `json:"template,omitempty"`
	From     int    `json:"from,omitempty"`
	To       int    `json:"to,omitempty"`
	// Bits are the option bits of the `options_debug` operation.
	Bits []uint32 `json:"bits,omitempty"`
}

// eventsRequest asks for the events of one document under one option set.
func eventsRequest(text string, options Options) oracleRequest {
	return oracleRequest{Op: "events", Text: text, Options: options.Bits()}
}

// answer is the Go side of every operation.
func answer(request json.RawMessage) any {
	var decoded struct {
		Op       string          `json:"op"`
		Text     string          `json:"text"`
		Texts    []string        `json:"texts"`
		Options  json.RawMessage `json:"options"`
		Callback bool            `json:"callback"`
		Template string          `json:"template"`
		From     int             `json:"from"`
		To       int             `json:"to"`
		Bits     []uint32        `json:"bits"`
	}
	if err := json.Unmarshal(request, &decoded); err != nil {
		panic(err)
	}
	switch decoded.Op {
	case "events":
		var bits uint32
		if err := json.Unmarshal(decoded.Options, &bits); err != nil {
			panic(err)
		}
		options := OptionsFromBitsTruncate(bits)
		return oracle.Catch(func() any {
			if decoded.Callback {
				return json.RawMessage(parseWithCallbackReply(decoded.Text, options))
			}
			return json.RawMessage(parseReply(decoded.Text, options))
		})
	case "digest":
		var sets []uint32
		if err := json.Unmarshal(decoded.Options, &sets); err != nil {
			panic(err)
		}
		replies := make([]string, len(decoded.Texts))
		for i, text := range decoded.Texts {
			replies[i] = digestLine(text, sets)
		}
		return map[string]any{"d": replies}
	case "codepoints":
		var sets []uint32
		if err := json.Unmarshal(decoded.Options, &sets); err != nil {
			panic(err)
		}
		return map[string]any{"d": digestLine(codepointsDocument(decoded.Template, decoded.From, decoded.To), sets)}
	case "debug":
		var bits uint32
		if err := json.Unmarshal(decoded.Options, &bits); err != nil {
			panic(err)
		}
		options := OptionsFromBitsTruncate(bits)
		return oracle.Catch(func() any {
			events := []string{}
			for event, span := range ParserNewExt(decoded.Text, options).IntoOffsetIter().All() {
				if !eventHoldsString(event) {
					events = append(events, rs.DebugString(rs.T2(event, span)))
				}
			}
			return map[string]any{
				"events":         events,
				"options":        rs.DebugString(options),
				"options_pretty": rs.PrettyDebugString(options),
			}
		})
	case "options_debug":
		debug := make([]string, len(decoded.Bits))
		pretty := make([]string, len(decoded.Bits))
		for i, bits := range decoded.Bits {
			debug[i] = rs.DebugString(OptionsFromBitsRetain(bits))
			pretty[i] = rs.PrettyDebugString(OptionsFromBitsRetain(bits))
		}
		return map[string]any{"debug": debug, "pretty": pretty}
	default:
		panic("unknown op " + decoded.Op)
	}
}

// --- the specification examples as oracle inputs ---------------------------

// suiteInput is the Markdown of one ported specification test, and the
// options that test parses it with.
type suiteInput struct {
	name    string
	text    string
	options Options
}

// suiteInputs reads the inputs of the specification tests out of the Go
// files they were ported to (it_lib_suite_*_test.go), so that the oracle
// corpus holds the same documents without a second copy of them.
func suiteInputs(t testing.TB) []suiteInput {
	t.Helper()
	files, err := filepath.Glob("it_lib_suite_*_test.go")
	if err != nil || len(files) == 0 {
		t.Fatalf("no ported suite files beside the test: %v", err)
	}
	sort.Strings(files)
	var inputs []suiteInput
	for _, file := range files {
		parsed, err := parser.ParseFile(token.NewFileSet(), file, nil, 0)
		if err != nil {
			t.Fatal(err)
		}
		for _, declaration := range parsed.Decls {
			function, ok := declaration.(*ast.FuncDecl)
			if !ok || !strings.HasPrefix(function.Name.Name, "TestItLibSuite") {
				continue
			}
			input := suiteInput{name: function.Name.Name}
			found := false
			for _, statement := range function.Body.List {
				switch statement := statement.(type) {
				case *ast.AssignStmt:
					if name, ok := statement.Lhs[0].(*ast.Ident); ok && name.Name == "original" {
						input.text = stringConstant(t, statement.Rhs[0])
						found = true
					}
				case *ast.ExprStmt:
					call, ok := statement.X.(*ast.CallExpr)
					if !ok || len(call.Args) != 6 {
						continue
					}
					flag := func(index int) bool { return call.Args[index].(*ast.Ident).Name == "true" }
					input.options = suiteOptions(flag(3), flag(4), flag(5))
				}
			}
			if !found {
				t.Fatalf("%s: %s has no `original`", file, function.Name.Name)
			}
			inputs = append(inputs, input)
		}
	}
	return inputs
}

// stringConstant evaluates a string literal or a concatenation of them.
func stringConstant(t testing.TB, expression ast.Expr) string {
	t.Helper()
	switch expression := expression.(type) {
	case *ast.BasicLit:
		value, err := strconv.Unquote(expression.Value)
		if err != nil {
			t.Fatal(err)
		}
		return value
	case *ast.BinaryExpr:
		return stringConstant(t, expression.X) + stringConstant(t, expression.Y)
	default:
		t.Fatalf("not a string constant: %T", expression)
		return ""
	}
}

// suiteOptions is the option set testMarkdownHtml builds from its flags.
func suiteOptions(smartPunct bool, metadataBlocks bool, oldFootnotes bool) Options {
	options := OptionsEnableTables | OptionsEnableStrikethrough | OptionsEnableTasklists | OptionsEnableHeadingAttributes
	if oldFootnotes {
		options |= OptionsEnableOldFootnotes
	} else {
		options |= OptionsEnableFootnotes
	}
	if metadataBlocks {
		options |= optionsMetadataBlocks
	}
	if smartPunct {
		options |= OptionsEnableSmartPunctuation
	}
	return options
}

// TestDiffSpecAgainstRust compares, event by event, every example of the
// CommonMark and extension specifications: with the options Mjolnir uses,
// and with the options the crate's own test gives the example.
func TestDiffSpecAgainstRust(t *testing.T) {
	withoutDebugAssertions(t)
	inputs := suiteInputs(t)
	oracle.Check(t, "pulldown_cmark", "spec", func(yield func(any)) {
		for _, input := range inputs {
			yield(eventsRequest(input.text, optionsMjolnir))
			yield(eventsRequest(input.text, input.options))
		}
	}, answer)
}

// TestDiffGeneratedAgainstRust compares the generated corpus (corpus_test.go)
// under every option set in digestOptionSets.
func TestDiffGeneratedAgainstRust(t *testing.T) {
	withoutDebugAssertions(t)
	inputs := suiteInputs(t)
	live := os.Getenv("MJ_ORACLE") == "live"
	oracle.Check(t, "pulldown_cmark", "generated", func(yield func(any)) {
		documents := corpusDocuments(inputs)
		if live {
			documents = append(documents, corpusFuzzDocuments(t, inputs)...)
			// A live run has Rust at hand: compare the events themselves,
			// so that a difference shows where it is.
			for _, text := range documents {
				for _, options := range optionSetsFor(text) {
					yield(eventsRequest(text, options))
				}
			}
			return
		}
		yieldDigests(yield, documents)
	}, answer)
}

// yieldDigests asks for the digests of documents, several documents to a
// request, and apart from the others those that cannot be parsed with
// metadata blocks.
func yieldDigests(yield func(any), documents []string) {
	const batch = 16
	bits := func(sets []Options) []uint32 {
		out := make([]uint32, len(sets))
		for i, set := range sets {
			out[i] = set.Bits()
		}
		return out
	}
	var plain, withoutMetadata []string
	flush := func(texts *[]string, sets []Options, force bool) {
		for len(*texts) >= batch || (force && len(*texts) > 0) {
			n := min(batch, len(*texts))
			yield(oracleRequest{Op: "digest", Texts: append([]string(nil), (*texts)[:n]...), Options: bits(sets)})
			*texts = (*texts)[n:]
		}
	}
	for _, text := range documents {
		if metadataBlockMayNotReturn(text) {
			withoutMetadata = append(withoutMetadata, text)
		} else {
			plain = append(plain, text)
		}
		flush(&plain, digestOptionSets, false)
		flush(&withoutMetadata, optionSetsFor(" ---"), false)
	}
	flush(&plain, digestOptionSets, true)
	flush(&withoutMetadata, optionSetsFor(" ---"), true)
}

// TestDiffCallbackAgainstRust compares parsers that were given a broken link
// callback: which references the callback is asked about, with which span
// and link type, and the links its answers become.
func TestDiffCallbackAgainstRust(t *testing.T) {
	withoutDebugAssertions(t)
	oracle.Check(t, "pulldown_cmark", "callback", func(yield func(any)) {
		for _, text := range corpusReferenceDocuments() {
			for _, options := range []Options{OptionsEmpty(), optionsMjolnir, optionsAllGfmFootnotes, optionsAll} {
				if metadataBlockMayNotReturn(text) {
					options &^= optionsMetadataBlocks
				}
				request := eventsRequest(text, options)
				request.Callback = true
				yield(request)
			}
		}
	}, answer)
}

// codepointsTemplates are the documents TestDiffCodepointsAgainstRust writes
// out for every character: next to emphasis, strong emphasis and
// strikethrough delimiters on either side, where whether the character is
// whitespace or punctuation decides if the delimiter opens or closes; as a
// quote's neighbour for smart punctuation; escaped, as an entity name, as a
// link label, in a code span and in a table cell; and starting a line, where
// it may indent or start a block.
var codepointsTemplates = []string{
	"{c}_a_ a*{c}b* a_{c}_ *a{c}*{c} {c}~a~ a~~{c}~~b {c}**a**{c} a__{c}__\n\n",
	"'{c}' \"a{c}\" {c}'a' a\"{c}\" ({c}'a') [{c}]\"a\" \\{c} &{c}; &#{c}; [{c}] `{c}` <{c}> |{c}|\n\n",
	"{c}- a\n{c}# b\n{c}> c\n{c}1. d\n{c}```\n{c}\n",
}

// codepointsRanges are the characters TestDiffCodepointsAgainstRust covers:
// all of them up to the end of the crate's punctuation table (U+1FBCA) and of
// the plane it is in, and samples of the planes above, where every character
// is neither whitespace nor punctuation and four bytes long.
var codepointsRanges = [][2]int{{0, 0x20000}, {0x2F800, 0x30000}, {0xE0000, 0xE1000}, {0x10F000, 0x110000}}

// TestDiffCodepointsAgainstRust runs every character in codepointsRanges
// through each of codepointsTemplates, in documents of 4096 characters, and
// compares the digests with the crate's.
func TestDiffCodepointsAgainstRust(t *testing.T) {
	withoutDebugAssertions(t)
	const chunk = 0x1000
	sets := []uint32{OptionsEmpty().Bits(), (optionsAllGfmFootnotes &^ optionsMetadataBlocks).Bits()}
	oracle.Check(t, "pulldown_cmark", "codepoints", func(yield func(any)) {
		for _, template := range codepointsTemplates {
			for _, span := range codepointsRanges {
				for from := span[0]; from < span[1]; from += chunk {
					yield(oracleRequest{Op: "codepoints", Template: template, From: from, To: min(from+chunk, span[1]), Options: sets})
				}
			}
		}
	}, answer)
}

// eventHoldsString reports whether the Debug text of an event shows a
// string. Rust's shows the representation of its CowStr there
// (`Text(Borrowed("a"))`), which the port, holding a Go string, does not
// have, so the oracle leaves those events out of `debug`.
func eventHoldsString(event Event) bool {
	switch event := event.(type) {
	case *Event_Text, *Event_Code, *Event_Html, *Event_InlineHtml, *Event_FootnoteReference:
		return true
	case *Event_Start:
		switch tag := event.F0.(type) {
		case *Tag_Heading:
			return tag.Id.IsSome() || len(tag.Classes) != 0 || len(tag.Attrs) != 0
		case *Tag_CodeBlock:
			return tag.F0.IsFenced()
		case *Tag_FootnoteDefinition, *Tag_Link, *Tag_Image:
			return true
		}
	}
	return false
}

// formatterReady reports whether go/rs's formatter has been ported; until
// then the comparison of Debug text skips.
func formatterReady() (ready bool) {
	defer func() {
		if recover() != nil {
			ready = false
		}
	}()
	return rs.Format("{:>3}|{:?}", 1, "a") == `  1|"a"`
}

// TestDiffDebugAgainstRust compares the `{:?}` text of every event that
// holds no string, paired with its range, for each specification example
// under the options its test gives it, and the `{:?}` and `{:#?}` text of
// Options for every combination of the defined bits and some undefined ones.
func TestDiffDebugAgainstRust(t *testing.T) {
	withoutDebugAssertions(t)
	if !formatterReady() {
		t.Skip("the formatter (rs.Format) is not ported yet")
	}
	inputs := suiteInputs(t)
	oracle.Check(t, "pulldown_cmark", "debug", func(yield func(any)) {
		for _, input := range inputs {
			yield(oracleRequest{Op: "debug", Text: input.text, Options: input.options.Bits()})
		}
		var bits []uint32
		for b := uint32(0); b < 1<<10; b++ {
			bits = append(bits, b)
		}
		bits = append(bits, 1<<10, 1<<31, 0xfffffc00, 0xffffffff)
		yield(oracleRequest{Op: "options_debug", Bits: bits})
	}, answer)
}

// TestDiffUnicaseAgainstRust checks the case folding table against the
// crate's through the one place it shows: which reference labels match a
// definition. Every character that folds to something else defines a label,
// and the document then refers to it by its fold, by itself, and by its
// simple lower and upper case.
func TestDiffUnicaseAgainstRust(t *testing.T) {
	withoutDebugAssertions(t)
	oracle.Check(t, "pulldown_cmark", "unicase", func(yield func(any)) {
		const perDocument = 48
		for start := 0; start < len(unicaseFoldTable); start += perDocument {
			end := min(start+perDocument, len(unicaseFoldTable))
			var definitions, references strings.Builder
			for i := start; i < end; i++ {
				entry := unicaseFoldTable[i]
				n := strconv.Itoa(i)
				c := string(entry.from)
				definitions.WriteString("[" + n + "-" + c + "]: /" + n + "\n")
				references.WriteString("[" + n + "-" + entry.to + "] [" + n + "-" + c + "] [" + n + "-" +
					string(unicode.ToLower(entry.from)) + "] [" + n + "-" + string(unicode.ToUpper(entry.from)) + "]\n")
			}
			yield(eventsRequest(definitions.String()+"\n"+references.String(), OptionsEmpty()))
		}
		// The three-character folds come out of the crate rotated: these are
		// the orders CaseFolding.txt gives, which must not match.
		yield(eventsRequest("[ﬃ]: /ffi\n[ﬄ]: /ffl\n[ΐ]: /iota\n\n[ffi] [iff] [ffl] [lff] [ΐ] [́ϊ]\n", OptionsEmpty()))
		// Labels that differ only by case, in ASCII and beyond it.
		yield(eventsRequest("[ΑΓΩ]: /greek\n[Straße]: /street\n[ǅ]: /dz\n\n[αγω] [STRASSE] [strasse] [ǆ] [Ǆ] [straße]\n", OptionsEmpty()))
	}, answer)
}
