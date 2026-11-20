// Port of serde_json 1.0.150 src/error.rs

package serdejson

import (
	"strconv"
	"strings"

	"github.com/BrokkAi/mjolnir/go/crates/serde"
	"github.com/BrokkAi/mjolnir/go/rs"
)

// Error is `serde_json::Error`: everything that can go wrong when
// serializing or deserializing JSON. Functions return it as an `error`; get
// it back with errors.As or a type assertion to *Error.
type Error struct {
	code   errorCode
	line   int
	column int
}

// Line is `Error::line`: the one-based line on which the error was detected,
// or zero when the error has no position.
func (e *Error) Line() int { return e.line }

// Column is `Error::column`: the one-based column of the first character
// that could not be read, or zero.
func (e *Error) Column() int { return e.column }

// Category is `serde_json::error::Category`: the cause of an error.
type Category uint8

const (
	// CategoryIo is a failure to read or write bytes on an I/O stream.
	CategoryIo Category = iota
	// CategorySyntax is input that is not syntactically valid JSON.
	CategorySyntax
	// CategoryData is input that is valid JSON but semantically wrong for
	// the type being read.
	CategoryData
	// CategoryEof is an unexpected end of the input.
	CategoryEof
)

var _ = rs.RegisterEnum[Category]("Category",
	rs.Const(CategoryIo, "Io"),
	rs.Const(CategorySyntax, "Syntax"),
	rs.Const(CategoryData, "Data"),
	rs.Const(CategoryEof, "Eof"),
)

// Classify is `Error::classify`.
func (e *Error) Classify() Category {
	switch e.code.kind {
	case errorCodeMessage:
		return CategoryData
	case errorCodeIo:
		return CategoryIo
	case errorCodeEofWhileParsingList,
		errorCodeEofWhileParsingObject,
		errorCodeEofWhileParsingString,
		errorCodeEofWhileParsingValue:
		return CategoryEof
	}
	return CategorySyntax
}

// IsIo is `Error::is_io`.
func (e *Error) IsIo() bool { return e.Classify() == CategoryIo }

// IsSyntax is `Error::is_syntax`.
func (e *Error) IsSyntax() bool { return e.Classify() == CategorySyntax }

// IsData is `Error::is_data`.
func (e *Error) IsData() bool { return e.Classify() == CategoryData }

// IsEof is `Error::is_eof`.
func (e *Error) IsEof() bool { return e.Classify() == CategoryEof }

// IoError returns the I/O error that caused e, or nil when e is not an I/O
// error. `Error::io_error_kind` is `rsio.KindOf(e.IoError())`.
func (e *Error) IoError() error {
	if e.code.kind == errorCodeIo {
		return e.code.io
	}
	return nil
}

// Unwrap returns the I/O error behind e, which is what Rust's
// `Error::source` leads to.
func (e *Error) Unwrap() error { return e.IoError() }

// errorCodeKind is the variant of the Rust enum `ErrorCode`.
type errorCodeKind uint8

const (
	// errorCodeMessage is a catchall for syntax error messages.
	errorCodeMessage errorCodeKind = iota
	// errorCodeIo is some I/O error that occurred while serializing or
	// deserializing.
	errorCodeIo
	// errorCodeEofWhileParsingList is EOF while parsing a list.
	errorCodeEofWhileParsingList
	// errorCodeEofWhileParsingObject is EOF while parsing an object.
	errorCodeEofWhileParsingObject
	// errorCodeEofWhileParsingString is EOF while parsing a string.
	errorCodeEofWhileParsingString
	// errorCodeEofWhileParsingValue is EOF while parsing a JSON value.
	errorCodeEofWhileParsingValue
	// errorCodeExpectedColon is expected this character to be a `':'`.
	errorCodeExpectedColon
	// errorCodeExpectedListCommaOrEnd is expected this character to be
	// either a `','` or a `']'`.
	errorCodeExpectedListCommaOrEnd
	// errorCodeExpectedObjectCommaOrEnd is expected this character to be
	// either a `','` or a `'}'`.
	errorCodeExpectedObjectCommaOrEnd
	// errorCodeExpectedSomeIdent is expected to parse either a `true`,
	// `false`, or a `null`.
	errorCodeExpectedSomeIdent
	// errorCodeExpectedSomeValue is expected this character to start a JSON
	// value.
	errorCodeExpectedSomeValue
	// errorCodeExpectedDoubleQuote is expected this character to be a `"`.
	errorCodeExpectedDoubleQuote
	// errorCodeInvalidEscape is invalid hex escape code.
	errorCodeInvalidEscape
	// errorCodeInvalidNumber is invalid number.
	errorCodeInvalidNumber
	// errorCodeNumberOutOfRange is number is bigger than the maximum value
	// of its type.
	errorCodeNumberOutOfRange
	// errorCodeInvalidUnicodeCodePoint is invalid unicode code point.
	errorCodeInvalidUnicodeCodePoint
	// errorCodeControlCharacterWhileParsingString is control character found
	// while parsing a string.
	errorCodeControlCharacterWhileParsingString
	// errorCodeKeyMustBeAString is object key is not a string.
	errorCodeKeyMustBeAString
	// errorCodeExpectedNumericKey is contents of key were supposed to be a
	// number.
	errorCodeExpectedNumericKey
	// errorCodeFloatKeyMustBeFinite is object key is a non-finite float
	// value.
	errorCodeFloatKeyMustBeFinite
	// errorCodeLoneLeadingSurrogateInHexEscape is lone leading surrogate in
	// hex escape.
	errorCodeLoneLeadingSurrogateInHexEscape
	// errorCodeTrailingComma is JSON has a comma after the last value in an
	// array or map.
	errorCodeTrailingComma
	// errorCodeTrailingCharacters is JSON has non-whitespace trailing
	// characters after the value.
	errorCodeTrailingCharacters
	// errorCodeUnexpectedEndOfHexEscape is unexpected end of hex escape.
	errorCodeUnexpectedEndOfHexEscape
	// errorCodeRecursionLimitExceeded is encountered nesting of JSON maps
	// and arrays more than 128 layers deep.
	errorCodeRecursionLimitExceeded
)

// errorCode is the Rust enum `ErrorCode`: the kind, with the message of a
// Message and the error of an Io.
type errorCode struct {
	kind errorCodeKind
	msg  string
	io   error
}

// String is the Display form of the code.
func (c errorCode) String() string {
	switch c.kind {
	case errorCodeMessage:
		return c.msg
	case errorCodeIo:
		return c.io.Error()
	case errorCodeEofWhileParsingList:
		return "EOF while parsing a list"
	case errorCodeEofWhileParsingObject:
		return "EOF while parsing an object"
	case errorCodeEofWhileParsingString:
		return "EOF while parsing a string"
	case errorCodeEofWhileParsingValue:
		return "EOF while parsing a value"
	case errorCodeExpectedColon:
		return "expected `:`"
	case errorCodeExpectedListCommaOrEnd:
		return "expected `,` or `]`"
	case errorCodeExpectedObjectCommaOrEnd:
		return "expected `,` or `}`"
	case errorCodeExpectedSomeIdent:
		return "expected ident"
	case errorCodeExpectedSomeValue:
		return "expected value"
	case errorCodeExpectedDoubleQuote:
		return "expected `\"`"
	case errorCodeInvalidEscape:
		return "invalid escape"
	case errorCodeInvalidNumber:
		return "invalid number"
	case errorCodeNumberOutOfRange:
		return "number out of range"
	case errorCodeInvalidUnicodeCodePoint:
		return "invalid unicode code point"
	case errorCodeControlCharacterWhileParsingString:
		return "control character (\\u0000-\\u001F) found while parsing a string"
	case errorCodeKeyMustBeAString:
		return "key must be a string"
	case errorCodeExpectedNumericKey:
		return "invalid value: expected key to be a number in quotes"
	case errorCodeFloatKeyMustBeFinite:
		return "float key must be finite (got NaN or +/-inf)"
	case errorCodeLoneLeadingSurrogateInHexEscape:
		return "lone leading surrogate in hex escape"
	case errorCodeTrailingComma:
		return "trailing comma"
	case errorCodeTrailingCharacters:
		return "trailing characters"
	case errorCodeUnexpectedEndOfHexEscape:
		return "unexpected end of hex escape"
	case errorCodeRecursionLimitExceeded:
		return "recursion limit exceeded"
	}
	panic(rs.Unreachable(c.kind))
}

// errorSyntax is `Error::syntax(code, line, column)`.
func errorSyntax(kind errorCodeKind, line int, column int) *Error {
	return &Error{code: errorCode{kind: kind}, line: line, column: column}
}

// errorIo is `Error::io(error)`.
func errorIo(err error) *Error {
	return &Error{code: errorCode{kind: errorCodeIo, io: err}}
}

// fixPosition is `Error::fix_position`: an error with no line takes the
// position f gives its code.
func (e *Error) fixPosition(f func(code errorCode) *Error) *Error {
	if e.line == 0 {
		return f(e.code)
	}
	return e
}

// Error is the Display form: the message, followed by " at line L column C"
// when the error has a position.
func (e *Error) Error() string {
	if e.line == 0 {
		return e.code.String()
	}
	return e.code.String() + " at line " + strconv.Itoa(e.line) + " column " + strconv.Itoa(e.column)
}

// String is the Display form.
func (e *Error) String() string { return e.Error() }

// FmtDisplay is `impl Display for Error`.
func (e *Error) FmtDisplay(f *rs.Formatter) error { return f.WriteStr(e.Error()) }

// DebugString is the Debug form, with two layers of verbosity removed
// because humans often end up seeing it: it is what unwrap() shows.
func (e *Error) DebugString() string {
	return "Error(" + serde.DebugStr(e.code.String()) + ", line: " + strconv.Itoa(e.line) + ", column: " + strconv.Itoa(e.column) + ")"
}

// FmtDebug is `impl Debug for Error`.
func (e *Error) FmtDebug(f *rs.Formatter) error { return f.WriteStr(e.DebugString()) }

// ErrorCustom is `<serde_json::Error as de::Error>::custom(msg)` and the
// same function of `ser::Error`: an error with the message msg, which is a
// string, an error or anything with a String method.
//
// A message that ends in " at line L column C", which is how this type
// prints its own position, becomes an error at that position with the rest
// as its message, as in Rust (it is there for errors that went through
// another error type as text).
func ErrorCustom(msg any) *Error {
	switch value := msg.(type) {
	case string:
		return makeError(value)
	case error:
		return makeError(value.Error())
	case interface{ String() string }:
		return makeError(value.String())
	}
	return makeError(rs.ToString(msg))
}

// toError gives an error the form this format gives it. An *Error is
// returned as it is. A serde error that no format has taken yet (a
// *serde.Error, which is what visitors and Deserialize impls return) becomes
// what serde_json's own `de::Error` and `ser::Error` impls would have made:
// invalid_type and invalid_value print `null` for a unit and a float as JSON
// writes it, and everything else is custom(the default message). Any other
// error is taken by its text.
func toError(err error) *Error {
	switch e := err.(type) {
	case *Error:
		return e
	case *serde.Error:
		switch e.Kind {
		case serde.ErrorKindInvalidType:
			return makeError("invalid type: " + jsonUnexpected(e.Unexpected) + ", expected " + e.Expected)
		case serde.ErrorKindInvalidValue:
			return makeError("invalid value: " + jsonUnexpected(e.Unexpected) + ", expected " + e.Expected)
		}
		return makeError(e.Error())
	}
	return makeError(err.Error())
}

// jsonUnexpected is the Display of `JsonUnexpected`: a unit is `null`, and a
// float prints as JSON writes it.
func jsonUnexpected(unexp serde.DeUnexpected) string {
	switch u := unexp.(type) {
	case *serde.DeUnexpectedUnit:
		return "null"
	case *serde.DeUnexpectedFloat:
		return "floating point `" + formatF64(u.F0) + "`"
	}
	return unexp.String()
}

// makeError parses our own error message that looks like "{} at line {}
// column {}" to work around erased-serde round-tripping the error through
// de::Error::custom.
func makeError(msg string) *Error {
	line, column, ok := parseLineCol(&msg)
	if !ok {
		line, column = 0, 0
	}
	return &Error{code: errorCode{kind: errorCodeMessage, msg: msg}, line: line, column: column}
}

func parseLineCol(msg *string) (line int, column int, ok bool) {
	startOfSuffix := strings.LastIndex(*msg, " at line ")
	if startOfSuffix < 0 {
		return 0, 0, false
	}

	// Find start and end of line number.
	startOfLine := startOfSuffix + len(" at line ")
	endOfLine := startOfLine
	for startsWithDigit((*msg)[endOfLine:]) {
		endOfLine++
	}

	if !strings.HasPrefix((*msg)[endOfLine:], " column ") {
		return 0, 0, false
	}

	// Find start and end of column number.
	startOfColumn := endOfLine + len(" column ")
	endOfColumn := startOfColumn
	for startsWithDigit((*msg)[endOfColumn:]) {
		endOfColumn++
	}

	if endOfColumn < len(*msg) {
		return 0, 0, false
	}

	// Parse numbers. usize::from_str refuses an empty string and a value
	// that does not fit.
	parsedLine, err := strconv.ParseUint((*msg)[startOfLine:endOfLine], 10, 64)
	if err != nil {
		return 0, 0, false
	}
	parsedColumn, err := strconv.ParseUint((*msg)[startOfColumn:endOfColumn], 10, 64)
	if err != nil {
		return 0, 0, false
	}
	if parsedLine > uint64(rsUsizeMax) || parsedColumn > uint64(rsUsizeMax) {
		return 0, 0, false
	}

	*msg = (*msg)[:startOfSuffix]
	return int(parsedLine), int(parsedColumn), true
}

// rsUsizeMax is usize::MAX as Go holds a usize: the largest int.
const rsUsizeMax = int(^uint(0) >> 1)

func startsWithDigit(slice string) bool {
	return len(slice) > 0 && slice[0] >= '0' && slice[0] <= '9'
}
