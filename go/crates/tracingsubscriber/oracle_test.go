package tracingsubscriber

import (
	"bytes"
	"encoding/json"
	"errors"
	"io"
	"math"
	"strings"
	"sync"
	"testing"

	"github.com/BrokkAi/mjolnir/go/crates/tracing"
	"github.com/BrokkAi/mjolnir/go/rs"
	"github.com/BrokkAi/mjolnir/go/rs/rstest/oracle"
)

// The differential tests run the same script of spans and events under the
// real fmt subscriber, configured from one description, in Rust
// (go/port/oracle/src/tracing_oracle.rs, op "run") and here, and compare the
// bytes written. The other ops parse filters and directives.

type runRequest struct {
	Op      string      `json:"op"`
	Config  runConfig   `json:"config"`
	Actions []runAction `json:"actions"`
}

type runConfig struct {
	Ansi       *bool       `json:"ansi,omitempty"`
	Level      *bool       `json:"level,omitempty"`
	Target     *bool       `json:"target,omitempty"`
	Sanitize   *bool       `json:"sanitize,omitempty"`
	SpanEvents string      `json:"span_events,omitempty"`
	Filter     *filterSpec `json:"filter,omitempty"`
	Time       string      `json:"time,omitempty"`
}

type filterSpec struct {
	Op         string  `json:"op,omitempty"`
	Directives *string `json:"directives,omitempty"`
	Regex      *bool   `json:"regex,omitempty"`
	Mode       string  `json:"mode,omitempty"`
	MaxLevel   string  `json:"max_level,omitempty"`
}

type runAction struct {
	Do      string          `json:"do"`
	Id      string          `json:"id,omitempty"`
	Name    string          `json:"name,omitempty"`
	Level   string          `json:"level,omitempty"`
	Target  string          `json:"target,omitempty"`
	Parent  string          `json:"parent,omitempty"`
	Message *messageSpec    `json:"message"`
	Fields  []fieldSpec     `json:"fields,omitempty"`
	Kind    string          `json:"kind,omitempty"`
	Value   json.RawMessage `json:"value,omitempty"`
}

type messageSpec struct {
	Format  string            `json:"format"`
	Literal string            `json:"literal,omitempty"`
	Args    []json.RawMessage `json:"args,omitempty"`
}

type fieldSpec struct {
	Name  string          `json:"name"`
	Kind  string          `json:"kind"`
	Value json.RawMessage `json:"value"`
}

func flag(value *bool, fallback bool) bool {
	if value == nil {
		return fallback
	}
	return *value
}

// fixedTime is the oracle's FixedTime.
type fixedTime struct{}

func (fixedTime) FormatTime(w *FmtFormatWriter) error {
	return w.WriteStr("2026-11-20T11:36:12.123456Z")
}

// captured is the oracle's Buffer: every writer it makes appends to one
// buffer.
type captured struct {
	mu  sync.Mutex
	buf bytes.Buffer
}

type capturedWriter struct{ into *captured }

func (w capturedWriter) Write(p []byte) (int, error) {
	w.into.mu.Lock()
	defer w.into.mu.Unlock()
	return w.into.buf.Write(p)
}

func (c *captured) MakeWriter() FmtMakeWriter {
	return FmtMakeWriterFn(func() io.Writer { return capturedWriter{into: c} })
}

func spanEvents(text string) FmtFormatFmtSpan {
	kind := FmtFormatFmtSpanNONE
	for _, part := range strings.Split(text, "|") {
		switch part {
		case "new":
			kind |= FmtFormatFmtSpanNEW
		case "enter":
			kind |= FmtFormatFmtSpanENTER
		case "exit":
			kind |= FmtFormatFmtSpanEXIT
		case "close":
			kind |= FmtFormatFmtSpanCLOSE
		}
	}
	return kind
}

func envFilter(spec *filterSpec) (*EnvFilter, error) {
	directives := ""
	if spec.Directives != nil {
		directives = *spec.Directives
	}
	builder := EnvFilterBuilder().WithRegex(flag(spec.Regex, true))
	if spec.Mode == "new" {
		return builder.WithDefaultDirective(FilterDirectiveFromLevelFilter(tracing.LevelFilterError)).
			ParseLossy(directives), nil
	}
	return builder.Parse(directives)
}

func hintText(hint rs.Option[tracing.LevelFilter]) any {
	if level, ok := hint.Get(); ok {
		return level.String()
	}
	return nil
}

func buildSubscriber(config runConfig, out *captured) (*FmtSubscriber, error) {
	builder := Fmt().
		WithWriter(out.MakeWriter()).
		WithAnsi(flag(config.Ansi, false)).
		WithLevel(flag(config.Level, true)).
		WithTarget(flag(config.Target, true)).
		WithAnsiSanitization(flag(config.Sanitize, true)).
		WithSpanEvents(spanEvents(config.SpanEvents))
	switch config.Time {
	case "none":
		builder = builder.WithoutTime()
	case "system":
	default:
		builder = builder.WithTimer(fixedTime{})
	}
	switch {
	case config.Filter != nil && config.Filter.Directives != nil:
		filter, err := envFilter(config.Filter)
		if err != nil {
			return nil, err
		}
		builder = builder.WithEnvFilter(filter)
	case config.Filter != nil && config.Filter.MaxLevel != "":
		maxLevel, err := tracing.LevelFilterFromStr(config.Filter.MaxLevel)
		if err != nil {
			return nil, err
		}
		builder = builder.WithMaxLevel(maxLevel)
	}
	return builder.Finish(), nil
}

// typedError is the oracle's Typed: an error with a chain of sources.
type typedError struct {
	text   string
	source *typedError
}

func (e *typedError) Error() string { return e.text }

func (e *typedError) Unwrap() error {
	if e.source == nil {
		return nil
	}
	return e.source
}

func newTypedError(texts []string) *typedError {
	var err *typedError
	for i := len(texts) - 1; i >= 0; i-- {
		err = &typedError{text: texts[i], source: err}
	}
	return err
}

// float reads a float from JSON, where the values JSON cannot hold are
// strings.
func float(raw json.RawMessage) (float64, bool) {
	var text string
	if json.Unmarshal(raw, &text) == nil {
		switch text {
		case "NaN":
			return math.NaN(), true
		case "inf":
			return math.Inf(1), true
		case "-inf":
			return math.Inf(-1), true
		case "-0":
			return math.Copysign(0, -1), true
		}
		return 0, false
	}
	var value float64
	return value, json.Unmarshal(raw, &value) == nil
}

func decode[T any](raw json.RawMessage) T {
	var value T
	_ = json.Unmarshal(raw, &value)
	return value
}

// heldValue is Held::parse: the field value the oracle records, as the Go
// macros take it. ok is false for a kind the oracle does not know.
func heldValue(kind string, raw json.RawMessage) (value any, ok bool) {
	switch kind {
	case "i64":
		return decode[int64](raw), true
	case "u64":
		return decode[uint64](raw), true
	case "f64":
		f, ok := float(raw)
		return f, ok
	case "bool":
		return decode[bool](raw), true
	case "str":
		return decode[string](raw), true
	case "display":
		return tracing.FieldDisplay(decode[string](raw)), true
	case "debug_str":
		return tracing.FieldDebug(decode[string](raw)), true
	case "debug_int":
		return tracing.FieldDebug(decode[int64](raw)), true
	case "debug_float":
		f, ok := float(raw)
		return tracing.FieldDebug(f), ok
	case "debug_bool":
		return tracing.FieldDebug(decode[bool](raw)), true
	case "debug_list_str":
		return tracing.FieldDebug(decode[[]string](raw)), true
	case "debug_list_int":
		return tracing.FieldDebug(decode[[]int64](raw)), true
	case "debug_opt_str":
		var text *string
		_ = json.Unmarshal(raw, &text)
		return tracing.FieldDebug(rs.OptionFromPtr(text)), true
	case "debug_opt_int":
		var number *int64
		_ = json.Unmarshal(raw, &number)
		return tracing.FieldDebug(rs.OptionFromPtr(number)), true
	case "debug_unit":
		return tracing.FieldDebug(rs.Unit{}), true
	case "error":
		return error(newTypedError(decode[[]string](raw))), true
	case "bytes":
		var numbers []uint64
		_ = json.Unmarshal(raw, &numbers)
		bytes := make([]byte, len(numbers))
		for i, number := range numbers {
			bytes[i] = byte(number)
		}
		return bytes, true
	case "empty":
		return tracing.FieldEmpty{}, true
	}
	return nil, false
}

// messageArgs gives the arguments of one of the oracle's message formats.
func messageArgs(spec *messageSpec) (string, []any, bool) {
	s := func(index int) string {
		if index < len(spec.Args) {
			return decode[string](spec.Args[index])
		}
		return ""
	}
	i := func(index int) int64 {
		if index < len(spec.Args) {
			return decode[int64](spec.Args[index])
		}
		return 0
	}
	f := func(index int) float64 {
		if index < len(spec.Args) {
			value, _ := float(spec.Args[index])
			return value
		}
		return 0
	}
	switch spec.Format {
	case "":
		return spec.Literal, nil, true
	case "{}":
		return spec.Format, []any{s(0)}, true
	case "connected to {} in {}ms":
		return spec.Format, []any{s(0), i(1)}, true
	case "value={:?} other={:?}":
		return spec.Format, []any{s(0), i(1)}, true
	case "[{:>8}] [{:<6}] [{:^7}]":
		return spec.Format, []any{s(0), s(1), s(2)}, true
	case "{:05} {:#x} {:.3}":
		return spec.Format, []any{i(0), i(1), f(2)}, true
	case "{0} {0} {1}":
		return spec.Format, []any{s(0), s(1)}, true
	case "multi\nline {}":
		return spec.Format, []any{s(0)}, true
	case "{name} is {age}":
		return spec.Format, []any{rs.Arg("name", s(0)), rs.Arg("age", i(1))}, true
	case "{{braces}} {} {{}}":
		return spec.Format, []any{s(0)}, true
	}
	return "", nil, false
}

func levelOf(text string) tracing.Level {
	level, err := tracing.LevelFromStr(text)
	if err != nil {
		return tracing.LevelInfo
	}
	return level
}

type enteredSpan struct {
	id    string
	guard *tracing.SpanEnteredSpan
}

// runScript is the Go side of op "run".
func runScript(t *testing.T, request runRequest) any {
	out := &captured{}
	subscriber, err := buildSubscriber(request.Config, out)
	if err != nil {
		return map[string]any{"filter_error": err.Error()}
	}
	hint := subscriber.MaxLevelHint()
	dispatch := tracing.DispatchNew(subscriber)

	spans := map[string]*tracing.Span{}
	spanEnabled := map[string]any{}
	var entered []enteredSpan
	probes := []any{}

	tracing.DispatcherWithDefault(dispatch, func() rs.Unit {
		for _, action := range request.Actions {
			level := levelOf(action.Level)
			switch action.Do {
			case "event":
				var names []string
				var values []any
				var fields []any
				if action.Message != nil {
					format, args, ok := messageArgs(action.Message)
					if !ok {
						t.Fatalf("unknown message format %q", action.Message.Format)
					}
					fields = append(fields, args...)
					names = append(names, "message")
					values = append(values, messageValue(format, args))
				}
				for _, field := range action.Fields {
					value, ok := heldValue(field.Kind, field.Value)
					if !ok {
						t.Fatalf("unknown field kind %q", field.Kind)
					}
					names = append(names, field.Name)
					values = append(values, value)
					fields = append(fields, tracing.F(field.Name, value))
				}
				switch action.Parent {
				case "", "current":
					// Through the macros, as product code logs.
					emitEvent(action, level, fields)
				default:
					meta := tracing.MetadataNew("event", action.Target, level, rs.None[string](), rs.None[uint32](), rs.None[string](), names, tracing.MetadataKindEvent)
					if tracing.CallsiteEnabled(meta) {
						valueSet := meta.Fields().ValueSetAll(values)
						if action.Parent == "root" {
							tracing.EventChildOf(rs.None[tracing.SpanId](), meta, valueSet)
						} else {
							tracing.EventChildOf(spanId(spans, action.Parent), meta, valueSet)
						}
					}
				}
			case "span":
				var names []string
				var values []any
				var fields []tracing.FieldArg
				for _, field := range action.Fields {
					value, ok := heldValue(field.Kind, field.Value)
					if !ok {
						t.Fatalf("unknown field kind %q", field.Kind)
					}
					names = append(names, field.Name)
					values = append(values, value)
					fields = append(fields, tracing.F(field.Name, value))
				}
				var span *tracing.Span
				switch action.Parent {
				case "", "current":
					// Through the macros, as product code makes spans.
					span = newSpanMacro(action, level, fields)
				default:
					meta := tracing.MetadataNew(action.Name, action.Target, level, rs.None[string](), rs.None[uint32](), rs.None[string](), names, tracing.MetadataKindSpan)
					if tracing.CallsiteEnabled(meta) {
						valueSet := meta.Fields().ValueSetAll(values)
						if action.Parent == "root" {
							span = tracing.SpanNewRoot(meta, valueSet)
						} else {
							span = tracing.SpanChildOf(spanId(spans, action.Parent), meta, valueSet)
						}
					} else {
						span = tracing.SpanNone()
					}
				}
				spanEnabled[action.Id] = !span.IsDisabled()
				if previous, ok := spans[action.Id]; ok {
					previous.Drop()
				}
				spans[action.Id] = span
			case "enter":
				if span, ok := spans[action.Id]; ok {
					entered = append(entered, enteredSpan{id: action.Id, guard: span.Clone().Entered()})
				}
			case "exit":
				for i := len(entered) - 1; i >= 0; i-- {
					if entered[i].id == action.Id {
						guard := entered[i].guard
						entered = append(entered[:i], entered[i+1:]...)
						guard.Drop()
						break
					}
				}
			case "record":
				value, ok := heldValue(action.Kind, action.Value)
				if !ok {
					t.Fatalf("unknown field kind %q", action.Kind)
				}
				if span, ok := spans[action.Id]; ok {
					if _, isEmpty := value.(tracing.FieldEmpty); !isEmpty {
						span.Record(action.Name, value)
					}
				}
			case "close":
				if span, ok := spans[action.Id]; ok {
					delete(spans, action.Id)
					span.Drop()
				}
			case "enabled":
				probes = append(probes, tracing.EventEnabled(action.Target, level))
			default:
				t.Fatalf("unknown action %q", action.Do)
			}
		}
		// Rust drops what the script left open, innermost first.
		for len(entered) > 0 {
			guard := entered[len(entered)-1].guard
			entered = entered[:len(entered)-1]
			guard.Drop()
		}
		for id, span := range spans {
			delete(spans, id)
			span.Drop()
		}
		return rs.Unit{}
	})

	return map[string]any{
		"output":  strings.ToValidUTF8(out.buf.String(), "�"),
		"hint":    hintText(hint),
		"spans":   spanEnabled,
		"enabled": probes,
	}
}

// messageValue is the oracle's message value for an event with an
// explicit parent: its text, recorded as `%text`.
func messageValue(format string, args []any) any {
	if args == nil {
		return tracing.FieldDisplay(strings.ReplaceAll(strings.ReplaceAll(format, "{{", "{"), "}}", "}"))
	}
	return tracing.FieldDisplay(rs.Format(format, args...))
}

func spanId(spans map[string]*tracing.Span, id string) rs.Option[tracing.SpanId] {
	if span, ok := spans[id]; ok {
		return span.Id()
	}
	return rs.None[tracing.SpanId]()
}

func emitEvent(action runAction, level tracing.Level, args []any) {
	if action.Message == nil {
		var fields []tracing.FieldArg
		for _, arg := range args {
			fields = append(fields, arg.(tracing.FieldArg))
		}
		switch level {
		case tracing.LevelError:
			tracing.ErrorFields(action.Target, fields...)
		case tracing.LevelWarn:
			tracing.WarnFields(action.Target, fields...)
		case tracing.LevelInfo:
			tracing.InfoFields(action.Target, fields...)
		case tracing.LevelDebug:
			tracing.DebugFields(action.Target, fields...)
		case tracing.LevelTrace:
			tracing.TraceFields(action.Target, fields...)
		}
		return
	}
	format, _, _ := messageArgs(action.Message)
	switch level {
	case tracing.LevelError:
		tracing.Error(action.Target, format, args...)
	case tracing.LevelWarn:
		tracing.Warn(action.Target, format, args...)
	case tracing.LevelInfo:
		tracing.Info(action.Target, format, args...)
	case tracing.LevelDebug:
		tracing.Debug(action.Target, format, args...)
	case tracing.LevelTrace:
		tracing.Trace(action.Target, format, args...)
	}
}

func newSpanMacro(action runAction, level tracing.Level, fields []tracing.FieldArg) *tracing.Span {
	switch level {
	case tracing.LevelError:
		return tracing.ErrorSpan(action.Target, action.Name, fields...)
	case tracing.LevelWarn:
		return tracing.WarnSpan(action.Target, action.Name, fields...)
	case tracing.LevelInfo:
		return tracing.InfoSpan(action.Target, action.Name, fields...)
	case tracing.LevelDebug:
		return tracing.DebugSpan(action.Target, action.Name, fields...)
	}
	return tracing.TraceSpan(action.Target, action.Name, fields...)
}

// ===== filters and directives =====

func describeFilter(spec filterSpec) any {
	diagnostics = &bytes.Buffer{}
	filter, err := envFilter(&spec)
	if err != nil {
		return map[string]any{"error": err.Error()}
	}
	return map[string]any{
		"display": filter.String(),
		"hint":    hintText(filter.MaxLevelHint()),
	}
}

type directiveRequest struct {
	Op   string `json:"op"`
	Text string `json:"text"`
}

func describeDirective(text string) any {
	directive, err := FilterDirectiveFromStr(text)
	if err != nil {
		var source any
		if inner := errors.Unwrap(err); inner != nil {
			source = inner.Error()
		}
		return map[string]any{"error": err.Error(), "source": source}
	}
	return map[string]any{"display": directive.String()}
}

func TestDiffRunAgainstRust(t *testing.T) {
	oracle.Check(t, "tracing", "run", runCases, func(request json.RawMessage) any {
		var decoded runRequest
		if err := json.Unmarshal(request, &decoded); err != nil {
			t.Fatalf("decode %s: %v", request, err)
		}
		return oracle.Catch(func() any { return runScript(t, decoded) })
	})
}

func TestDiffFilterAgainstRust(t *testing.T) {
	saved := diagnostics
	defer func() { diagnostics = saved }()
	oracle.Check(t, "tracing", "filter", filterCases, func(request json.RawMessage) any {
		var spec filterSpec
		if err := json.Unmarshal(request, &spec); err != nil {
			t.Fatalf("decode %s: %v", request, err)
		}
		return oracle.Catch(func() any { return describeFilter(spec) })
	})
}

func TestDiffDirectiveAgainstRust(t *testing.T) {
	oracle.Check(t, "tracing", "directive", directiveCases, func(request json.RawMessage) any {
		var decoded directiveRequest
		if err := json.Unmarshal(request, &decoded); err != nil {
			t.Fatalf("decode %s: %v", request, err)
		}
		return oracle.Catch(func() any { return describeDirective(decoded.Text) })
	})
}
