package anyhow

import (
	"strconv"
	"strings"
	"testing"

	"github.com/BrokkAi/mjolnir/go/rs"
)

// The types below stand in for the Rust types the crate's tests and the
// oracle module (go/port/oracle/src/anyhow_oracle.rs) use. Each prints what
// its Rust counterpart prints.

// ioError stands in for std::io::Error in its three representations.
type ioError struct {
	repr string // "os", "custom" or "simple"
	code int32
	kind ioKind
	text string
}

// ioKind is std::io::ErrorKind: its Debug form is the variant's name.
type ioKind string

func (k ioKind) FmtDebug(f *rs.Formatter) error { return f.WriteStr(string(k)) }

// osErrors holds what Rust reports for the error numbers the tests use,
// which mean the same on Linux and macOS.
var osErrors = map[int32]struct {
	message string
	kind    ioKind
}{
	1:  {"Operation not permitted", "PermissionDenied"},
	2:  {"No such file or directory", "NotFound"},
	9:  {"Bad file descriptor", "Uncategorized"},
	13: {"Permission denied", "PermissionDenied"},
	17: {"File exists", "AlreadyExists"},
	21: {"Is a directory", "IsADirectory"},
	22: {"Invalid argument", "InvalidInput"},
	28: {"No space left on device", "StorageFull"},
	32: {"Broken pipe", "BrokenPipe"},
}

// kindDescriptions is ErrorKind::description for the kinds the tests use.
var kindDescriptions = map[ioKind]string{
	"NotFound":          "entity not found",
	"PermissionDenied":  "permission denied",
	"ConnectionRefused": "connection refused",
	"AlreadyExists":     "entity already exists",
	"InvalidInput":      "invalid input parameter",
	"InvalidData":       "invalid data",
	"TimedOut":          "timed out",
	"UnexpectedEof":     "unexpected end of file",
	"BrokenPipe":        "broken pipe",
	"Other":             "other error",
}

func ioErrorFromRawOsError(code int32) *ioError {
	known, ok := osErrors[code]
	if !ok {
		panic("support_test.go has no text for os error " + strconv.Itoa(int(code)))
	}
	return &ioError{repr: "os", code: code, kind: known.kind, text: known.message}
}

// ioErrorNew is io::Error::new(kind, text).
func ioErrorNew(kind ioKind, text string) *ioError {
	return &ioError{repr: "custom", kind: kind, text: text}
}

// ioErrorFromKind is io::Error::from(kind).
func ioErrorFromKind(kind ioKind) *ioError {
	description, ok := kindDescriptions[kind]
	if !ok {
		panic("support_test.go has no description for " + string(kind))
	}
	return &ioError{repr: "simple", kind: kind, text: description}
}

func (e *ioError) Error() string {
	if e.repr == "os" {
		return e.text + " (os error " + strconv.Itoa(int(e.code)) + ")"
	}
	return e.text
}

func (e *ioError) FmtDisplay(f *rs.Formatter) error {
	if e.repr == "custom" {
		// The inner error is a string, whose Display pads.
		return f.Pad(e.text)
	}
	// `write!(fmt, ..)`: no padding.
	return f.WriteStr(e.Error())
}

func (e *ioError) FmtDebug(f *rs.Formatter) error {
	switch e.repr {
	case "os":
		return f.DebugStruct("Os").Field("code", e.code).Field("kind", e.kind).Field("message", e.text).Finish()
	case "custom":
		return f.DebugStruct("Custom").Field("kind", e.kind).Field("error", e.text).Finish()
	default:
		return f.DebugTuple("Kind").Field(e.kind).Finish()
	}
}

// typedError stands in for an error type product code defines: Display is
// its text, Debug is derived, and it reports its source through Unwrap, as a
// ported `impl std::error::Error` does.
type typedError struct {
	text   string
	source rs.Option[*typedError]
}

func (e *typedError) Error() string { return e.text }

func (e *typedError) FmtDisplay(f *rs.Formatter) error { return f.WriteStr(e.text) }

func (e *typedError) FmtDebug(f *rs.Formatter) error {
	return f.DebugStruct("Typed").Field("text", e.text).Field("source", e.source).Finish()
}

func (e *typedError) Unwrap() error {
	if inner, ok := e.source.Get(); ok {
		return inner
	}
	return nil
}

// goError is the same error holding its source as a plain pointer. Chain must
// treat the two alike.
type goError struct {
	text   string
	source *goError
}

func (e *goError) Error() string { return e.text }

func (e *goError) FmtDisplay(f *rs.Formatter) error { return f.WriteStr(e.text) }

func (e *goError) FmtDebug(f *rs.Formatter) error {
	source := rs.None[*goError]()
	if e.source != nil {
		source = rs.Some(e.source)
	}
	return f.DebugStruct("Typed").Field("text", e.text).Field("source", source).Finish()
}

func (e *goError) Unwrap() error {
	if e.source == nil {
		// A nil *goError in an error would not be nil.
		return nil
	}
	return e.source
}

// newTyped builds the typed error whose chain has the given texts.
func newTyped(texts []string) *typedError {
	var err *typedError
	for i := len(texts) - 1; i >= 0; i-- {
		next := &typedError{text: texts[i]}
		if err != nil {
			next.source = rs.Some(err)
		}
		err = next
	}
	return err
}

func newGoError(texts []string) *goError {
	var err *goError
	for i := len(texts) - 1; i >= 0; i-- {
		err = &goError{text: texts[i], source: err}
	}
	return err
}

// typedCtx is a context value that is not an error.
type typedCtx struct {
	text string
}

func (c typedCtx) String() string { return c.text }

func (c typedCtx) FmtDisplay(f *rs.Formatter) error { return f.WriteStr(c.text) }

func (c typedCtx) FmtDebug(f *rs.Formatter) error {
	return f.DebugStruct("TypedCtx").Field("text", c.text).Finish()
}

// typedMsg is a message value for Msg that is neither a string nor an error.
type typedMsg struct {
	text string
}

func (m typedMsg) String() string { return m.text }

func (m typedMsg) FmtDisplay(f *rs.Formatter) error { return f.WriteStr(m.text) }

func (m typedMsg) FmtDebug(f *rs.Formatter) error {
	return f.DebugStruct("TypedMsg").Field("text", m.text).Finish()
}

// stringError is what `Box::<dyn Error + Send + Sync>::from("..")` holds: a
// string that is an error, printing as the string does.
type stringError string

func (e stringError) Error() string { return string(e) }

func (e stringError) FmtDisplay(f *rs.Formatter) error { return f.Pad(string(e)) }

func (e stringError) FmtDebug(f *rs.Formatter) error { return rs.DebugFmt(string(e), f) }

// formatterProbe prints through every part of the formatter the tests of this
// package rely on.
type formatterProbe struct{}

func (formatterProbe) FmtDebug(f *rs.Formatter) error {
	if f.Alternate() {
		return f.DebugStruct("Probe").Field("text", "x").Finish()
	}
	return f.WriteStr("probe")
}

// formatterPorted reports whether rs.Format and rs.Formatter are implemented
// yet. Until they are, they panic with "not yet ported".
func formatterPorted() (ported bool) {
	defer func() {
		if recovered := recover(); recovered != nil {
			if strings.HasPrefix(rs.PanicInfoOf(recovered).Message, "not yet ported: core::fmt") {
				ported = false
				return
			}
			panic(recovered)
		}
	}()
	_ = rs.Format("{:#?} {:>4}", formatterProbe{}, "x")
	return true
}

// requireFormatter skips a test that cannot say anything until the formatter
// exists: one whose input is a format string with arguments, or whose
// expected text is a Debug form.
func requireFormatter(t *testing.T) {
	t.Helper()
	if !formatterPorted() {
		t.Skip("waiting for rs.Format and rs.Formatter (go/rs/formatter_pending.go)")
	}
}

func assertEq[T comparable](t *testing.T, left, right T) {
	t.Helper()
	if left != right {
		t.Fatalf("assertion `left == right` failed\n  left: %#v\n right: %#v", left, right)
	}
}

func assert(t *testing.T, condition bool) {
	t.Helper()
	if !condition {
		t.Fatal("assertion failed")
	}
}
