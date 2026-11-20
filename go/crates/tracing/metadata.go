// Port of tracing-core 0.1.36 src/metadata.rs

package tracing

import (
	"strconv"
	"sync/atomic"

	"github.com/BrokkAi/mjolnir/go/rs"
	"github.com/BrokkAi/mjolnir/go/rs/strs"
)

// Metadata describes a span or an event: its name, target, level and the
// names of its fields.
//
// In Rust a Metadata is a static of the callsite. Here an event's Metadata is
// made when the event is, and a span's is shared by every span made with the
// same level, target, name and field names.
type Metadata struct {
	name       string
	target     string
	level      Level
	modulePath rs.Option[string]
	file       rs.Option[string]
	line       rs.Option[uint32]
	fields     FieldFieldSet
	kind       MetadataKind
}

// MetadataKind is tracing_core::metadata::Kind: whether a callsite is a
// span, an event, or an `enabled!` hint about one of them.
type MetadataKind uint8

// Level is tracing::Level: the verbosity of a span or an event.
//
// The constants are ordered as Rust orders levels: LevelError < LevelWarn <
// LevelInfo < LevelDebug < LevelTrace, a more verbose level being greater.
// Go's own comparison operators therefore give what the Rust operators give.
type Level uint8

// LevelFilter is tracing::level_filters::LevelFilter: the most verbose level
// that is enabled, or LevelFilterOff. It compares with a Level by plain
// conversion: an event is enabled when uint8(filter) >= uint8(level), which
// is what LevelFilter.Enables reports.
type LevelFilter uint8

// ParseLevelFilterError is returned when a LevelFilter does not parse.
type ParseLevelFilterError struct{}

var maxLevel atomic.Uint32 // a LevelFilter; LevelFilterOff until a dispatcher is registered

// MetadataNew is Metadata::new. The field set is given by its names; the
// callsite it belongs to is the Metadata itself.
func MetadataNew(
	name string,
	target string,
	level Level,
	file rs.Option[string],
	line rs.Option[uint32],
	modulePath rs.Option[string],
	fieldNames []string,
	kind MetadataKind,
) *Metadata {
	metadata := &Metadata{
		name:       name,
		target:     target,
		level:      level,
		modulePath: modulePath,
		file:       file,
		line:       line,
		kind:       kind,
	}
	metadata.fields = FieldFieldSet{names: fieldNames, callsite: CallsiteIdentifier{metadata: metadata}}
	return metadata
}

// Fields returns the names of the fields.
func (m *Metadata) Fields() *FieldFieldSet { return &m.fields }

// Level returns the level.
func (m *Metadata) Level() Level { return m.level }

// Name returns the name: a span's name, or "event" for an event.
func (m *Metadata) Name() string { return m.name }

// Target returns the target: by default the Rust module path of the callsite.
func (m *Metadata) Target() string { return m.target }

// ModulePath returns the module path, when it was recorded.
func (m *Metadata) ModulePath() rs.Option[string] { return m.modulePath }

// File returns the source file, when it was recorded.
func (m *Metadata) File() rs.Option[string] { return m.file }

// Line returns the source line, when it was recorded.
func (m *Metadata) Line() rs.Option[uint32] { return m.line }

// Callsite returns the identity of the callsite this metadata describes.
func (m *Metadata) Callsite() CallsiteIdentifier { return m.fields.callsite }

// IsEvent reports whether the callsite is an event.
func (m *Metadata) IsEvent() bool { return m.kind.IsEvent() }

// IsSpan reports whether the callsite is a span.
func (m *Metadata) IsSpan() bool { return m.kind.IsSpan() }

// FmtDebug is `impl Debug for Metadata`.
func (m *Metadata) FmtDebug(f *rs.Formatter) error {
	meta := f.DebugStruct("Metadata")
	meta.Field("name", m.name).Field("target", m.target).Field("level", m.level)

	if path, ok := m.modulePath.Get(); ok {
		meta.Field("module_path", path)
	}

	file, hasFile := m.file.Get()
	line, hasLine := m.line.Get()
	switch {
	case hasFile && hasLine:
		meta.Field("location", FieldDisplay(file+":"+strconv.FormatUint(uint64(line), 10)))
	case hasFile:
		meta.Field("file", FieldDisplay(file))
	case hasLine:
		meta.Field("line", line)
	}

	return meta.Field("fields", FieldDisplay(&m.fields)).
		Field("callsite", m.Callsite()).
		Field("kind", m.kind).
		Finish()
}

const (
	kindEventBit MetadataKind = 1 << 0
	kindSpanBit  MetadataKind = 1 << 1
	kindHintBit  MetadataKind = 1 << 2

	// MetadataKindEvent is Kind::EVENT.
	MetadataKindEvent = kindEventBit
	// MetadataKindSpan is Kind::SPAN.
	MetadataKindSpan = kindSpanBit
	// MetadataKindHint is Kind::HINT.
	MetadataKindHint = kindHintBit
)

// IsSpan reports whether the kind is a span.
func (k MetadataKind) IsSpan() bool { return k&kindSpanBit == kindSpanBit }

// IsEvent reports whether the kind is an event.
func (k MetadataKind) IsEvent() bool { return k&kindEventBit == kindEventBit }

// IsHint reports whether the kind is a hint.
func (k MetadataKind) IsHint() bool { return k&kindHintBit == kindHintBit }

// Hint is Kind::hint: the kind with the hint bit set.
func (k MetadataKind) Hint() MetadataKind { return k | kindHintBit }

// FmtDebug is `impl Debug for Kind`.
func (k MetadataKind) FmtDebug(f *rs.Formatter) error {
	return f.WriteStr(k.debugString())
}

func (k MetadataKind) debugString() string {
	text := "Kind("
	hasBits := false
	writeBit := func(name string) {
		if hasBits {
			text += " | "
		}
		text += name
		hasBits = true
	}

	if k.IsEvent() {
		writeBit("EVENT")
	}

	if k.IsSpan() {
		writeBit("SPAN")
	}

	if k.IsHint() {
		writeBit("HINT")
	}

	// if none of the expected bits were set, something is messed up, so
	// just print the bits for debugging purposes
	if !hasBits {
		text += "0b" + strconv.FormatUint(uint64(k), 2)
	}

	return text + ")"
}

const (
	// LevelError is Level::ERROR: very serious errors.
	LevelError Level = 1
	// LevelWarn is Level::WARN: hazardous situations.
	LevelWarn Level = 2
	// LevelInfo is Level::INFO: useful information.
	LevelInfo Level = 3
	// LevelDebug is Level::DEBUG: lower priority information.
	LevelDebug Level = 4
	// LevelTrace is Level::TRACE: very low priority, often extremely verbose.
	LevelTrace Level = 5
)

// AsStr is Level::as_str.
func (l Level) AsStr() string {
	switch l {
	case LevelTrace:
		return "TRACE"
	case LevelDebug:
		return "DEBUG"
	case LevelInfo:
		return "INFO"
	case LevelWarn:
		return "WARN"
	case LevelError:
		return "ERROR"
	}
	panic(rs.Unreachable("invalid tracing.Level {}", uint8(l)))
}

// String is the level's Display text: its name in capitals.
func (l Level) String() string { return l.AsStr() }

// FmtDisplay is `impl Display for Level`.
func (l Level) FmtDisplay(f *rs.Formatter) error { return f.Pad(l.AsStr()) }

// FmtDebug is the derived Debug of Level: `Level(Info)`.
func (l Level) FmtDebug(f *rs.Formatter) error { return f.WriteStr(l.debugString()) }

func (l Level) debugString() string {
	switch l {
	case LevelTrace:
		return "Level(Trace)"
	case LevelDebug:
		return "Level(Debug)"
	case LevelInfo:
		return "Level(Info)"
	case LevelWarn:
		return "Level(Warn)"
	case LevelError:
		return "Level(Error)"
	}
	panic(rs.Unreachable("invalid tracing.Level {}", uint8(l)))
}

// Compare is `impl Ord for Level`: a more verbose level is greater.
func (l Level) Compare(other Level) int {
	switch {
	case l < other:
		return -1
	case l > other:
		return 1
	}
	return 0
}

// LevelFromStr is `impl FromStr for Level`: a name in any case, or a number
// from 1 (error) to 5 (trace).
func LevelFromStr(s string) (Level, error) {
	if num, err := strs.ParseUsize(s); err == nil {
		switch num {
		case 1:
			return LevelError, nil
		case 2:
			return LevelWarn, nil
		case 3:
			return LevelInfo, nil
		case 4:
			return LevelDebug, nil
		case 5:
			return LevelTrace, nil
		}
	}
	switch {
	case strs.EqIgnoreAsciiCase(s, "error"):
		return LevelError, nil
	case strs.EqIgnoreAsciiCase(s, "warn"):
		return LevelWarn, nil
	case strs.EqIgnoreAsciiCase(s, "info"):
		return LevelInfo, nil
	case strs.EqIgnoreAsciiCase(s, "debug"):
		return LevelDebug, nil
	case strs.EqIgnoreAsciiCase(s, "trace"):
		return LevelTrace, nil
	}
	return 0, &ParseLevelError{}
}

const (
	// LevelFilterOff is LevelFilter::OFF: nothing is enabled.
	LevelFilterOff LevelFilter = 0
	// LevelFilterError is LevelFilter::ERROR.
	LevelFilterError LevelFilter = LevelFilter(LevelError)
	// LevelFilterWarn is LevelFilter::WARN.
	LevelFilterWarn LevelFilter = LevelFilter(LevelWarn)
	// LevelFilterInfo is LevelFilter::INFO.
	LevelFilterInfo LevelFilter = LevelFilter(LevelInfo)
	// LevelFilterDebug is LevelFilter::DEBUG.
	LevelFilterDebug LevelFilter = LevelFilter(LevelDebug)
	// LevelFilterTrace is LevelFilter::TRACE: everything is enabled.
	LevelFilterTrace LevelFilter = LevelFilter(LevelTrace)
)

// LevelFilterFromLevel is LevelFilter::from_level and `From<Level>`.
func LevelFilterFromLevel(level Level) LevelFilter { return LevelFilter(level) }

// LevelFilterFromOptionLevel is `From<Option<Level>> for LevelFilter`.
func LevelFilterFromOptionLevel(level rs.Option[Level]) LevelFilter {
	if level, ok := level.Get(); ok {
		return LevelFilter(level)
	}
	return LevelFilterOff
}

// IntoLevel is LevelFilter::into_level: the most verbose level enabled, or
// None for LevelFilterOff.
func (f LevelFilter) IntoLevel() rs.Option[Level] {
	if f == LevelFilterOff {
		return rs.None[Level]()
	}
	return rs.Some(Level(f))
}

// Enables is `filter >= level`: whether the filter lets the level through.
func (f LevelFilter) Enables(level Level) bool { return uint8(f) >= uint8(level) }

// Compare is `impl Ord for LevelFilter`: off is least, trace greatest.
func (f LevelFilter) Compare(other LevelFilter) int {
	switch {
	case f < other:
		return -1
	case f > other:
		return 1
	}
	return 0
}

// LevelFilterCurrent is LevelFilter::current: the most verbose level any
// registered subscriber enables. Events and spans above it are skipped
// before a subscriber is asked.
func LevelFilterCurrent() LevelFilter {
	return LevelFilter(maxLevel.Load())
}

func levelFilterSetMax(level LevelFilter) {
	maxLevel.Store(uint32(level))
}

// String is the filter's Display text: its name in lower case.
func (f LevelFilter) String() string {
	switch f {
	case LevelFilterOff:
		return "off"
	case LevelFilterError:
		return "error"
	case LevelFilterWarn:
		return "warn"
	case LevelFilterInfo:
		return "info"
	case LevelFilterDebug:
		return "debug"
	case LevelFilterTrace:
		return "trace"
	}
	panic(rs.Unreachable("invalid tracing.LevelFilter {}", uint8(f)))
}

// FmtDisplay is `impl Display for LevelFilter`.
func (f LevelFilter) FmtDisplay(formatter *rs.Formatter) error { return formatter.Pad(f.String()) }

// FmtDebug is `impl Debug for LevelFilter`.
func (f LevelFilter) FmtDebug(formatter *rs.Formatter) error {
	return formatter.Pad(f.debugString())
}

func (f LevelFilter) debugString() string {
	switch f {
	case LevelFilterOff:
		return "LevelFilter::OFF"
	case LevelFilterError:
		return "LevelFilter::ERROR"
	case LevelFilterWarn:
		return "LevelFilter::WARN"
	case LevelFilterInfo:
		return "LevelFilter::INFO"
	case LevelFilterDebug:
		return "LevelFilter::DEBUG"
	case LevelFilterTrace:
		return "LevelFilter::TRACE"
	}
	panic(rs.Unreachable("invalid tracing.LevelFilter {}", uint8(f)))
}

// LevelFilterFromStr is `impl FromStr for LevelFilter`: a name in any case
// ("off" included), a number from 0 (off) to 5 (trace), or the empty string,
// which is error.
func LevelFilterFromStr(from string) (LevelFilter, error) {
	if num, err := strs.ParseUsize(from); err == nil {
		switch num {
		case 0:
			return LevelFilterOff, nil
		case 1:
			return LevelFilterError, nil
		case 2:
			return LevelFilterWarn, nil
		case 3:
			return LevelFilterInfo, nil
		case 4:
			return LevelFilterDebug, nil
		case 5:
			return LevelFilterTrace, nil
		}
	}
	switch {
	case from == "":
		return LevelFilterError, nil
	case strs.EqIgnoreAsciiCase(from, "error"):
		return LevelFilterError, nil
	case strs.EqIgnoreAsciiCase(from, "warn"):
		return LevelFilterWarn, nil
	case strs.EqIgnoreAsciiCase(from, "info"):
		return LevelFilterInfo, nil
	case strs.EqIgnoreAsciiCase(from, "debug"):
		return LevelFilterDebug, nil
	case strs.EqIgnoreAsciiCase(from, "trace"):
		return LevelFilterTrace, nil
	case strs.EqIgnoreAsciiCase(from, "off"):
		return LevelFilterOff, nil
	}
	return 0, &ParseLevelFilterError{}
}

// ParseLevelError is returned when a Level does not parse.
type ParseLevelError struct{}

func (e *ParseLevelError) String() string {
	return "error parsing level: expected one of \"error\", \"warn\", " +
		"\"info\", \"debug\", \"trace\", or a number 1-5"
}

func (e *ParseLevelError) Error() string { return e.String() }

// FmtDisplay is `impl Display for ParseLevelError`.
func (e *ParseLevelError) FmtDisplay(f *rs.Formatter) error { return f.Pad(e.String()) }

// FmtDebug is the derived Debug of ParseLevelError.
func (e *ParseLevelError) FmtDebug(f *rs.Formatter) error {
	return f.DebugStruct("ParseLevelError").Field("_p", rs.Unit{}).Finish()
}

func (e *ParseLevelFilterError) String() string {
	return "error parsing level filter: expected one of \"off\", \"error\", " +
		"\"warn\", \"info\", \"debug\", \"trace\", or a number 0-5"
}

func (e *ParseLevelFilterError) Error() string { return e.String() }

// FmtDisplay is `impl Display for ParseLevelFilterError`.
func (e *ParseLevelFilterError) FmtDisplay(f *rs.Formatter) error { return f.Pad(e.String()) }

// FmtDebug is the derived Debug of ParseLevelFilterError.
func (e *ParseLevelFilterError) FmtDebug(f *rs.Formatter) error {
	return f.DebugTuple("ParseLevelFilterError").Field(rs.Unit{}).Finish()
}
