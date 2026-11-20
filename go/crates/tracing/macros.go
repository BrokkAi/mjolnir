// Port of tracing 0.1.44 src/macros.rs and the __macro_support module of
// src/lib.rs

package tracing

import (
	"strings"
	"sync"

	"github.com/BrokkAi/mjolnir/go/rs"
)

// The macros become functions. Each ported file declares its target once,
// the Rust module path the macros would have used:
//
//	const runtime_TARGET = "mj_core::runtime"
//
// and the macros translate as
//
//	tracing::warn!(session_id = %id, error = ?err, in_flight, "shutting down {}", name);
//	tracing.Warn(runtime_TARGET, "shutting down {}",
//		tracing.FDisplay("session_id", id), tracing.FDebug("error", err),
//		tracing.F("in_flight", inFlight), name)
//
//	tracing::debug!(target: "mj_chat::latency", %command_id, "applied");
//	tracing.Debug("mj_chat::latency", "applied", tracing.FDisplay("command_id", commandId))
//
//	tracing::warn!(%reason);                       // no message
//	tracing.WarnFields(runtime_TARGET, tracing.FDisplay("reason", reason))
//
//	let span = tracing::info_span!("quota_probe", profile_id = %id);
//	span := tracing.InfoSpan(quota_TARGET, "quota_probe", tracing.FDisplay("profile_id", id))
//
// The arguments after the format string are the macro's fields, made with F,
// FDisplay and FDebug, and its format arguments, in any order; the fields keep
// their order among themselves, as they are written in Rust. A captured
// `{name}` is passed as rs.Arg("name", value), as for rs.Format.

// FieldArg is one field of an event or a span as a macro takes it: its name
// and its value. Make it with F, FDisplay or FDebug.
type FieldArg struct {
	name  string
	value any
}

// F is a field written `name = value`, or `name` alone for a variable of that
// name. The value is recorded by its type, as tracing's `impl Value` for that
// type does: strings, booleans, integers, floats, byte slices, errors,
// options of those, and FieldEmpty (tracing::field::Empty). Any other value
// is passed with FDisplay or FDebug, as Rust requires `%` or `?` for it.
func F(name string, value any) FieldArg {
	return FieldArg{name: name, value: value}
}

// FDisplay is a field written `name = %value`, or `%name`: the value is
// recorded by its Display text.
func FDisplay(name string, value any) FieldArg {
	return FieldArg{name: name, value: FieldDisplay(value)}
}

// FDebug is a field written `name = ?value`, or `?name`: the value is
// recorded by its Debug text.
func FDebug(name string, value any) FieldArg {
	return FieldArg{name: name, value: FieldDebug(value)}
}

// Error is `error!(target: target, fields.., format, args..)`.
func Error(target, format string, args ...any) { event(target, LevelError, true, format, args) }

// Warn is `warn!(target: target, fields.., format, args..)`.
func Warn(target, format string, args ...any) { event(target, LevelWarn, true, format, args) }

// Info is `info!(target: target, fields.., format, args..)`.
func Info(target, format string, args ...any) { event(target, LevelInfo, true, format, args) }

// Debug is `debug!(target: target, fields.., format, args..)`.
func Debug(target, format string, args ...any) { event(target, LevelDebug, true, format, args) }

// Trace is `trace!(target: target, fields.., format, args..)`.
func Trace(target, format string, args ...any) { event(target, LevelTrace, true, format, args) }

// ErrorFields is `error!(target: target, fields..)`: an event with no message.
func ErrorFields(target string, fields ...FieldArg) { eventFields(target, LevelError, fields) }

// WarnFields is `warn!(target: target, fields..)`: an event with no message.
func WarnFields(target string, fields ...FieldArg) { eventFields(target, LevelWarn, fields) }

// InfoFields is `info!(target: target, fields..)`: an event with no message.
func InfoFields(target string, fields ...FieldArg) { eventFields(target, LevelInfo, fields) }

// DebugFields is `debug!(target: target, fields..)`: an event with no message.
func DebugFields(target string, fields ...FieldArg) { eventFields(target, LevelDebug, fields) }

// TraceFields is `trace!(target: target, fields..)`: an event with no message.
func TraceFields(target string, fields ...FieldArg) { eventFields(target, LevelTrace, fields) }

// ErrorSpan is `error_span!(target: target, name, fields..)`.
func ErrorSpan(target, name string, fields ...FieldArg) *Span {
	return newSpan(target, LevelError, name, fields)
}

// WarnSpan is `warn_span!(target: target, name, fields..)`.
func WarnSpan(target, name string, fields ...FieldArg) *Span {
	return newSpan(target, LevelWarn, name, fields)
}

// InfoSpan is `info_span!(target: target, name, fields..)`.
func InfoSpan(target, name string, fields ...FieldArg) *Span {
	return newSpan(target, LevelInfo, name, fields)
}

// DebugSpan is `debug_span!(target: target, name, fields..)`.
func DebugSpan(target, name string, fields ...FieldArg) *Span {
	return newSpan(target, LevelDebug, name, fields)
}

// TraceSpan is `trace_span!(target: target, name, fields..)`.
func TraceSpan(target, name string, fields ...FieldArg) *Span {
	return newSpan(target, LevelTrace, name, fields)
}

// Enabled is `enabled!(target: target, level, field_names..)`: whether an
// event or a span at this level, with this target and these fields, would be
// recorded by the current subscriber.
func Enabled(target string, level Level, fieldNames ...string) bool {
	return enabled(MetadataKindHint, target, level, fieldNames)
}

// EventEnabled is `event_enabled!(target: target, level, field_names..)`:
// Enabled for an event.
func EventEnabled(target string, level Level, fieldNames ...string) bool {
	return enabled(MetadataKindEvent, target, level, fieldNames)
}

// SpanEnabled is `span_enabled!(target: target, level, field_names..)`:
// Enabled for a span.
func SpanEnabled(target string, level Level, fieldNames ...string) bool {
	return enabled(MetadataKindSpan, target, level, fieldNames)
}

// levelEnabled is level_enabled!: the level is within STATIC_MAX_LEVEL
// (every level: Mjolnir enables none of the max_level features) and within
// LevelFilter::current.
func levelEnabled(level Level) bool {
	return LevelFilterCurrent().Enables(level)
}

// CallsiteEnabled is the check every macro makes before it builds an event
// or a span from its callsite: level_enabled!, the callsite's interest
// (Callsite::interest) and __macro_support::__is_enabled. It is for code that
// makes its own Metadata and then calls EventDispatch, EventChildOf, SpanNew
// or SpanChildOf, as an event or span with an explicit parent needs.
func CallsiteEnabled(meta *Metadata) bool {
	if !levelEnabled(meta.Level()) {
		return false
	}
	interest := callsiteInterest(meta)
	return !interest.IsNever() && isEnabled(meta, interest)
}

// isEnabled is __macro_support::__is_enabled.
func isEnabled(meta *Metadata, interest SubscriberInterest) bool {
	return interest.IsAlways() || DispatcherGetDefault(func(current *Dispatch) bool {
		return current.Enabled(meta)
	})
}

// event is the body of event! for a callsite with a message: the message is
// the first field, the macro's fields follow in order.
func event(target string, level Level, hasMessage bool, format string, args []any) {
	if !levelEnabled(level) {
		return
	}
	var fields []FieldArg
	var formatArgs []any
	for _, arg := range args {
		if field, ok := arg.(FieldArg); ok {
			fields = append(fields, field)
		} else {
			formatArgs = append(formatArgs, arg)
		}
	}
	names := make([]string, 0, len(fields)+1)
	values := make([]any, 0, len(fields)+1)
	if hasMessage {
		names = append(names, "message")
		values = append(values, &formatArguments{format: format, args: formatArgs})
	}
	for _, field := range fields {
		names = append(names, field.name)
		values = append(values, field.value)
	}
	dispatchEvent(target, level, names, values)
}

// eventFields is the body of event! for a callsite without a message.
func eventFields(target string, level Level, fields []FieldArg) {
	if !levelEnabled(level) {
		return
	}
	names := make([]string, len(fields))
	values := make([]any, len(fields))
	for i, field := range fields {
		names[i] = field.name
		values[i] = field.value
	}
	dispatchEvent(target, level, names, values)
}

func dispatchEvent(target string, level Level, names []string, values []any) {
	// The macro names an event's callsite "event <file>:<line>"; the Go
	// call site is not the Rust one, and the name is shown nowhere the
	// subscribers of this port look.
	meta := MetadataNew("event", target, level, rs.None[string](), rs.None[uint32](), rs.None[string](), names, MetadataKindEvent)
	interest := callsiteInterest(meta)
	if interest.IsNever() || !isEnabled(meta, interest) {
		return
	}
	// event with contextual parent
	EventDispatch(meta, meta.Fields().ValueSetAll(values))
}

// spanCallsites holds one Metadata for each span callsite: a span's
// metadata is its callsite's identity, which subscribers key on (an
// EnvFilter keeps the span callsites its directives match). Two spans are of
// one callsite when they have the same level, target, name and field names.
var spanCallsites sync.Map // spanCallsiteKey -> *Metadata

type spanCallsiteKey struct {
	level  Level
	target string
	name   string
	fields string
}

func spanCallsite(target string, level Level, name string, names []string) *Metadata {
	key := spanCallsiteKey{level: level, target: target, name: name, fields: strings.Join(names, "\x00")}
	if meta, ok := spanCallsites.Load(key); ok {
		return meta.(*Metadata)
	}
	meta := MetadataNew(name, target, level, rs.None[string](), rs.None[uint32](), rs.None[string](), names, MetadataKindSpan)
	stored, _ := spanCallsites.LoadOrStore(key, meta)
	return stored.(*Metadata)
}

// newSpan is the body of span! with a contextual parent.
func newSpan(target string, level Level, name string, fields []FieldArg) *Span {
	names := make([]string, len(fields))
	values := make([]any, len(fields))
	for i, field := range fields {
		names[i] = field.name
		values[i] = field.value
	}
	meta := spanCallsite(target, level, name, names)
	if levelEnabled(level) {
		if interest := callsiteInterest(meta); !interest.IsNever() && isEnabled(meta, interest) {
			// span with contextual parent
			return SpanNew(meta, meta.Fields().ValueSetAll(values))
		}
	}
	// __disabled_span: tracing's `log` feature is on in Mjolnir's build
	// (tower enables it), so a disabled span keeps its metadata.
	return SpanNewDisabled(meta)
}

// enabled is the body of enabled!.
func enabled(kind MetadataKind, target string, level Level, fieldNames []string) bool {
	if !levelEnabled(level) {
		return false
	}
	meta := MetadataNew("enabled", target, level, rs.None[string](), rs.None[uint32](), rs.None[string](), fieldNames, kind.Hint())
	interest := callsiteInterest(meta)
	if !interest.IsNever() && isEnabled(meta, interest) {
		return DispatcherGetDefault(func(current *Dispatch) bool { return current.Enabled(meta) })
	}
	return false
}

// formatArguments is the `format_args!(..)` a macro records as the value of
// its "message" field. Like fmt::Arguments it is formatted when a subscriber
// reads it, not when the event is made, and its Debug and Display forms are
// the formatted text, written without padding.
type formatArguments struct {
	format string
	args   []any
	text   rs.Option[string]
}

func (a *formatArguments) String() string {
	if text, ok := a.text.Get(); ok {
		return text
	}
	var text string
	if len(a.args) == 0 && !strings.ContainsAny(a.format, "{}") {
		// `format_args!("literal")` is the literal.
		text = a.format
	} else {
		text = rs.Format(a.format, a.args...)
	}
	a.text = rs.Some(text)
	return text
}

// Record is `impl Value for fmt::Arguments`.
func (a *formatArguments) Record(key *FieldField, visitor FieldVisit) {
	visitor.RecordDebug(key, a)
}

// FmtDebug is `impl Debug for Arguments`: the text, without the formatter's
// flags.
func (a *formatArguments) FmtDebug(f *rs.Formatter) error { return f.WriteStr(a.String()) }

// FmtDisplay is `impl Display for Arguments`.
func (a *formatArguments) FmtDisplay(f *rs.Formatter) error { return f.WriteStr(a.String()) }
