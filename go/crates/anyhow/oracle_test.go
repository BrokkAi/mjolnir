package anyhow

import (
	"encoding/json"
	"os"
	"strings"
	"testing"

	"github.com/BrokkAi/mjolnir/go/rs"
	"github.com/BrokkAi/mjolnir/go/rs/rstest/oracle"
)

// The differential tests build the same error in Rust (go/port/oracle/src/
// anyhow_oracle.rs) and here from one description, and compare what each
// says about it.

// errorSpec describes an error: a root and the context layers put on it,
// innermost first.
type errorSpec struct {
	Op     string      `json:"op"`
	Root   rootSpec    `json:"root"`
	Layers []layerSpec `json:"layers"`
	// Go says how the Go side holds the root; Rust does not read it.
	Go goSpec `json:"go"`
}

type rootSpec struct {
	Kind   string   `json:"kind"`
	Text   string   `json:"text,omitempty"`
	Texts  []string `json:"texts,omitempty"`
	Code   int32    `json:"code,omitempty"`
	IoKind string   `json:"io_kind,omitempty"`
}

type layerSpec struct {
	Kind   string   `json:"kind"`
	Text   string   `json:"text,omitempty"`
	Texts  []string `json:"texts,omitempty"`
	Value  int32    `json:"value,omitempty"`
	Direct bool     `json:"direct,omitempty"`
}

type goSpec struct {
	// Bare leaves a root that is an error value unwrapped: the error reaches
	// the functions of this package without having passed through New.
	Bare bool `json:"bare,omitempty"`
	// Unwrap makes a typed root report its source through Unwrap instead of
	// Source.
	Unwrap bool `json:"unwrap,omitempty"`
}

// staticStr stands in for `&'static str` where Rust tells it from `String`:
// the two are one type in Go, and the comparison would otherwise not know
// which of the two downcasts Rust answered.
type staticStr string

func (s staticStr) String() string                   { return string(s) }
func (s staticStr) FmtDisplay(f *rs.Formatter) error { return f.Pad(string(s)) }
func (s staticStr) FmtDebug(f *rs.Formatter) error   { return rs.DebugFmt(string(s), f) }

func buildError(spec errorSpec) error {
	var current error
	std := false
	switch spec.Root.Kind {
	case "literal":
		current = Msg(staticStr(spec.Root.Text))
	case "string":
		current = Msg(spec.Root.Text)
	case "msg_typed":
		current = Msg(typedMsg{text: spec.Root.Text})
	case "option":
		_, current = OptionContext(rs.None[rs.Unit](), spec.Root.Text)
	case "option_typed":
		_, current = OptionWithContext(rs.None[rs.Unit](), func() typedCtx { return typedCtx{text: spec.Root.Text} })
	case "io_os":
		current, std = ioErrorFromRawOsError(spec.Root.Code), true
	case "io_custom":
		current, std = ioErrorNew(ioKind(spec.Root.IoKind), spec.Root.Text), true
	case "io_simple":
		current, std = ioErrorFromKind(ioKind(spec.Root.IoKind)), true
	case "typed":
		if spec.Go.Unwrap {
			current, std = newGoError(spec.Root.Texts), true
		} else {
			current, std = newTyped(spec.Root.Texts), true
		}
	case "boxed":
		current = FromBoxed(newTyped(spec.Root.Texts))
	case "boxed_str":
		current = FromBoxed(stringError(spec.Root.Text))
	default:
		panic("unknown root kind " + spec.Root.Kind)
	}
	for index, layer := range spec.Layers {
		var context any
		switch layer.Kind {
		case "str":
			context = staticStr(layer.Text)
		case "string":
			context = layer.Text
		case "typed":
			context = typedCtx{text: layer.Text}
		case "int":
			context = layer.Value
		case "error":
			context = newTyped(layer.Texts)
		default:
			panic("unknown layer kind " + layer.Kind)
		}
		if index == 0 && std && !layer.Direct && !spec.Go.Bare {
			// Rust: Error::new(error).context(..)
			current = New(current)
		}
		current = Context(current, context)
	}
	if len(spec.Layers) == 0 && std && !spec.Go.Bare {
		current = New(current)
	}
	return current
}

func textOrNil[T any](value T, ok bool, text func(T) string) any {
	if !ok {
		return nil
	}
	return text(value)
}

func errorText[T error](value T) string { return value.Error() }

func describeError(err error) any {
	if err.Error() != DisplayString(err) {
		panic("Error() and DisplayString disagree")
	}

	downcast := map[string]any{}
	str, ok := DowncastRef[staticStr](err)
	downcast["str"] = textOrNil(str, ok, staticStr.String)
	text, ok := DowncastRef[string](err)
	downcast["string"] = textOrNil(text, ok, func(s string) string { return s })
	io, ok := DowncastRef[*ioError](err)
	downcast["io"] = textOrNil(io, ok, errorText)
	if typed, ok := DowncastRef[*typedError](err); ok {
		downcast["typed"] = typed.Error()
	} else {
		typed, ok := DowncastRef[*goError](err)
		downcast["typed"] = textOrNil(typed, ok, errorText)
	}
	ctx, ok := DowncastRef[typedCtx](err)
	downcast["typed_ctx"] = textOrNil(ctx, ok, typedCtx.String)
	msg, ok := DowncastRef[typedMsg](err)
	downcast["typed_msg"] = textOrNil(msg, ok, typedMsg.String)
	number, ok := DowncastRef[int32](err)
	downcast["int"] = textOrNil(number, ok, func(n int32) string { return rs.ToString(n) })
	boxed, ok := DowncastRef[error](err)
	downcast["boxed"] = textOrNil(boxed, ok, errorText)

	chain := []string{}
	chainDowncast := []any{}
	for _, cause := range Chain(err) {
		chain = append(chain, cause.Error())
		found := []string{}
		if Is[*ioError](cause) {
			found = append(found, "io")
		}
		if Is[*typedError](cause) || Is[*goError](cause) {
			found = append(found, "typed")
		}
		chainDowncast = append(chainDowncast, found)
	}

	var source any
	if cause, ok := Source(err).Get(); ok {
		source = cause.Error()
	}

	return map[string]any{
		"display":        DisplayString(err),
		"alternate":      AlternateString(err),
		"debug":          DebugString(err),
		"chain":          chain,
		"root_cause":     RootCause(err).Error(),
		"source":         source,
		"downcast":       downcast,
		"chain_downcast": chainDowncast,
	}
}

func describeErrorFmt(err error) any {
	chainDisplay := []string{}
	chainDebug := []string{}
	chainAltDebug := []string{}
	chainWidth := []string{}
	for _, cause := range Chain(err) {
		chainDisplay = append(chainDisplay, rs.Format("{}", cause))
		chainDebug = append(chainDebug, rs.Format("{:?}", cause))
		chainAltDebug = append(chainAltDebug, rs.Format("{:#?}", cause))
		chainWidth = append(chainWidth, rs.Format("[{:>12.5}]", cause))
	}
	return map[string]any{
		"display":         rs.Format("{}", err),
		"alternate":       rs.Format("{:#}", err),
		"debug":           rs.Format("{:?}", err),
		"alt_debug":       rs.Format("{:#?}", err),
		"width":           rs.Format("[{error:>40}|{error:<#40}|{error:^8.3}|{error:12?}]", rs.Arg("error", err)),
		"chain_display":   chainDisplay,
		"chain_debug":     chainDebug,
		"chain_alt_debug": chainAltDebug,
		"chain_width":     chainWidth,
		"nested":          rs.Format("{:?}", rs.Some(err)),
		"nested_alt":      rs.Format("{:#?}", rs.T2(err, int32(1))),
	}
}

// messages are the texts errors and contexts are made of: the edges of the
// "Caused by:" layout (empty, multi-line, trailing newline), text that looks
// like a format string or like anyhow's own output, and text Debug escapes.
var messages = []string{
	"",
	" ",
	"plain",
	"failed to read the configuration",
	"trailing newline\n",
	"\nleading newline",
	"multi\nline\nmessage",
	"\n",
	"\n\n",
	"crlf\r\nline",
	"tab\there",
	"braces {} {0} {name} {{escaped}} }{ {:?}",
	"percent %s %d %v %%",
	"quote \" apostrophe ' backslash \\",
	"unicode é ñ ü 日本語 🦀",
	"combining é mark",
	"́ starts with a combining mark",
	"zero​width and bidi ‮ override",
	"control \x01 \x7f \x1b[31m",
	"nul \x00 byte",
	"Caused by:",
	"    0: looks numbered",
	": colon: separated: ",
	"ﾞ halfwidth voiced mark",
	"private  use, unassigned ͸, tag \U000e0041",
	"line one\n    indented line two\n\ttabbed line three",
	"ends with spaces   ",
	"中文\n第二行",
	"No such file or directory (os error 2)",
	strings.Repeat("long ", 60),
}

// generator is a small fixed random source, so the corpus does not depend on
// the Go release.
type generator struct{ state uint64 }

func (g *generator) next() uint64 {
	g.state += 0x9e3779b97f4a7c15
	z := g.state
	z = (z ^ (z >> 30)) * 0xbf58476d1ce4e5b9
	z = (z ^ (z >> 27)) * 0x94d049bb133111eb
	return z ^ (z >> 31)
}

func (g *generator) below(n int) int { return int(g.next() % uint64(n)) }

func (g *generator) message() string { return messages[g.below(len(messages))] }

func (g *generator) texts() []string {
	texts := make([]string, 1+g.below(4))
	for i := range texts {
		texts[i] = g.message()
	}
	return texts
}

var osCodes = []int32{1, 2, 9, 13, 17, 21, 22, 28, 32}

var ioKinds = []string{
	"NotFound", "PermissionDenied", "ConnectionRefused", "AlreadyExists", "InvalidInput",
	"InvalidData", "TimedOut", "UnexpectedEof", "BrokenPipe", "Other",
}

var rootKinds = []string{
	"literal", "string", "msg_typed", "option", "option_typed", "io_os", "io_custom", "io_simple",
	"typed", "typed", "boxed", "boxed_str",
}

var layerKinds = []string{"str", "str", "string", "string", "typed", "int", "error"}

func (g *generator) root(kind string) rootSpec {
	root := rootSpec{Kind: kind}
	switch kind {
	case "io_os":
		root.Code = osCodes[g.below(len(osCodes))]
	case "io_custom":
		root.IoKind = ioKinds[g.below(len(ioKinds))]
		root.Text = g.message()
	case "io_simple":
		root.IoKind = ioKinds[g.below(len(ioKinds))]
	case "typed", "boxed":
		root.Texts = g.texts()
	default:
		root.Text = g.message()
	}
	return root
}

func (g *generator) layer(kind string) layerSpec {
	layer := layerSpec{Kind: kind}
	switch kind {
	case "int":
		layer.Value = int32(g.below(2001)) - 1000
	case "error":
		layer.Texts = g.texts()
	default:
		layer.Text = g.message()
	}
	return layer
}

// errorSpecs yields the fixed cases, then the random ones. fmt says the
// specs are for the forms that go through the formatter, which only an error
// known to be an anyhow error has: no bare roots there.
func errorSpecs(op string, fmt bool, yield func(any)) {
	g := &generator{state: 20261120}
	emit := func(spec errorSpec) {
		spec.Op = op
		if spec.Layers == nil {
			spec.Layers = []layerSpec{}
		}
		if fmt {
			spec.Go.Bare = false
		}
		yield(spec)
	}

	// Every root alone, with each message.
	for _, kind := range []string{"literal", "string", "msg_typed", "option", "option_typed", "boxed_str"} {
		for _, message := range messages {
			emit(errorSpec{Root: rootSpec{Kind: kind, Text: message}})
		}
	}
	for _, code := range osCodes {
		emit(errorSpec{Root: rootSpec{Kind: "io_os", Code: code}})
		emit(errorSpec{Root: rootSpec{Kind: "io_os", Code: code}, Go: goSpec{Bare: true}})
	}
	for _, kind := range ioKinds {
		emit(errorSpec{Root: rootSpec{Kind: "io_simple", IoKind: kind}})
		emit(errorSpec{Root: rootSpec{Kind: "io_custom", IoKind: kind, Text: g.message()}})
	}
	for depth := 1; depth <= 4; depth++ {
		texts := []string{"outermost", "second", "third\nwith a second line", "innermost"}[:depth]
		for _, kind := range []string{"typed", "boxed"} {
			emit(errorSpec{Root: rootSpec{Kind: kind, Texts: texts}})
			emit(errorSpec{Root: rootSpec{Kind: kind, Texts: texts}, Go: goSpec{Unwrap: true}})
			emit(errorSpec{Root: rootSpec{Kind: kind, Texts: texts}, Go: goSpec{Bare: true}})
		}
	}

	// Every context kind on every root, attached to the typed error itself
	// (`result.context(..)` on a typed Result) and to an anyhow error.
	for _, rootKind := range rootKinds {
		for _, layerKind := range []string{"str", "string", "typed", "int", "error"} {
			for _, direct := range []bool{false, true} {
				layer := g.layer(layerKind)
				layer.Direct = direct
				emit(errorSpec{Root: g.root(rootKind), Layers: []layerSpec{layer}})
			}
		}
	}

	// One to six levels of one message: the "Caused by:" layouts. The
	// formatter corpus repeats each message in a dozen forms, so it takes
	// fewer of them.
	levelsOf := []int{2, 3, 4, 5, 6}
	random := 400
	if fmt {
		levelsOf = []int{2, 4}
		random = 120
	}
	for _, message := range messages {
		for _, levels := range levelsOf {
			spec := errorSpec{Root: rootSpec{Kind: "literal", Text: message}}
			for range levels - 1 {
				spec.Layers = append(spec.Layers, layerSpec{Kind: "string", Text: message})
			}
			emit(spec)
		}
	}

	// More than ten causes: the number column grows.
	deep := errorSpec{Root: rootSpec{Kind: "typed", Texts: []string{"a", "b"}}}
	for range 12 {
		deep.Layers = append(deep.Layers, layerSpec{Kind: "str", Text: "layer\nsecond line"})
	}
	emit(deep)

	// Random chains.
	for range random {
		spec := errorSpec{Root: g.root(rootKinds[g.below(len(rootKinds))])}
		for range g.below(6) {
			spec.Layers = append(spec.Layers, g.layer(layerKinds[g.below(len(layerKinds))]))
		}
		if len(spec.Layers) > 0 {
			spec.Layers[0].Direct = g.below(2) == 0
		}
		spec.Go.Bare = g.below(4) == 0
		spec.Go.Unwrap = g.below(3) == 0
		emit(spec)
	}
}

func decodeSpec(t *testing.T, request json.RawMessage) errorSpec {
	t.Helper()
	var spec errorSpec
	if err := json.Unmarshal(request, &spec); err != nil {
		t.Fatalf("decode request %s: %v", request, err)
	}
	return spec
}

// TestDiffErrorAgainstRust compares the forms built from plain messages
// ({}, {:#}, {:?}), the chain, the root cause and every downcast.
func TestDiffErrorAgainstRust(t *testing.T) {
	oracle.Check(t, "anyhow", "error", func(yield func(any)) {
		errorSpecs("error", false, yield)
	}, func(request json.RawMessage) any {
		return describeError(buildError(decodeSpec(t, request)))
	})
}

// TestDiffErrorFmtAgainstRust compares what goes through rs.Formatter: the
// same three forms reached with rs.Format, {:#?}, the Debug form of each chain
// element, and widths.
func TestDiffErrorFmtAgainstRust(t *testing.T) {
	recording := os.Getenv("MJ_ORACLE") == "record"
	if !recording {
		requireFormatter(t)
	}
	oracle.Check(t, "anyhow", "error_fmt", func(yield func(any)) {
		errorSpecs("error_fmt", true, yield)
	}, func(request json.RawMessage) any {
		return oracle.Catch(func() any {
			return describeErrorFmt(buildError(decodeSpec(t, request)))
		})
	})
}

type ensureSpec struct {
	Op   string `json:"op"`
	Type string `json:"type"`
	Lhs  any    `json:"lhs"`
	Rhs  any    `json:"rhs,omitempty"`
}

func ensureSpecs(yield func(any)) {
	integers := []int64{0, 1, -1, 2, 42, -9223372036854775808, 9223372036854775807}
	for _, lhs := range integers {
		for _, rhs := range integers {
			yield(ensureSpec{Op: "ensure", Type: "i64", Lhs: lhs, Rhs: rhs})
		}
	}
	// A Debug text of up to 40 bytes with no space or newline is shown; the
	// quotes count, and so does every escape.
	texts := []string{
		"", "a", "b", "two words", "line\nbreak", "tab\ttab", "quote\"quote", "é", "日本語", "🦀",
		strings.Repeat("x", 37), strings.Repeat("x", 38), strings.Repeat("x", 39), strings.Repeat("é", 19),
		strings.Repeat("é", 20), "nul\x00", "́combining",
	}
	for _, lhs := range texts {
		for _, rhs := range texts {
			yield(ensureSpec{Op: "ensure", Type: "string", Lhs: lhs, Rhs: rhs})
		}
	}
	for _, lhs := range []bool{false, true} {
		for _, rhs := range []bool{false, true} {
			yield(ensureSpec{Op: "ensure", Type: "bool", Lhs: lhs, Rhs: rhs})
		}
		yield(ensureSpec{Op: "ensure", Type: "plain", Lhs: lhs})
	}
}

// TestDiffEnsureAgainstRust compares the message of `ensure!` without a
// message, which shows both operands of a comparison in their Debug form.
func TestDiffEnsureAgainstRust(t *testing.T) {
	if os.Getenv("MJ_ORACLE") != "record" {
		requireFormatter(t)
	}
	oracle.Check(t, "anyhow", "ensure", ensureSpecs, func(request json.RawMessage) any {
		return oracle.Catch(func() any {
			var spec struct {
				Type string          `json:"type"`
				Lhs  json.RawMessage `json:"lhs"`
				Rhs  json.RawMessage `json:"rhs"`
			}
			if err := json.Unmarshal(request, &spec); err != nil {
				t.Fatalf("decode request %s: %v", request, err)
			}
			message := func(err error) any {
				if err == nil {
					return map[string]any{"message": nil}
				}
				return map[string]any{"message": err.Error()}
			}
			switch spec.Type {
			case "i64":
				var lhs, rhs int64
				mustDecode(t, spec.Lhs, &lhs)
				mustDecode(t, spec.Rhs, &rhs)
				if lhs == rhs {
					return message(nil)
				}
				return message(EnsureFailedDebug("lhs == rhs", lhs, rhs))
			case "string":
				var lhs, rhs string
				mustDecode(t, spec.Lhs, &lhs)
				mustDecode(t, spec.Rhs, &rhs)
				if lhs == rhs {
					return message(nil)
				}
				return message(EnsureFailedDebug("lhs == rhs", lhs, rhs))
			case "bool":
				var lhs, rhs bool
				mustDecode(t, spec.Lhs, &lhs)
				mustDecode(t, spec.Rhs, &rhs)
				if lhs == rhs {
					return message(nil)
				}
				return message(EnsureFailedDebug("lhs == rhs", lhs, rhs))
			case "plain":
				var ok bool
				mustDecode(t, spec.Lhs, &ok)
				if ok {
					return message(nil)
				}
				return message(EnsureFailed("ok"))
			}
			panic("unknown ensure type " + spec.Type)
		})
	})
}

func mustDecode(t *testing.T, raw json.RawMessage, into any) {
	t.Helper()
	if err := json.Unmarshal(raw, into); err != nil {
		t.Fatalf("decode %s: %v", raw, err)
	}
}
