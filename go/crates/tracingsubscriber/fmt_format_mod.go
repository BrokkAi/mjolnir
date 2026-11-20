// Port of tracing-subscriber 0.3.23 src/fmt/format/mod.rs

package tracingsubscriber

import (
	"errors"
	"strings"

	"github.com/BrokkAi/mjolnir/go/crates/tracing"
	"github.com/BrokkAi/mjolnir/go/rs"
)

// Of the crate's formatters only Full, the default, is ported: it is the
// one Mjolnir writes its logs with. Compact, Pretty and Json are not.

// FieldRecordFields is tracing_subscriber::field::RecordFields: something
// whose fields can be visited, an Event, span Attributes or a span Record.
type FieldRecordFields interface {
	Record(visitor tracing.FieldVisit)
}

// fmtWrite is `dyn fmt::Write`: where a Writer writes.
type fmtWrite interface {
	WriteString(s string) (int, error)
}

// FmtFormatWriter is tracing_subscriber::fmt::format::Writer: where an event
// or a span's fields are formatted, and whether ANSI colours and escape
// sanitisation are on.
type FmtFormatWriter struct {
	writer           fmtWrite
	isAnsi           bool
	ansiSanitization bool
}

// FmtFormatFormat is tracing_subscriber::fmt::format::Format with the Full
// formatter: how an event is written, and its timer.
type FmtFormatFormat struct {
	timer FmtTimeFormatTime
	// ansi is the format's own colour setting (Format::with_ansi), which
	// overrides the layer's; the builder sets the layer's.
	ansi              rs.Option[bool]
	displayTimestamp  bool
	displayTarget     bool
	displayLevel      bool
	displayThreadId   bool
	displayThreadName bool
	displayFilename   bool
	displayLineNumber bool
}

// === impl Writer ===

// FmtFormatWriterNew is Writer::new: a writer into w, without colours and
// with sanitisation.
func FmtFormatWriterNew(w fmtWrite) *FmtFormatWriter {
	return &FmtFormatWriter{writer: w, isAnsi: false, ansiSanitization: true}
}

func (w *FmtFormatWriter) withAnsi(isAnsi bool) *FmtFormatWriter {
	copied := *w
	copied.isAnsi = isAnsi
	return &copied
}

func (w *FmtFormatWriter) withAnsiSanitization(ansiSanitization bool) *FmtFormatWriter {
	copied := *w
	copied.ansiSanitization = ansiSanitization
	return &copied
}

// ByRef is Writer::by_ref: a writer into this one, with its settings.
func (w *FmtFormatWriter) ByRef() *FmtFormatWriter {
	return &FmtFormatWriter{writer: w, isAnsi: w.isAnsi, ansiSanitization: w.ansiSanitization}
}

// WriteString lets a Writer be written into, as `impl fmt::Write for
// Writer` does.
func (w *FmtFormatWriter) WriteString(s string) (int, error) {
	return w.writer.WriteString(s)
}

// WriteStr is Writer::write_str.
func (w *FmtFormatWriter) WriteStr(s string) error {
	_, err := w.writer.WriteString(s)
	return err
}

// WriteChar is Writer::write_char.
func (w *FmtFormatWriter) WriteChar(c rune) error {
	_, err := w.writer.WriteString(string(c))
	return err
}

// WriteFmt is `write!(writer, ..)`: format is a Rust format string, as for
// rs.Format.
func (w *FmtFormatWriter) WriteFmt(format string, args ...any) error {
	return w.WriteStr(rs.Format(format, args...))
}

// HasAnsiEscapes is Writer::has_ansi_escapes.
func (w *FmtFormatWriter) HasAnsiEscapes() bool { return w.isAnsi }

// SanitizesAnsiEscapes is Writer::sanitizes_ansi_escapes.
func (w *FmtFormatWriter) SanitizesAnsiEscapes() bool { return w.ansiSanitization }

func (w *FmtFormatWriter) bold() ansiStyle {
	if w.isAnsi {
		return styleNew().bold()
	}
	return styleNew()
}

func (w *FmtFormatWriter) dimmed() ansiStyle {
	if w.isAnsi {
		return styleNew().dimmed()
	}
	return styleNew()
}

func (w *FmtFormatWriter) italic() ansiStyle {
	if w.isAnsi {
		return styleNew().italic()
	}
	return styleNew()
}

// === impl Format ===

// FmtFormatFormatDefault is `impl Default for Format<Full, SystemTime>`.
func FmtFormatFormatDefault() FmtFormatFormat {
	return FmtFormatFormat{
		timer:             FmtTimeSystemTime{},
		ansi:              rs.None[bool](),
		displayTimestamp:  true,
		displayTarget:     true,
		displayLevel:      true,
		displayThreadId:   false,
		displayThreadName: false,
		displayFilename:   false,
		displayLineNumber: false,
	}
}

// FmtFormat is fmt::format::format: the default format.
func FmtFormat() FmtFormatFormat {
	return FmtFormatFormatDefault()
}

// WithTimer is Format::with_timer.
func (f FmtFormatFormat) WithTimer(timer FmtTimeFormatTime) FmtFormatFormat {
	f.timer = timer
	return f
}

// WithoutTime is Format::without_time.
func (f FmtFormatFormat) WithoutTime() FmtFormatFormat {
	f.timer = fmtTimeNone{}
	f.displayTimestamp = false
	return f
}

// WithAnsi is Format::with_ansi.
func (f FmtFormatFormat) WithAnsi(ansi bool) FmtFormatFormat {
	f.ansi = rs.Some(ansi)
	return f
}

// WithTarget is Format::with_target.
func (f FmtFormatFormat) WithTarget(displayTarget bool) FmtFormatFormat {
	f.displayTarget = displayTarget
	return f
}

// WithLevel is Format::with_level.
func (f FmtFormatFormat) WithLevel(displayLevel bool) FmtFormatFormat {
	f.displayLevel = displayLevel
	return f
}

// WithThreadIds is Format::with_thread_ids.
func (f FmtFormatFormat) WithThreadIds(displayThreadId bool) FmtFormatFormat {
	f.displayThreadId = displayThreadId
	return f
}

// WithThreadNames is Format::with_thread_names.
func (f FmtFormatFormat) WithThreadNames(displayThreadName bool) FmtFormatFormat {
	f.displayThreadName = displayThreadName
	return f
}

// WithFile is Format::with_file.
func (f FmtFormatFormat) WithFile(displayFilename bool) FmtFormatFormat {
	f.displayFilename = displayFilename
	return f
}

// WithLineNumber is Format::with_line_number.
func (f FmtFormatFormat) WithLineNumber(displayLineNumber bool) FmtFormatFormat {
	f.displayLineNumber = displayLineNumber
	return f
}

// WithSourceLocation is Format::with_source_location.
func (f FmtFormatFormat) WithSourceLocation(displayLocation bool) FmtFormatFormat {
	return f.WithLineNumber(displayLocation).WithFile(displayLocation)
}

func (f *FmtFormatFormat) formatTimestamp(writer *FmtFormatWriter) error {
	// If timestamps are disabled, do nothing.
	if !f.displayTimestamp {
		return nil
	}

	// If ANSI color codes are enabled, format the timestamp with ANSI
	// colors.
	if writer.HasAnsiEscapes() {
		style := styleNew().dimmed()
		if err := writer.WriteStr(style.prefix()); err != nil {
			return err
		}

		// If getting the timestamp failed, don't bail --- only bail on
		// formatting errors.
		if f.timer.FormatTime(writer) != nil {
			if err := writer.WriteStr("<unknown time>"); err != nil {
				return err
			}
		}

		return writer.WriteStr(style.suffix() + " ")
	}

	// Otherwise, just format the timestamp without ANSI formatting.
	// If getting the timestamp failed, don't bail --- only bail on
	// formatting errors.
	if f.timer.FormatTime(writer) != nil {
		if err := writer.WriteStr("<unknown time>"); err != nil {
			return err
		}
	}
	return writer.WriteChar(' ')
}

// FormatEvent is `impl FormatEvent for Format<Full, T>`: one line,
//
//	<time> <LEVEL> span{fields}:span{fields}: target: message field=value ...
//
// Event normalisation for records that came from the `log` crate
// (tracing-log's NormalizeEvent) does not apply: the port has no `log`
// bridge, so no event comes from one.
func (f *FmtFormatFormat) FormatEvent(ctx *FmtFmtContext, writer *FmtFormatWriter, event *tracing.Event) error {
	meta := event.Metadata()

	// if the `Format` struct *also* has an ANSI color configuration,
	// override the writer...the API for configuring ANSI color codes on the
	// `Format` struct is deprecated, but we still need to honor those
	// configurations.
	if ansi, ok := f.ansi.Get(); ok {
		writer = writer.withAnsi(ansi)
	}

	if err := f.formatTimestamp(writer); err != nil {
		return err
	}

	if f.displayLevel {
		fmtLevel := fmtLevelString(meta.Level(), writer.HasAnsiEscapes())
		if err := writer.WriteStr(fmtLevel + " "); err != nil {
			return err
		}
	}

	if f.displayThreadName || f.displayThreadId {
		panic(rs.Todo("tracing_subscriber::fmt::format::Format::with_thread_names/with_thread_ids: " +
			"the port has no thread names or ids to show"))
	}

	dimmed := writer.dimmed()

	if scope, ok := ctx.EventScope().Get(); ok {
		bold := writer.bold()

		seen := false

		fromRoot := scope.FromRoot()
		for span, ok := fromRoot.Next().Get(); ok; span, ok = fromRoot.Next().Get() {
			if err := writer.WriteStr(bold.paint(span.Metadata().Name())); err != nil {
				return err
			}
			seen = true

			ext := span.Extensions()
			if fields, ok := RegistryExtensionsGet[FmtFormattedFields](ext).Get(); ok {
				if fields.Fields != "" {
					if err := writer.WriteStr(bold.paint("{") + fields.Fields + bold.paint("}")); err != nil {
						ext.Drop()
						return err
					}
				}
			}
			ext.Drop()
			if err := writer.WriteStr(dimmed.paint(":")); err != nil {
				return err
			}
		}

		if seen {
			if err := writer.WriteChar(' '); err != nil {
				return err
			}
		}
	}

	if f.displayTarget {
		if err := writer.WriteStr(dimmed.paint(meta.Target()) + dimmed.paint(":") + " "); err != nil {
			return err
		}
	}

	lineNumber := rs.None[uint32]()
	if f.displayLineNumber {
		lineNumber = meta.Line()
	}

	if f.displayFilename {
		if filename, ok := meta.File().Get(); ok {
			trailing := " "
			if lineNumber.IsSome() {
				trailing = ""
			}
			if err := writer.WriteStr(dimmed.paint(filename) + dimmed.paint(":") + trailing); err != nil {
				return err
			}
		}
	}

	if line, ok := lineNumber.Get(); ok {
		if err := writer.WriteStr(dimmed.prefix() + rs.ToString(line) + ":" + dimmed.suffix() + " "); err != nil {
			return err
		}
	}

	if err := ctx.FormatFields(writer.ByRef(), event); err != nil {
		return err
	}
	return writer.WriteStr("\n")
}

// === impl DefaultFields ===

// FmtFormatDefaultFields is tracing_subscriber::fmt::format::DefaultFields:
// fields written as `name=value` separated by spaces, the message first and
// bare.
type FmtFormatDefaultFields struct{}

// FmtFormatDefaultVisitor is tracing_subscriber::fmt::format::DefaultVisitor.
type FmtFormatDefaultVisitor struct {
	writer  *FmtFormatWriter
	isEmpty bool
	result  error
}

// FmtFormatDefaultFieldsNew is DefaultFields::new.
func FmtFormatDefaultFieldsNew() *FmtFormatDefaultFields {
	return &FmtFormatDefaultFields{}
}

// MakeVisitor is `impl MakeVisitor for DefaultFields`.
func (d *FmtFormatDefaultFields) MakeVisitor(target *FmtFormatWriter) *FmtFormatDefaultVisitor {
	return FmtFormatDefaultVisitorNew(target, true)
}

// FormatFields is `impl FormatFields for DefaultFields`: the fields of
// fields, written into writer.
func (d *FmtFormatDefaultFields) FormatFields(writer *FmtFormatWriter, fields FieldRecordFields) error {
	v := d.MakeVisitor(writer)
	fields.Record(v)
	return v.Finish()
}

// AddFields is FormatFields::add_fields: values recorded on a span later
// are appended to its formatted fields after a space.
func (d *FmtFormatDefaultFields) AddFields(current *FmtFormattedFields, fields *tracing.SpanRecord) error {
	if current.Fields != "" {
		current.Fields += " "
	}
	return d.FormatFields(current.AsWriter(), fields)
}

// === impl DefaultVisitor ===

// FmtFormatDefaultVisitorNew is DefaultVisitor::new. is_empty says whether
// the first field must not be preceded by a space.
func FmtFormatDefaultVisitorNew(writer *FmtFormatWriter, isEmpty bool) *FmtFormatDefaultVisitor {
	return &FmtFormatDefaultVisitor{writer: writer, isEmpty: isEmpty, result: nil}
}

func (v *FmtFormatDefaultVisitor) maybePad() {
	if v.isEmpty {
		v.isEmpty = false
	} else {
		v.result = v.writer.WriteStr(" ")
	}
}

// debugText is a value whose Debug form is the text: `format_args!(..)`
// recorded with record_debug.
type debugText string

// FmtDebug writes the text.
func (t debugText) FmtDebug(f *rs.Formatter) error { return f.WriteStr(string(t)) }

func (t debugText) String() string { return string(t) }

// RecordStr is `impl Visit for DefaultVisitor`: a string is written in its
// Debug form, quoted and escaped, except the message, written as it is.
func (v *FmtFormatDefaultVisitor) RecordStr(field *tracing.FieldField, value string) {
	if v.result != nil {
		return
	}

	if field.Name() == "message" {
		v.RecordDebug(field, debugText(value))
	} else {
		v.RecordDebug(field, value)
	}
}

// RecordError is `impl Visit for DefaultVisitor`: an error is written by
// its Display text, followed by the list of its sources when it has any.
func (v *FmtFormatDefaultVisitor) RecordError(field *tracing.FieldField, value error) {
	sanitize := v.writer.SanitizesAnsiEscapes()
	if source := errors.Unwrap(value); source != nil {
		italic := v.writer.italic()
		v.RecordDebug(field, debugText(
			escapeGuard(errorText(value), sanitize)+" "+
				italic.paint(field.Name())+
				italic.paint(".sources")+
				v.writer.dimmed().paint("=")+
				errorSourceList(source, sanitize)))
	} else {
		v.RecordDebug(field, debugText(escapeGuard(errorText(value), sanitize)))
	}
}

// RecordDebug is `impl Visit for DefaultVisitor`: `name=value` with the
// value's Debug text, or the message alone.
func (v *FmtFormatDefaultVisitor) RecordDebug(field *tracing.FieldField, value any) {
	if v.result != nil {
		return
	}

	name := field.Name()

	// Skip fields that are actually log metadata that have already been handled
	if strings.HasPrefix(name, "log.") {
		rs.DebugAssert(v.result == nil) // no need to update self.result
		return
	}

	// emit separating spaces if needed
	v.maybePad()

	switch {
	case name == "message":
		// Escape ANSI characters to prevent malicious patterns (e.g., terminal injection attacks)
		v.result = v.writer.WriteStr(escapeGuard(fieldDebugText(value), v.writer.SanitizesAnsiEscapes()))
	case strings.HasPrefix(name, "r#"):
		v.result = v.writer.WriteStr(v.writer.italic().paint(name[2:]) + v.writer.dimmed().paint("=") + fieldDebugText(value))
	default:
		v.result = v.writer.WriteStr(v.writer.italic().paint(name) + v.writer.dimmed().paint("=") + fieldDebugText(value))
	}
}

// RecordF64 is the default Visit::record_f64.
func (v *FmtFormatDefaultVisitor) RecordF64(field *tracing.FieldField, value float64) {
	tracing.FieldVisitDefaultRecordF64(v, field, value)
}

// RecordI64 is the default Visit::record_i64.
func (v *FmtFormatDefaultVisitor) RecordI64(field *tracing.FieldField, value int64) {
	tracing.FieldVisitDefaultRecordI64(v, field, value)
}

// RecordU64 is the default Visit::record_u64.
func (v *FmtFormatDefaultVisitor) RecordU64(field *tracing.FieldField, value uint64) {
	tracing.FieldVisitDefaultRecordU64(v, field, value)
}

// RecordBool is the default Visit::record_bool.
func (v *FmtFormatDefaultVisitor) RecordBool(field *tracing.FieldField, value bool) {
	tracing.FieldVisitDefaultRecordBool(v, field, value)
}

// RecordBytes is the default Visit::record_bytes.
func (v *FmtFormatDefaultVisitor) RecordBytes(field *tracing.FieldField, value []byte) {
	tracing.FieldVisitDefaultRecordBytes(v, field, value)
}

// Finish is `impl VisitOutput for DefaultVisitor`.
func (v *FmtFormatDefaultVisitor) Finish() error {
	return v.result
}

// Writer is `impl VisitFmt for DefaultVisitor`.
func (v *FmtFormatDefaultVisitor) Writer() *FmtFormatWriter {
	return v.writer
}

// fieldDebugText is `format!("{:?}", value)` for a value a visitor was
// given.
func fieldDebugText(value any) string {
	if text, ok := value.(debugText); ok {
		return string(text)
	}
	return tracing.FieldDebugString(value)
}

// errorText is `format!("{}", error)`.
func errorText(err error) string {
	return err.Error()
}

// errorSourceList is ErrorSourceList's Display: the error and its sources
// as a Debug list of their Display texts, `[a, b]`.
func errorSourceList(err error, ansiSanitization bool) string {
	var list strings.Builder
	list.WriteByte('[')
	for curr := err; curr != nil; curr = errors.Unwrap(curr) {
		if curr != err {
			list.WriteString(", ")
		}
		list.WriteString(escapeGuard(errorText(curr), ansiSanitization))
	}
	list.WriteByte(']')
	return list.String()
}

// === FmtLevel ===

const (
	traceStr = "TRACE"
	debugStr = "DEBUG"
	infoStr  = " INFO"
	warnStr  = " WARN"
	errorStr = "ERROR"
)

// fmtLevelString is FmtLevel's Display without width: the level padded to
// five characters, in its colour when ANSI is on.
func fmtLevelString(level tracing.Level, ansi bool) string {
	if ansi {
		switch level {
		case tracing.LevelTrace:
			return colorPaint(ansiPurple, traceStr)
		case tracing.LevelDebug:
			return colorPaint(ansiBlue, debugStr)
		case tracing.LevelInfo:
			return colorPaint(ansiGreen, infoStr)
		case tracing.LevelWarn:
			return colorPaint(ansiYellow, warnStr)
		case tracing.LevelError:
			return colorPaint(ansiRed, errorStr)
		}
	} else {
		switch level {
		case tracing.LevelTrace:
			return traceStr
		case tracing.LevelDebug:
			return debugStr
		case tracing.LevelInfo:
			return infoStr
		case tracing.LevelWarn:
			return warnStr
		case tracing.LevelError:
			return errorStr
		}
	}
	panic(rs.Unreachable())
}

// === printing synthetic Span events ===

// FmtFormatFmtSpan is tracing_subscriber::fmt::format::FmtSpan: which span
// events the fmt layer writes as events of their own.
type FmtFormatFmtSpan uint8

const (
	// FmtFormatFmtSpanNEW is FmtSpan::NEW: one event when a span is made.
	FmtFormatFmtSpanNEW FmtFormatFmtSpan = 1 << 0
	// FmtFormatFmtSpanENTER is FmtSpan::ENTER: one event each time a span is
	// entered.
	FmtFormatFmtSpanENTER FmtFormatFmtSpan = 1 << 1
	// FmtFormatFmtSpanEXIT is FmtSpan::EXIT: one event each time a span is
	// exited.
	FmtFormatFmtSpanEXIT FmtFormatFmtSpan = 1 << 2
	// FmtFormatFmtSpanCLOSE is FmtSpan::CLOSE: one event when a span
	// closes, with its busy and idle times unless the format has no timer.
	FmtFormatFmtSpanCLOSE FmtFormatFmtSpan = 1 << 3
	// FmtFormatFmtSpanNONE is FmtSpan::NONE: no span events.
	FmtFormatFmtSpanNONE FmtFormatFmtSpan = 0
	// FmtFormatFmtSpanACTIVE is FmtSpan::ACTIVE: ENTER and EXIT.
	FmtFormatFmtSpanACTIVE = FmtFormatFmtSpanENTER | FmtFormatFmtSpanEXIT
	// FmtFormatFmtSpanFULL is FmtSpan::FULL: every span event.
	FmtFormatFmtSpanFULL = FmtFormatFmtSpanNEW | FmtFormatFmtSpanENTER | FmtFormatFmtSpanEXIT | FmtFormatFmtSpanCLOSE
)

func (s FmtFormatFmtSpan) contains(other FmtFormatFmtSpan) bool {
	return s&other == other
}

// FmtDebug is `impl Debug for FmtSpan`.
func (s FmtFormatFmtSpan) FmtDebug(f *rs.Formatter) error {
	return f.WriteStr(s.debugString())
}

func (s FmtFormatFmtSpan) debugString() string {
	if FmtFormatFmtSpanNONE|s == FmtFormatFmtSpanNONE {
		return "FmtSpan::NONE"
	}
	var flags []string
	for _, flag := range []struct {
		flag FmtFormatFmtSpan
		name string
	}{
		{FmtFormatFmtSpanNEW, "FmtSpan::NEW"},
		{FmtFormatFmtSpanENTER, "FmtSpan::ENTER"},
		{FmtFormatFmtSpanEXIT, "FmtSpan::EXIT"},
		{FmtFormatFmtSpanCLOSE, "FmtSpan::CLOSE"},
	} {
		if s.contains(flag.flag) {
			flags = append(flags, flag.name)
		}
	}
	return strings.Join(flags, " | ")
}

// fmtSpanConfig is FmtSpanConfig.
type fmtSpanConfig struct {
	kind      FmtFormatFmtSpan
	fmtTiming bool
}

func (c fmtSpanConfig) withoutTime() fmtSpanConfig {
	return fmtSpanConfig{kind: c.kind, fmtTiming: false}
}

func (c fmtSpanConfig) withKind(kind FmtFormatFmtSpan) fmtSpanConfig {
	return fmtSpanConfig{kind: kind, fmtTiming: c.fmtTiming}
}

func (c fmtSpanConfig) traceNew() bool { return c.kind.contains(FmtFormatFmtSpanNEW) }

func (c fmtSpanConfig) traceEnter() bool { return c.kind.contains(FmtFormatFmtSpanENTER) }

func (c fmtSpanConfig) traceExit() bool { return c.kind.contains(FmtFormatFmtSpanEXIT) }

func (c fmtSpanConfig) traceClose() bool { return c.kind.contains(FmtFormatFmtSpanCLOSE) }

// fmtSpanConfigDefault is `impl Default for FmtSpanConfig`.
func fmtSpanConfigDefault() fmtSpanConfig {
	return fmtSpanConfig{kind: FmtFormatFmtSpanNONE, fmtTiming: true}
}

// timingDisplay is TimingDisplay: a duration in nanoseconds with three
// significant digits and its unit.
type timingDisplay uint64

func (t timingDisplay) String() string {
	value := float64(t)
	for _, unit := range []string{"ns", "µs", "ms", "s"} {
		if value < 10.0 {
			return rs.Format("{:.2}{}", value, unit)
		} else if value < 100.0 {
			return rs.Format("{:.1}{}", value, unit)
		} else if value < 1000.0 {
			return rs.Format("{:.0}{}", value, unit)
		}
		value /= 1000.0
	}
	return rs.Format("{:.0}s", value*1000.0)
}

// FmtDisplay is `impl Display for TimingDisplay`.
func (t timingDisplay) FmtDisplay(f *rs.Formatter) error { return f.WriteStr(t.String()) }
