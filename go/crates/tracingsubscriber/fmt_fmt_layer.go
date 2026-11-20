// Port of tracing-subscriber 0.3.23 src/fmt/fmt_layer.rs

package tracingsubscriber

import (
	"io"
	"os"
	"reflect"
	"strings"
	"time"

	"github.com/BrokkAi/mjolnir/go/crates/tracing"
	"github.com/BrokkAi/mjolnir/go/rs"
	"github.com/BrokkAi/mjolnir/go/rs/rsio"
	"github.com/BrokkAi/mjolnir/go/rs/rstime"
)

// FmtLayer is tracing_subscriber::fmt::Layer: the layer that writes each
// event as a line of text, with the Full format and DefaultFields.
type FmtLayer struct {
	makeWriter        FmtMakeWriter
	fmtFields         *FmtFormatDefaultFields
	fmtEvent          FmtFormatFormat
	fmtSpan           fmtSpanConfig
	isAnsi            bool
	ansiSanitization  bool
	logInternalErrors bool
}

// FmtLayerNew is fmt::Layer::new: FmtLayerDefault.
func FmtLayerNew() FmtLayer {
	return FmtLayerDefault()
}

// WithWriter is Layer::with_writer: makeWriter is a FmtMakeWriter, or a
// function of no arguments returning an io.Writer.
func (l FmtLayer) WithWriter(makeWriter any) FmtLayer {
	l.makeWriter = intoMakeWriter(makeWriter)
	return l
}

// Writer is Layer::writer.
func (l *FmtLayer) Writer() FmtMakeWriter {
	return l.makeWriter
}

// SetAnsi is Layer::set_ansi.
func (l *FmtLayer) SetAnsi(ansi bool) {
	l.isAnsi = ansi
}

// SetSpanEvents is Layer::set_span_events.
func (l *FmtLayer) SetSpanEvents(kind FmtFormatFmtSpan) {
	l.fmtSpan = fmtSpanConfig{kind: kind, fmtTiming: l.fmtSpan.fmtTiming}
}

// WithAnsi is Layer::with_ansi: whether to write colours.
func (l FmtLayer) WithAnsi(ansi bool) FmtLayer {
	l.isAnsi = ansi
	return l
}

// WithAnsiSanitization is Layer::with_ansi_sanitization: whether to escape
// the characters of terminal escape sequences in messages and errors.
func (l FmtLayer) WithAnsiSanitization(ansiSanitization bool) FmtLayer {
	l.ansiSanitization = ansiSanitization
	return l
}

// LogInternalErrors is Layer::log_internal_errors: whether to report on
// standard error an event that could not be formatted or written.
func (l FmtLayer) LogInternalErrors(logInternalErrors bool) FmtLayer {
	l.logInternalErrors = logInternalErrors
	return l
}

// WithTimer is Layer::with_timer.
func (l FmtLayer) WithTimer(timer FmtTimeFormatTime) FmtLayer {
	l.fmtEvent = l.fmtEvent.WithTimer(timer)
	return l
}

// WithoutTime is Layer::without_time: no timestamps, and no timings on
// close events.
func (l FmtLayer) WithoutTime() FmtLayer {
	l.fmtEvent = l.fmtEvent.WithoutTime()
	l.fmtSpan = l.fmtSpan.withoutTime()
	return l
}

// WithSpanEvents is Layer::with_span_events.
func (l FmtLayer) WithSpanEvents(kind FmtFormatFmtSpan) FmtLayer {
	l.fmtSpan = l.fmtSpan.withKind(kind)
	return l
}

// WithTarget is Layer::with_target.
func (l FmtLayer) WithTarget(displayTarget bool) FmtLayer {
	l.fmtEvent = l.fmtEvent.WithTarget(displayTarget)
	return l
}

// WithFile is Layer::with_file. The port's callsites know no Rust source
// file, so it panics with rs.Todo when asked to show one.
func (l FmtLayer) WithFile(displayFilename bool) FmtLayer {
	if displayFilename {
		panic(rs.Todo("tracing_subscriber::fmt::Layer::with_file: the port's callsites have no source file"))
	}
	l.fmtEvent = l.fmtEvent.WithFile(displayFilename)
	return l
}

// WithLineNumber is Layer::with_line_number. The port's callsites know no
// Rust source line, so it panics with rs.Todo when asked to show one.
func (l FmtLayer) WithLineNumber(displayLineNumber bool) FmtLayer {
	if displayLineNumber {
		panic(rs.Todo("tracing_subscriber::fmt::Layer::with_line_number: the port's callsites have no source line"))
	}
	l.fmtEvent = l.fmtEvent.WithLineNumber(displayLineNumber)
	return l
}

// WithLevel is Layer::with_level.
func (l FmtLayer) WithLevel(displayLevel bool) FmtLayer {
	l.fmtEvent = l.fmtEvent.WithLevel(displayLevel)
	return l
}

// WithThreadIds is Layer::with_thread_ids.
func (l FmtLayer) WithThreadIds(displayThreadIds bool) FmtLayer {
	l.fmtEvent = l.fmtEvent.WithThreadIds(displayThreadIds)
	return l
}

// WithThreadNames is Layer::with_thread_names.
func (l FmtLayer) WithThreadNames(displayThreadNames bool) FmtLayer {
	l.fmtEvent = l.fmtEvent.WithThreadNames(displayThreadNames)
	return l
}

// FmtLayerDefault is `impl Default for fmt::Layer`: the Full format with the
// system time, written to standard output, in colour unless NO_COLOR is set
// and not empty.
func FmtLayerDefault() FmtLayer {
	// only enable ANSI when the feature is enabled, and the NO_COLOR
	// environment variable is unset or empty.
	noColor, _ := os.LookupEnv("NO_COLOR")
	ansi := noColor == ""
	return FmtLayer{
		fmtFields:         FmtFormatDefaultFieldsNew(),
		fmtEvent:          FmtFormatFormatDefault(),
		fmtSpan:           fmtSpanConfigDefault(),
		makeWriter:        stdoutMakeWriter(),
		isAnsi:            ansi,
		ansiSanitization:  true,
		logInternalErrors: false,
	}
}

func (l *FmtLayer) makeCtx(ctx LayerContext, event *tracing.Event) *FmtFmtContext {
	return &FmtFmtContext{ctx: ctx, fmtFields: l.fmtFields, event: event}
}

// FmtFormattedFields is tracing_subscriber::fmt::FormattedFields: a span's
// fields as the field formatter wrote them, kept in the span's extensions.
type FmtFormattedFields struct {
	wasAnsi          bool
	wasAnsiSanitized bool
	// Fields is the formatted fields.
	Fields string
}

// FmtFormattedFieldsNew is FormattedFields::new.
func FmtFormattedFieldsNew(fields string) FmtFormattedFields {
	return FmtFormattedFields{Fields: fields, wasAnsi: false, wasAnsiSanitized: true}
}

// AsWriter is FormattedFields::as_writer: a writer that appends to Fields.
func (f *FmtFormattedFields) AsWriter() *FmtFormatWriter {
	return FmtFormatWriterNew(stringAppender{s: &f.Fields}).
		withAnsi(f.wasAnsi).
		withAnsiSanitization(f.wasAnsiSanitized)
}

// String is `impl Display for FormattedFields`.
func (f *FmtFormattedFields) String() string { return f.Fields }

// stringAppender is `&mut String` as a fmt::Write.
type stringAppender struct {
	s *string
}

func (a stringAppender) WriteString(s string) (int, error) {
	*a.s += s
	return len(s), nil
}

// === impl FmtLayer ===

// withEventFromSpan is the crate's with_event_from_span! macro: an event
// with the span's metadata and the given fields, child of the span, given
// to the layer's on_event.
func (l *FmtLayer) withEventFromSpan(id tracing.SpanId, span *RegistrySpanRef, names []string, values []any, ctx LayerContext) {
	meta := span.Metadata()
	cs := meta.Callsite()
	fs := tracing.FieldFieldSetNew(names, cs)
	vs := fs.ValueSet(fs.Iter(), values)
	event := tracing.EventNewChildOf(rs.Some(id), meta, vs)
	l.OnEvent(event, ctx)
}

func (l *FmtLayer) OnRegisterDispatch(subscriber *tracing.Dispatch) {
	LayerDefaultOnRegisterDispatch(l, subscriber)
}

func (l *FmtLayer) OnLayer(subscriber tracing.Subscriber) { LayerDefaultOnLayer(l, subscriber) }

func (l *FmtLayer) RegisterCallsite(metadata *tracing.Metadata) tracing.SubscriberInterest {
	return LayerDefaultRegisterCallsite(l, metadata)
}

func (l *FmtLayer) Enabled(metadata *tracing.Metadata, ctx LayerContext) bool {
	return LayerDefaultEnabled(l, metadata, ctx)
}

// OnNewSpan is `impl Layer for fmt::Layer`: the span's fields are formatted
// once, now, and kept with it.
func (l *FmtLayer) OnNewSpan(attrs *tracing.SpanAttributes, id tracing.SpanId, ctx LayerContext) {
	span := ctx.Span(id).Expect("Span not found, this is a bug")
	extensions := span.ExtensionsMut()

	if RegistryExtensionsMutGetMut[FmtFormattedFields](extensions).IsNone() {
		fields := FmtFormattedFieldsNew("")
		fields.wasAnsi = l.isAnsi
		fields.wasAnsiSanitized = l.ansiSanitization
		if l.fmtFields.FormatFields(fields.AsWriter(), attrs) == nil {
			RegistryExtensionsMutInsert(extensions, fields)
		} else {
			extensions.Drop()
			eprintln("[tracing-subscriber] Unable to format the following event, ignoring: " + attributesDebug(attrs))
			extensions = span.ExtensionsMut()
		}
	}

	if l.fmtSpan.fmtTiming && l.fmtSpan.traceClose() &&
		RegistryExtensionsMutGetMut[timings](extensions).IsNone() {
		RegistryExtensionsMutInsert(extensions, timingsNew())
	}

	if l.fmtSpan.traceNew() {
		extensions.Drop()
		l.withEventFromSpan(id, span, []string{"message"}, []any{"new"}, ctx)
		return
	}
	extensions.Drop()
}

// OnRecord is `impl Layer for fmt::Layer`: values recorded later are added
// to the span's formatted fields.
func (l *FmtLayer) OnRecord(id tracing.SpanId, values *tracing.SpanRecord, ctx LayerContext) {
	span := ctx.Span(id).Expect("Span not found, this is a bug")
	extensions := span.ExtensionsMut()
	defer extensions.Drop()
	if fields, ok := RegistryExtensionsMutGetMut[FmtFormattedFields](extensions).Get(); ok {
		_ = l.fmtFields.AddFields(fields, values)
		return
	}

	fields := FmtFormattedFieldsNew("")
	fields.wasAnsi = l.isAnsi
	fields.wasAnsiSanitized = l.ansiSanitization
	if l.fmtFields.FormatFields(fields.AsWriter(), values) == nil {
		RegistryExtensionsMutInsert(extensions, fields)
	}
}

func (l *FmtLayer) MaxLevelHint() rs.Option[tracing.LevelFilter] {
	return LayerDefaultMaxLevelHint(l)
}

func (l *FmtLayer) OnFollowsFrom(span tracing.SpanId, follows tracing.SpanId, ctx LayerContext) {
	LayerDefaultOnFollowsFrom(l, span, follows, ctx)
}

func (l *FmtLayer) EventEnabled(event *tracing.Event, ctx LayerContext) bool {
	return LayerDefaultEventEnabled(l, event, ctx)
}

// OnEnter is `impl Layer for fmt::Layer`.
func (l *FmtLayer) OnEnter(id tracing.SpanId, ctx LayerContext) {
	if l.fmtSpan.traceEnter() || l.fmtSpan.traceClose() && l.fmtSpan.fmtTiming {
		span := ctx.Span(id).Expect("Span not found, this is a bug")
		extensions := span.ExtensionsMut()
		if timings, ok := RegistryExtensionsMutGetMut[timings](extensions).Get(); ok {
			if timings.enteredCount == 0 {
				now := rstime.InstantNow()
				timings.idle += uint64(now.DurationSince(timings.last) / time.Nanosecond)
				timings.last = now
			}
			timings.enteredCount++
		}

		if l.fmtSpan.traceEnter() {
			extensions.Drop()
			l.withEventFromSpan(id, span, []string{"message"}, []any{"enter"}, ctx)
			return
		}
		extensions.Drop()
	}
}

// OnExit is `impl Layer for fmt::Layer`.
func (l *FmtLayer) OnExit(id tracing.SpanId, ctx LayerContext) {
	if l.fmtSpan.traceExit() || l.fmtSpan.traceClose() && l.fmtSpan.fmtTiming {
		span := ctx.Span(id).Expect("Span not found, this is a bug")
		extensions := span.ExtensionsMut()
		if timings, ok := RegistryExtensionsMutGetMut[timings](extensions).Get(); ok {
			timings.enteredCount--
			if timings.enteredCount == 0 {
				now := rstime.InstantNow()
				timings.busy += uint64(now.DurationSince(timings.last) / time.Nanosecond)
				timings.last = now
			}
		}

		if l.fmtSpan.traceExit() {
			extensions.Drop()
			l.withEventFromSpan(id, span, []string{"message"}, []any{"exit"}, ctx)
			return
		}
		extensions.Drop()
	}
}

// OnClose is `impl Layer for fmt::Layer`.
func (l *FmtLayer) OnClose(id tracing.SpanId, ctx LayerContext) {
	if l.fmtSpan.traceClose() {
		span := ctx.Span(id).Expect("Span not found, this is a bug")
		extensions := span.Extensions()
		if timing, ok := RegistryExtensionsGet[timings](extensions).Get(); ok {
			busy := timing.busy
			idle := timing.idle
			last := timing.last
			rs.DebugAssert(timing.enteredCount == 0, "assertion `left == right` failed\n  left: {}\n right: 0", timing.enteredCount)
			idle += uint64(rstime.InstantNow().DurationSince(last) / time.Nanosecond)

			tIdle := tracing.FieldDisplay(timingDisplay(idle))
			tBusy := tracing.FieldDisplay(timingDisplay(busy))
			extensions.Drop()
			l.withEventFromSpan(id, span,
				[]string{"message", "time.busy", "time.idle"},
				[]any{"close", tBusy, tIdle}, ctx)
		} else {
			extensions.Drop()
			l.withEventFromSpan(id, span, []string{"message"}, []any{"close"}, ctx)
		}
	}
}

// OnEvent is `impl Layer for fmt::Layer`: the event is formatted into one
// string and written with one write_all to a writer made for it, which is
// then dropped.
func (l *FmtLayer) OnEvent(event *tracing.Event, ctx LayerContext) {
	var buf strings.Builder

	fmtCtx := l.makeCtx(ctx, event)
	writer := FmtFormatWriterNew(&buf).withAnsi(l.isAnsi).withAnsiSanitization(l.ansiSanitization)
	if l.fmtEvent.FormatEvent(fmtCtx, writer, event) == nil {
		w := l.makeWriter.MakeWriterFor(event.Metadata())
		res := rsio.WriteAll(w, []byte(buf.String()))
		if l.logInternalErrors {
			if res != nil {
				eprintln("[tracing-subscriber] Unable to write an event to the Writer for this Subscriber! Error: " +
					res.Error() + "\n")
			}
		}
		dropWriter(w)
	} else if l.logInternalErrors {
		errMsg := "Unable to format the following event. Name: " + event.Metadata().Name() +
			"; Fields: " + eventFieldsDebug(event) + "\n"
		w := l.makeWriter.MakeWriterFor(event.Metadata())
		if res := rsio.WriteAll(w, []byte(errMsg)); res != nil {
			eprintln("[tracing-subscriber] Unable to write an \"event formatting error\" to the Writer for this Subscriber! Error: " +
				res.Error() + "\n")
		}
		dropWriter(w)
	}
}

func (l *FmtLayer) OnIdChange(old tracing.SpanId, new tracing.SpanId, ctx LayerContext) {
	LayerDefaultOnIdChange(l, old, new, ctx)
}

// DowncastRaw is `impl Layer for fmt::Layer`: the layer, or its event
// format, its field format or its writer.
func (l *FmtLayer) DowncastRaw(target reflect.Type) (any, bool) {
	switch target {
	case reflect.TypeFor[*FmtLayer]():
		return l, true
	case reflect.TypeFor[FmtFormatFormat]():
		return l.fmtEvent, true
	case reflect.TypeFor[*FmtFormatFormat]():
		return &l.fmtEvent, true
	case reflect.TypeFor[*FmtFormatDefaultFields]():
		return l.fmtFields, true
	case reflect.TypeFor[FmtFormatDefaultFields]():
		return *l.fmtFields, true
	}
	if reflect.TypeOf(l.makeWriter) == target {
		return l.makeWriter, true
	}
	return nil, false
}

// dropWriter drops a writer made for one event, as Rust drops it at the end
// of on_event: one with a Drop method has it called.
func dropWriter(w io.Writer) {
	if dropper, ok := w.(interface{ Drop() }); ok {
		dropper.Drop()
	}
}

// eventFieldsDebug is `format!("{:?}", event.fields())`: the derived Debug
// of field::Iter.
func eventFieldsDebug(event *tracing.Event) string {
	fields := event.Fields()
	var names []string
	for _, field := range fields {
		names = append(names, tracing.FieldDebugString(field.Name()))
	}
	callsite := rs.DebugString(event.Metadata().Callsite())
	return "Iter { idxs: 0.." + rs.ToString(len(fields)) + ", fields: FieldSet { names: [" +
		strings.Join(names, ", ") + "], callsite: " + callsite + " } }"
}

// attributesDebug is `format!("{:?}", attrs)`: the derived Debug of span
// Attributes.
func attributesDebug(attrs *tracing.SpanAttributes) string {
	return rs.DebugString(attrs)
}

// FmtFmtContext is tracing_subscriber::fmt::FmtContext: what the event
// formatter is given to look up spans and format fields.
type FmtFmtContext struct {
	ctx       LayerContext
	fmtFields *FmtFormatDefaultFields
	event     *tracing.Event
}

// FmtDebug is `impl Debug for FmtContext`.
func (c *FmtFmtContext) FmtDebug(f *rs.Formatter) error {
	return f.DebugStruct("FmtContext").Finish()
}

// FormatFields is `impl FormatFields for FmtContext`.
func (c *FmtFmtContext) FormatFields(writer *FmtFormatWriter, fields FieldRecordFields) error {
	return c.fmtFields.FormatFields(writer, fields)
}

// VisitSpans is FmtContext::visit_spans: f for each span of the event's
// scope, outermost first, until one fails.
func (c *FmtFmtContext) VisitSpans(f func(span *RegistrySpanRef) error) error {
	// visit all the current spans
	if scope, ok := c.EventScope().Get(); ok {
		fromRoot := scope.FromRoot()
		for span, ok := fromRoot.Next().Get(); ok; span, ok = fromRoot.Next().Get() {
			if err := f(span); err != nil {
				return err
			}
		}
	}
	return nil
}

// Metadata is FmtContext::metadata.
func (c *FmtFmtContext) Metadata(id tracing.SpanId) rs.Option[*tracing.Metadata] {
	return c.ctx.Metadata(id)
}

// Span is FmtContext::span.
func (c *FmtFmtContext) Span(id tracing.SpanId) rs.Option[*RegistrySpanRef] {
	return c.ctx.Span(id)
}

// Exists is FmtContext::exists.
func (c *FmtFmtContext) Exists(id tracing.SpanId) bool {
	return c.ctx.Exists(id)
}

// LookupCurrent is FmtContext::lookup_current.
func (c *FmtFmtContext) LookupCurrent() rs.Option[*RegistrySpanRef] {
	return c.ctx.LookupCurrent()
}

// CurrentSpan is FmtContext::current_span.
func (c *FmtFmtContext) CurrentSpan() tracing.CurrentSpan {
	return c.ctx.CurrentSpan()
}

// ParentSpan is FmtContext::parent_span: the span the event belongs to.
func (c *FmtFmtContext) ParentSpan() rs.Option[*RegistrySpanRef] {
	return c.ctx.EventSpan(c.event)
}

// SpanScope is FmtContext::span_scope.
func (c *FmtFmtContext) SpanScope(id tracing.SpanId) rs.Option[*RegistryScope] {
	return c.ctx.SpanScope(id)
}

// EventScope is FmtContext::event_scope: the span the event belongs to and
// its parents.
func (c *FmtFmtContext) EventScope() rs.Option[*RegistryScope] {
	return c.ctx.EventScope(c.event)
}

// FieldFormat is FmtContext::field_format.
func (c *FmtFmtContext) FieldFormat() *FmtFormatDefaultFields {
	return c.fmtFields
}

// timings is Timings: how long a span has been entered and not, for its
// close event.
type timings struct {
	idle         uint64
	busy         uint64
	last         rstime.Instant
	enteredCount uint64
}

func timingsNew() timings {
	return timings{idle: 0, busy: 0, last: rstime.InstantNow(), enteredCount: 0}
}
