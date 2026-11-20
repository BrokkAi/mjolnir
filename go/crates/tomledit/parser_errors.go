// Port of toml_edit 0.20.2 src/parser/errors.rs

package tomledit

import (
	"strconv"
	"strings"
	"unicode/utf8"

	"github.com/BrokkAi/mjolnir/go/rs"
)

// TomlError is toml_edit::TomlError: type representing a TOML parse error.
type TomlError struct {
	message  string
	original rs.Option[string]
	keys     []string
	span     rs.Option[Range]
}

func tomlErrorNew(e parseErr, original *input) *TomlError {
	offset := original.pos
	var span Range
	if offset == len(original.data) {
		span = Range{Start: offset, End: offset}
	} else {
		span = Range{Start: offset, End: offset + 1}
	}

	message := e.message()

	return &TomlError{
		message:  message,
		original: rs.Some(original.data),
		keys:     nil,
		span:     rs.Some(span),
	}
}

func tomlErrorCustom(message string, span rs.Option[Range]) *TomlError {
	return &TomlError{
		message:  message,
		original: rs.None[string](),
		keys:     nil,
		span:     span,
	}
}

func (e *TomlError) addKey(key string) {
	e.keys = append([]string{key}, e.keys...)
}

// Message is TomlError::message: what went wrong.
func (e *TomlError) Message() string {
	return e.message
}

// Span is TomlError::span: the start/end index into the original document
// where the error occurred.
func (e *TomlError) Span() rs.Option[Range] {
	return e.span
}

func (e *TomlError) setSpan(span rs.Option[Range]) {
	e.span = span
}

func (e *TomlError) setOriginal(original rs.Option[string]) {
	e.original = original
}

// Clone is the derived `Clone for TomlError`.
func (e *TomlError) Clone() TomlError {
	cloned := *e
	cloned.keys = append([]string(nil), e.keys...)
	return cloned
}

// Equal is the derived `PartialEq for TomlError`.
func (e *TomlError) Equal(other *TomlError) bool {
	if e.message != other.message || e.original != other.original || e.span != other.span || len(e.keys) != len(other.keys) {
		return false
	}
	for index := range e.keys {
		if e.keys[index] != other.keys[index] {
			return false
		}
	}
	return true
}

// FmtDebug is the derived `Debug for TomlError`.
func (e *TomlError) FmtDebug(f *rs.Formatter) error {
	return f.DebugStruct("TomlError").Field("message", e.message).Field("original", e.original).
		Field("keys", e.keys).Field("span", e.span).Finish()
}

// Error is `impl Display for TomlError`: displays a TOML parse error.
//
// # Example
//
//	TOML parse error at line 1, column 10
//	  |
//	1 | 00:32:00.a999999
//	  |          ^
//	Unexpected `a`
//	Expected `digit`
//	While parsing a Time
//	While parsing a Date-Time
func (e *TomlError) Error() string {
	var f strings.Builder
	context := false
	original, hasOriginal := e.original.Get()
	span, hasSpan := e.span.Get()
	if hasOriginal && hasSpan {
		context = true

		line, column := translatePosition(original, span.Start)
		lineNum := line + 1
		colNum := column + 1
		gutter := len(strconv.Itoa(lineNum))
		content := nthLine(original, line)

		f.WriteString("TOML parse error at line " + strconv.Itoa(lineNum) + ", column " + strconv.Itoa(colNum) + "\n")
		//   |
		for range gutter + 1 {
			f.WriteString(" ")
		}
		f.WriteString("|\n")

		// 1 | 00:32:00.a999999
		f.WriteString(strconv.Itoa(lineNum) + " | ")
		f.WriteString(content + "\n")

		//   |          ^
		for range gutter + 1 {
			f.WriteString(" ")
		}
		f.WriteString("|")
		for range column + 1 {
			f.WriteString(" ")
		}
		// The span will be empty at eof, so we need to make sure we always print at least
		// one `^`
		f.WriteString("^")
		end := min(span.End, span.Start+len(content))
		for i := span.Start + 1; i < end; i++ {
			f.WriteString("^")
		}
		f.WriteString("\n")
	}
	f.WriteString(e.message + "\n")
	if !context && len(e.keys) != 0 {
		f.WriteString("in `" + strings.Join(e.keys, ".") + "`\n")
	}

	return f.String()
}

// FmtDisplay is `impl Display for TomlError`.
func (e *TomlError) FmtDisplay(f *rs.Formatter) error { return f.WriteStr(e.Error()) }

// nthLine is `original.split('\n').nth(line).expect("valid line number")`.
func nthLine(original string, line int) string {
	rest := original
	for range line {
		newline := strings.IndexByte(rest, '\n')
		if newline < 0 {
			rs.PanicAt(2, "valid line number")
		}
		rest = rest[newline+1:]
	}
	if newline := strings.IndexByte(rest, '\n'); newline >= 0 {
		return rest[:newline]
	}
	return rest
}

func translatePosition(input string, index int) (int, int) {
	if len(input) == 0 {
		return 0, index
	}

	safeIndex := min(index, len(input)-1)
	columnOffset := index - safeIndex
	index = safeIndex

	lineStart := 0
	if nl := strings.LastIndexByte(input[:index], '\n'); nl >= 0 {
		lineStart = nl + 1
	}
	line := strings.Count(input[:lineStart], "\n")

	var column int
	if prefix := input[lineStart : index+1]; utf8.ValidString(prefix) {
		column = utf8.RuneCountInString(prefix) - 1
	} else {
		column = index - lineStart
	}
	column += columnOffset

	return line, column
}

// customError is parser::errors::CustomError. Its variants are
// *customError_DuplicateKey, *customError_DottedKeyExtendWrongType,
// *customError_OutOfRange and *customError_RecursionLimitExceeded.
type customError interface {
	error
	isCustomError()
}

type customError_DuplicateKey struct {
	key   string
	table rs.Option[[]Key]
}

type customError_DottedKeyExtendWrongType struct {
	key    []Key
	actual string
}

type customError_OutOfRange struct{}

type customError_RecursionLimitExceeded struct{}

func (*customError_DuplicateKey) isCustomError()             {}
func (*customError_DottedKeyExtendWrongType) isCustomError() {}
func (*customError_OutOfRange) isCustomError()               {}
func (*customError_RecursionLimitExceeded) isCustomError()   {}

func customErrorDuplicateKey(path []Key, i int) customError {
	rs.Assert(i < len(path))
	key := &path[i]
	repr := key.DisplayRepr()
	return &customError_DuplicateKey{
		key:   repr,
		table: rs.Some(cloneKeys(path[:i])),
	}
}

func customErrorExtendWrongType(path []Key, i int, actual string) customError {
	rs.Assert(i < len(path))
	return &customError_DottedKeyExtendWrongType{
		key:    cloneKeys(path[:i+1]),
		actual: actual,
	}
}

func cloneKeys(keys []Key) []Key {
	return append([]Key{}, keys...)
}

func joinKeys(keys []Key) string {
	parts := make([]string, len(keys))
	for index := range keys {
		parts[index] = keys[index].Get()
	}
	return strings.Join(parts, ".")
}

// Error is `impl Display for CustomError`.
func (e *customError_DuplicateKey) Error() string {
	if table, ok := e.table.Get(); ok {
		if len(table) == 0 {
			return "duplicate key `" + e.key + "` in document root"
		}
		path := joinKeys(table)
		return "duplicate key `" + e.key + "` in table `" + path + "`"
	}
	return "duplicate key `" + e.key + "`"
}

// Error is `impl Display for CustomError`.
func (e *customError_DottedKeyExtendWrongType) Error() string {
	path := joinKeys(e.key)
	return "dotted key `" + path + "` attempted to extend non-table type (" + e.actual + ")"
}

// Error is `impl Display for CustomError`.
func (*customError_OutOfRange) Error() string { return "value is out of range" }

// Error is `impl Display for CustomError`.
func (*customError_RecursionLimitExceeded) Error() string { return "recursion limit exceeded" }
