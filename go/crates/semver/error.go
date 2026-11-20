// Port of semver 1.0.28 src/error.rs

package semver

import (
	"github.com/BrokkAi/mjolnir/go/rs"
	"github.com/BrokkAi/mjolnir/go/rs/strs"
)

type errorErrorKind interface {
	isErrorErrorKind()
}

type errorErrorKind_Empty struct{}

type errorErrorKind_UnexpectedEnd struct {
	F0 errorPosition
}

type errorErrorKind_UnexpectedChar struct {
	F0 errorPosition
	F1 rune
}

type errorErrorKind_UnexpectedCharAfter struct {
	F0 errorPosition
	F1 rune
}

type errorErrorKind_ExpectedCommaFound struct {
	F0 errorPosition
	F1 rune
}

type errorErrorKind_LeadingZero struct {
	F0 errorPosition
}

type errorErrorKind_Overflow struct {
	F0 errorPosition
}

type errorErrorKind_EmptySegment struct {
	F0 errorPosition
}

type errorErrorKind_IllegalCharacter struct {
	F0 errorPosition
}

type errorErrorKind_WildcardNotTheOnlyComparator struct {
	F0 rune
}

type errorErrorKind_UnexpectedAfterWildcard struct{}

type errorErrorKind_ExcessiveComparators struct{}

func (*errorErrorKind_Empty) isErrorErrorKind()                        {}
func (*errorErrorKind_UnexpectedEnd) isErrorErrorKind()                {}
func (*errorErrorKind_UnexpectedChar) isErrorErrorKind()               {}
func (*errorErrorKind_UnexpectedCharAfter) isErrorErrorKind()          {}
func (*errorErrorKind_ExpectedCommaFound) isErrorErrorKind()           {}
func (*errorErrorKind_LeadingZero) isErrorErrorKind()                  {}
func (*errorErrorKind_Overflow) isErrorErrorKind()                     {}
func (*errorErrorKind_EmptySegment) isErrorErrorKind()                 {}
func (*errorErrorKind_IllegalCharacter) isErrorErrorKind()             {}
func (*errorErrorKind_WildcardNotTheOnlyComparator) isErrorErrorKind() {}
func (*errorErrorKind_UnexpectedAfterWildcard) isErrorErrorKind()      {}
func (*errorErrorKind_ExcessiveComparators) isErrorErrorKind()         {}

type errorPosition uint8

const (
	errorPosition_Major errorPosition = iota
	errorPosition_Minor
	errorPosition_Patch
	errorPosition_Pre
	errorPosition_Build
)

// Error makes an Error a Go error. `impl std::error::Error for Error` adds
// nothing to Display: the error has no source.
func (self *Error) Error() string {
	return self.String()
}

// String is the `Display` output: the message.
func (self *Error) String() string {
	switch kind := self.kind.(type) {
	case *errorErrorKind_Empty:
		return "empty string, expected a semver version"
	case *errorErrorKind_UnexpectedEnd:
		pos := kind.F0
		return "unexpected end of input while parsing " + pos.String()
	case *errorErrorKind_UnexpectedChar:
		pos, ch := kind.F0, kind.F1
		return "unexpected character " + errorQuotedChar(ch).String() + " while parsing " + pos.String()
	case *errorErrorKind_UnexpectedCharAfter:
		pos, ch := kind.F0, kind.F1
		return "unexpected character " + errorQuotedChar(ch).String() + " after " + pos.String()
	case *errorErrorKind_ExpectedCommaFound:
		pos, ch := kind.F0, kind.F1
		return "expected comma after " + pos.String() + ", found " + errorQuotedChar(ch).String()
	case *errorErrorKind_LeadingZero:
		pos := kind.F0
		return "invalid leading zero in " + pos.String()
	case *errorErrorKind_Overflow:
		pos := kind.F0
		return "value of " + pos.String() + " exceeds u64::MAX"
	case *errorErrorKind_EmptySegment:
		pos := kind.F0
		return "empty identifier segment in " + pos.String()
	case *errorErrorKind_IllegalCharacter:
		pos := kind.F0
		return "unexpected character in " + pos.String()
	case *errorErrorKind_WildcardNotTheOnlyComparator:
		ch := kind.F0
		return "wildcard req (" + string(ch) + ") must be the only comparator in the version req"
	case *errorErrorKind_UnexpectedAfterWildcard:
		return "unexpected character after wildcard in version req"
	case *errorErrorKind_ExcessiveComparators:
		return "excessive number of version comparators"
	default:
		panic(rs.Unreachable(kind))
	}
}

// FmtDisplay is `impl Display for Error`. The Rust impl writes with
// `write_str` and `write!`, so width, fill and alignment of the caller's `{}`
// do not apply.
func (self *Error) FmtDisplay(formatter *rs.Formatter) error {
	return formatter.WriteStr(self.String())
}

// String is the `Display` output: what the position is called in a message.
func (self errorPosition) String() string {
	switch self {
	case errorPosition_Major:
		return "major version number"
	case errorPosition_Minor:
		return "minor version number"
	case errorPosition_Patch:
		return "patch version number"
	case errorPosition_Pre:
		return "pre-release identifier"
	case errorPosition_Build:
		return "build metadata"
	default:
		panic(rs.Unreachable(self))
	}
}

// debugString is the `Debug` output: `Error("<message>")`, the message as it
// is, without escapes.
func (self *Error) debugString() string {
	return "Error(\"" + self.String() + "\")"
}

// FmtDebug is `impl Debug for Error`.
func (self *Error) FmtDebug(formatter *rs.Formatter) error {
	if err := formatter.WriteStr("Error(\""); err != nil {
		return err
	}
	if err := self.FmtDisplay(formatter); err != nil {
		return err
	}
	if err := formatter.WriteStr("\")"); err != nil {
		return err
	}
	return nil
}

type errorQuotedChar rune

// String is the `Display` output: the char as `{:?}` prints it.
func (self errorQuotedChar) String() string {
	// Standard library versions prior to https://github.com/rust-lang/rust/pull/95345
	// print character 0 as '\u{0}'. We prefer '\0' to keep error messages
	// the same across all supported Rust versions.
	if self == 0 {
		return "'\\0'"
	} else {
		// `{:?}` of a char is the char between single quotes, escaped as
		// char::escape_debug escapes it, except that a double quote is left
		// alone.
		if self == '"' {
			return "'\"'"
		}
		return "'" + strs.CharEscapeDebug(rune(self)) + "'"
	}
}
