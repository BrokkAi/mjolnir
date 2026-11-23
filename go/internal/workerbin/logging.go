// Part of the port of mj-worker-bin/src/main.rs: install_stderr_logging,
// the worker's tracing-subscriber setup.
//
// Rust logs through tracing with an fmt subscriber (no ANSI) behind an
// EnvFilter read from RUST_LOG, writing to core.StderrWriter. Go logs
// through log/slog; this file is the slog.Handler that writes the same
// lines through the same writer, and EnvFilter's directive language:
// comma-separated `level` and `target=level` directives
// (`warn,mj_jev=info`), where a target is a Rust crate or module path and a
// directive matches every target it is a prefix of. A Go call names its
// target with a "target" attribute (`slog.Info(..., "target", "mj_jev")`,
// as the worker package does); a call without one has the target of the
// Go package that made it, spelled as the Rust crate it ports
// (`mj_worker`). Go has no tracing spans, so directives that name a span or
// fields parse as they do in Rust but match no record.

package workerbin

import (
	"context"
	"errors"
	"fmt"
	"log/slog"
	"math"
	"os"
	"runtime"
	"slices"
	"strconv"
	"strings"
	"sync/atomic"
	"time"
	"unicode"
	"unicode/utf8"

	"github.com/BrokkAi/mjolnir/go/internal/core"
	"github.com/BrokkAi/mjolnir/go/internal/worker"
)

// stderrLoggingInstalled is whether installStderrLogging has set the
// process-wide logger, which tracing allows once.
var stderrLoggingInstalled atomic.Bool

// installStderrLogging installs the worker's slog default: tracing's fmt
// lines on core.StderrWriter, filtered by RUST_LOG, or by
// worker.DefaultWorkerLogFilter when RUST_LOG is unset, not Unicode or
// invalid; the last two are then logged as a warning, "ignored invalid
// RUST_LOG filter", with the error.
func installStderrLogging() error {
	fallback, err := parseLogFilter(worker.DefaultWorkerLogFilter)
	if err != nil {
		return fmt.Errorf("parse the default worker log filter: %w", err)
	}
	filter := fallback
	var filterError error
	if value, present := os.LookupEnv("RUST_LOG"); present {
		if !utf8.ValidString(value) {
			filterError = errors.New("environment variable was not valid unicode: " + rustOsStrDebug(value))
		} else if parsed, err := parseLogFilter(value); err != nil {
			filterError = err
		} else {
			filter = parsed
		}
	}
	if !stderrLoggingInstalled.CompareAndSwap(false, true) {
		return errors.New("install Mjolnir stderr subscriber: a global default trace dispatcher has already been set")
	}
	slog.SetDefault(slog.New(&stderrLogHandler{filter: filter, writer: core.StderrWriter()}))
	if filterError != nil {
		slog.Warn("ignored invalid RUST_LOG filter", "error", filterError)
	}
	return nil
}

// rustOsStrDebug is Rust's Debug for an OsStr on Unix: a quoted string with
// the valid UTF-8 escaped as Rust escapes it and each invalid byte as
// `\xNN`.
func rustOsStrDebug(text string) string {
	var b strings.Builder
	b.WriteByte('"')
	for len(text) > 0 {
		r, size := utf8.DecodeRuneInString(text)
		if r == utf8.RuneError && size == 1 {
			fmt.Fprintf(&b, "\\x%02X", text[0])
		} else {
			quoted := core.RustDebugString(text[:size])
			b.WriteString(quoted[1 : len(quoted)-1])
		}
		text = text[size:]
	}
	b.WriteByte('"')
	return b.String()
}

// levelTrace is tracing's TRACE, more verbose than slog's Debug.
const levelTrace = slog.LevelDebug - 4

// logFilter is an EnvFilter: the directive with the longest target that
// matches a record's target decides the most verbose level it lets through.
type logFilter struct {
	// directives are the static directives, most specific first.
	directives []logDirective
}

// logDirective is one EnvFilter directive.
type logDirective struct {
	// target is the crate or module path the directive applies to; empty
	// for every target.
	target string
	// level is the most verbose level the directive lets through.
	level slog.Level
	// off lets nothing through (`off`).
	off bool
	// matchesNothing marks a directive that names a span or fields, which
	// no Go record has.
	matchesNothing bool
}

// parseLogLevel is tracing's LevelFilter::from_str: a level name in any
// case, `off`, a number 0 (off) to 5 (trace), or nothing (error).
func parseLogLevel(text string) (slog.Level, bool, error) {
	if _, magnitude, err := rustParseInt(text, false); err == nil && magnitude <= 5 {
		switch magnitude {
		case 0:
			return 0, true, nil
		case 1:
			return slog.LevelError, false, nil
		case 2:
			return slog.LevelWarn, false, nil
		case 3:
			return slog.LevelInfo, false, nil
		case 4:
			return slog.LevelDebug, false, nil
		case 5:
			return levelTrace, false, nil
		}
	}
	switch strings.ToLower(text) {
	case "", "error":
		return slog.LevelError, false, nil
	case "warn":
		return slog.LevelWarn, false, nil
	case "info":
		return slog.LevelInfo, false, nil
	case "debug":
		return slog.LevelDebug, false, nil
	case "trace":
		return levelTrace, false, nil
	case "off":
		return 0, true, nil
	}
	return 0, false, errors.New(`error parsing level filter: expected one of "off", "error", "warn", "info", "debug", "trace", or a number 0-5`)
}

// errInvalidDirective is EnvFilter's ParseError without a message.
var errInvalidDirective = errors.New("invalid filter directive")

// parseLogFilter is EnvFilter::try_new: directives separated by commas,
// each `level` or `target=level`, with tracing's level names in any case
// (`trace`, `debug`, `info`, `warn`, `error`, `off`).
func parseLogFilter(spec string) (logFilter, error) {
	var filter logFilter
	if spec == "" {
		return filter, nil
	}
	for _, text := range strings.Split(spec, ",") {
		if text == "" {
			continue
		}
		directive, err := parseLogDirective(text)
		if err != nil {
			return logFilter{}, err
		}
		filter.add(directive)
	}
	return filter, nil
}

// parseLogDirective is tracing-subscriber's Directive::parse: its state
// machine walks the trimmed text but slices the original, as Rust does.
func parseLogDirective(from string) (logDirective, error) {
	const (
		stateStart = iota
		stateLevelOrTarget
		stateSpan
		stateField
		stateFields
		stateTarget
		stateLevel
		stateComplete
	)
	directive := logDirective{level: levelTrace}
	setLevelOrTarget := func(text string) {
		if text == "" {
			directive.level = levelTrace
			return
		}
		if level, off, err := parseLogLevel(text); err == nil {
			directive.level, directive.off = level, off
			return
		}
		// Setting the target without the level enables every level for
		// that target.
		directive.level, directive.target = levelTrace, text
	}
	setLevel := func(text string) error {
		if text == "" {
			directive.level = levelTrace
			return nil
		}
		level, off, err := parseLogLevel(text)
		if err != nil {
			return err
		}
		directive.level, directive.off = level, off
		return nil
	}
	state, start := stateStart, 0
	trimmed := strings.TrimSpace(from)
	for index, c := range trimmed {
		switch {
		case state == stateStart && c == '[':
			state, start = stateSpan, index+1
		case state == stateStart && c != '-' && c != ':' && c != '_' && !isRustAlphanumeric(c):
			return logDirective{}, errInvalidDirective
		case state == stateStart:
			state, start = stateLevelOrTarget, index
		case state == stateLevelOrTarget && c == '=':
			directive.target = from[start:index]
			state, start = stateLevel, index+1
		case state == stateLevelOrTarget && c == '[':
			directive.target = from[start:index]
			state, start = stateSpan, index+1
		case state == stateLevelOrTarget && c == ',':
			setLevelOrTarget(from[start:])
			state = stateComplete
		case state == stateLevelOrTarget:
		case state == stateTarget && c == '=':
			state, start = stateLevel, index+1
		case state == stateSpan && c == ']':
			directive.matchesNothing = true
			state = stateTarget
		case state == stateSpan && c == '{':
			directive.matchesNothing = true
			state, start = stateField, index+1
		case state == stateSpan:
		case state == stateField && (c == '}' || c == ','):
			if from[start:index] == "" {
				return logDirective{}, errInvalidDirective
			}
			if c == '}' {
				state = stateFields
			} else {
				start = index + 1
			}
		case state == stateField:
		case state == stateFields && c == ']':
			state = stateTarget
		case state == stateLevel && c == ',':
			if err := setLevel(from[start:index]); err != nil {
				return logDirective{}, err
			}
			state = stateComplete
		case state == stateLevel:
		default:
			return logDirective{}, errInvalidDirective
		}
	}
	switch state {
	case stateLevelOrTarget:
		setLevelOrTarget(from[start:])
	case stateLevel:
		if err := setLevel(from[start:]); err != nil {
			return logDirective{}, err
		}
	case stateTarget, stateComplete:
	default:
		return logDirective{}, errInvalidDirective
	}
	return directive, nil
}

// isRustAlphanumeric is Rust's char::is_alphanumeric: a letter or a number.
func isRustAlphanumeric(c rune) bool { return unicode.IsLetter(c) || unicode.IsNumber(c) }

// add inserts directive in specificity order, replacing one with the same
// target, as EnvFilter's DirectiveSet does.
func (f *logFilter) add(directive logDirective) {
	for index, existing := range f.directives {
		if existing.target == directive.target && existing.matchesNothing == directive.matchesNothing {
			f.directives[index] = directive
			return
		}
	}
	f.directives = append(f.directives, directive)
	slices.SortStableFunc(f.directives, func(a, b logDirective) int {
		return len(b.target) - len(a.target)
	})
}

// tracingLevel is the tracing level a slog level falls in.
func tracingLevel(level slog.Level) slog.Level {
	switch {
	case level >= slog.LevelError:
		return slog.LevelError
	case level >= slog.LevelWarn:
		return slog.LevelWarn
	case level >= slog.LevelInfo:
		return slog.LevelInfo
	case level >= slog.LevelDebug:
		return slog.LevelDebug
	}
	return levelTrace
}

// enabled reports whether a record from target at level passes the filter.
// A target no directive matches passes nothing, as an EnvFilter without a
// default directive does.
func (f logFilter) enabled(target string, level slog.Level) bool {
	for _, directive := range f.directives {
		if directive.matchesNothing || !strings.HasPrefix(target, directive.target) {
			continue
		}
		return !directive.off && tracingLevel(level) >= directive.level
	}
	return false
}

// maxLevel is the most verbose level any directive lets through, which is
// all slog.Handler.Enabled can decide before the target is known.
func (f logFilter) maxLevel() slog.Level {
	most := slog.Level(math.MaxInt)
	for _, directive := range f.directives {
		if !directive.off && !directive.matchesNothing {
			most = min(most, directive.level)
		}
	}
	return most
}

// stderrLogHandler is the slog.Handler installStderrLogging installs: each
// record that passes its filter becomes one line in tracing's fmt layout
// without ANSI, `<RFC 3339 UTC time with microseconds>  <LEVEL right-aligned
// to 5> <target>: <message> <key>=<value>...`, written to writer.
type stderrLogHandler struct {
	filter logFilter
	writer core.ResilientWriter
	// attrs are the attributes from WithAttrs, already qualified by the
	// groups open when they were added.
	attrs []slog.Attr
	// groups are the open groups from WithGroup, which qualify later keys
	// as `group.key`.
	groups []string
}

func (h *stderrLogHandler) Enabled(_ context.Context, level slog.Level) bool {
	return level >= h.filter.maxLevel()
}

func (h *stderrLogHandler) Handle(_ context.Context, record slog.Record) error {
	target := recordTarget(h.attrs, record)
	if !h.filter.enabled(target, record.Level) {
		return nil
	}
	at := record.Time
	if at.IsZero() {
		at = time.Now()
	}
	var b strings.Builder
	b.WriteString(at.UTC().Format("2006-01-02T15:04:05.000000Z"))
	b.WriteString(" ")
	switch tracingLevel(record.Level) {
	case slog.LevelError:
		b.WriteString("ERROR")
	case slog.LevelWarn:
		b.WriteString(" WARN")
	case slog.LevelInfo:
		b.WriteString(" INFO")
	case slog.LevelDebug:
		b.WriteString("DEBUG")
	default:
		b.WriteString("TRACE")
	}
	b.WriteString(" " + target + ": ")
	first := true
	pad := func() {
		if !first {
			b.WriteString(" ")
		}
		first = false
	}
	if record.Message != "" {
		pad()
		b.WriteString(record.Message)
	}
	targetTaken := false
	field := func(attr slog.Attr) {
		if attr.Key == "target" && !targetTaken && attr.Value.Kind() == slog.KindString && attr.Value.String() == target {
			// The target is metadata, not a field.
			targetTaken = true
			return
		}
		writeLogField(&b, pad, "", attr)
	}
	for _, attr := range h.attrs {
		field(attr)
	}
	recordAttrs := make([]slog.Attr, 0, record.NumAttrs())
	record.Attrs(func(attr slog.Attr) bool {
		recordAttrs = append(recordAttrs, attr)
		return true
	})
	for _, attr := range qualifyLogAttrs(h.groups, recordAttrs) {
		field(attr)
	}
	b.WriteString("\n")
	_, _ = h.writer.Write([]byte(b.String()))
	return nil
}

// writeLogField writes one field as tracing's DefaultVisitor does:
// `key=value`, a string in Rust's Debug quoting, an error or other value in
// its display form. Groups flatten to `group.key`.
func writeLogField(b *strings.Builder, pad func(), prefix string, attr slog.Attr) {
	value := attr.Value.Resolve()
	if value.Kind() == slog.KindGroup {
		for _, member := range value.Group() {
			key := attr.Key
			if key != "" {
				key += "."
			}
			writeLogField(b, pad, prefix+key, member)
		}
		return
	}
	if attr.Key == "" && value.Any() == nil {
		return
	}
	pad()
	b.WriteString(prefix + attr.Key + "=" + formatLogValue(value))
}

// formatLogValue is a field's value as tracing's fmt layer prints it.
func formatLogValue(value slog.Value) string {
	switch value.Kind() {
	case slog.KindString:
		return core.RustDebugString(value.String())
	case slog.KindInt64:
		return strconv.FormatInt(value.Int64(), 10)
	case slog.KindUint64:
		return strconv.FormatUint(value.Uint64(), 10)
	case slog.KindFloat64:
		text := strconv.FormatFloat(value.Float64(), 'f', -1, 64)
		if !strings.ContainsAny(text, ".eEnN") {
			text += ".0"
		}
		return text
	case slog.KindBool:
		return strconv.FormatBool(value.Bool())
	case slog.KindDuration:
		return value.Duration().String()
	case slog.KindTime:
		return value.Time().UTC().Format(time.RFC3339Nano)
	}
	switch any := value.Any().(type) {
	case error:
		return any.Error()
	case fmt.Stringer:
		return any.String()
	case nil:
		return "None"
	default:
		return fmt.Sprintf("%+v", any)
	}
}

func (h *stderrLogHandler) WithAttrs(attrs []slog.Attr) slog.Handler {
	if len(attrs) == 0 {
		return h
	}
	next := *h
	next.attrs = append(slices.Clip(h.attrs), qualifyLogAttrs(h.groups, attrs)...)
	return &next
}

func (h *stderrLogHandler) WithGroup(name string) slog.Handler {
	if name == "" {
		return h
	}
	next := *h
	next.groups = append(slices.Clip(h.groups), name)
	return &next
}

// qualifyLogAttrs prefixes each attribute's key with the open groups,
// joined by dots.
func qualifyLogAttrs(groups []string, attrs []slog.Attr) []slog.Attr {
	if len(groups) == 0 {
		return attrs
	}
	prefix := strings.Join(groups, ".") + "."
	qualified := make([]slog.Attr, 0, len(attrs))
	for _, attr := range attrs {
		if attr.Key == "" && attr.Value.Kind() != slog.KindGroup {
			continue
		}
		attr.Key = prefix + attr.Key
		qualified = append(qualified, attr)
	}
	return qualified
}

// recordTarget is a record's target: its "target" attribute (the handler's
// or the record's own), or else the Rust crate name of the Go package whose
// function logged it (`go/internal/worker` is `mj_worker`).
func recordTarget(handlerAttrs []slog.Attr, record slog.Record) string {
	target := ""
	record.Attrs(func(attr slog.Attr) bool {
		if attr.Key == "target" && attr.Value.Kind() == slog.KindString {
			target = attr.Value.String()
			return false
		}
		return true
	})
	if target != "" {
		return target
	}
	for _, attr := range handlerAttrs {
		if attr.Key == "target" && attr.Value.Kind() == slog.KindString {
			return attr.Value.String()
		}
	}
	if record.PC == 0 {
		return "mj_worker"
	}
	frame, _ := runtime.CallersFrames([]uintptr{record.PC}).Next()
	return goPackageCrate(frame.Function)
}

// mjolnirPackagePrefix is the import path of Mjolnir's Go packages.
const mjolnirPackagePrefix = "github.com/BrokkAi/mjolnir/go/"

// goPackageCrates are the Go packages whose crate name is not `mj_` and
// the package name.
var goPackageCrates = map[string]string{
	"cmd/mj-worker":            "mj_worker",
	"internal/workerbin":       "mj_worker",
	"internal/workermcp":       "mj_worker_mcp",
	"internal/workerreview":    "mj_worker_review",
	"internal/harnessinstall":  "mj_harness_install",
	"internal/buildid":         "mj_build_id",
	"internal/sessiontypes":    "mj_session_types",
	"internal/githubrelevance": "mj_github_relevance",
}

// goPackageCrate is the Rust crate a Go function's package ports, from the
// function's full name (`github.com/.../internal/worker.(*X).run`).
func goPackageCrate(function string) string {
	slash := strings.LastIndexByte(function, '/')
	dot := strings.IndexByte(function[slash+1:], '.')
	path := function
	if dot >= 0 {
		path = function[:slash+1+dot]
	}
	if local, ok := strings.CutPrefix(path, mjolnirPackagePrefix); ok {
		if crate, ok := goPackageCrates[local]; ok {
			return crate
		}
		return "mj_" + local[strings.LastIndexByte(local, '/')+1:]
	}
	name := path[strings.LastIndexByte(path, '/')+1:]
	return strings.NewReplacer("-", "_", ".", "_").Replace(name)
}
