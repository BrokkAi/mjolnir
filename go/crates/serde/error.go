// Port of serde_core 1.0.229 src/de/mod.rs (Error, Unexpected, Expected,
// OneOf, WithDecimalPoint) and src/ser/mod.rs (Error).

package serde

import (
	"fmt"
	"math"
	"strconv"
	"strings"
)

// ErrorKind says which constructor of serde's `de::Error` (or `ser::Error`)
// made an Error.
type ErrorKind uint8

const (
	// ErrorKindCustom is `Error::custom(msg)`.
	ErrorKindCustom ErrorKind = iota
	// ErrorKindInvalidType is `Error::invalid_type(unexp, exp)`.
	ErrorKindInvalidType
	// ErrorKindInvalidValue is `Error::invalid_value(unexp, exp)`.
	ErrorKindInvalidValue
	// ErrorKindInvalidLength is `Error::invalid_length(len, exp)`.
	ErrorKindInvalidLength
	// ErrorKindUnknownVariant is `Error::unknown_variant(variant, expected)`.
	ErrorKindUnknownVariant
	// ErrorKindUnknownField is `Error::unknown_field(field, expected)`.
	ErrorKindUnknownField
	// ErrorKindMissingField is `Error::missing_field(field)`.
	ErrorKindMissingField
	// ErrorKindDuplicateField is `Error::duplicate_field(field)`.
	ErrorKindDuplicateField
)

// Error is a serde error that no format has taken yet.
//
// In Rust the error type belongs to the format (`D::Error`, `S::Error`), and
// a visitor or a `Deserialize` impl makes one through the trait's
// constructors: `de::Error::custom`, `invalid_type`, `missing_field`, ... A
// Go visitor has no type parameter to make the format's error with, so it
// returns an *Error, which records which constructor was called and with
// what. Every format turns an *Error into its own error type at the first
// point the error passes back through it (a Deserializer, SeqAccess,
// MapAccess, EnumAccess or VariantAccess method, or the format's entry
// point), using the constructor's arguments where the Rust format overrides
// that constructor (serde_json overrides invalid_type and invalid_value) and
// Error() otherwise.
//
// Error() is the text serde's default implementation of the constructor
// produces. That makes *Error itself `serde::de::value::Error`, the error
// type of the deserializers in de_value.go.
type Error struct {
	// Kind is the constructor that was called.
	Kind ErrorKind
	// Msg is the message of a custom error.
	Msg string
	// Unexpected is the first argument of invalid_type and invalid_value.
	Unexpected DeUnexpected
	// Expected is the rendered `exp` argument of invalid_type, invalid_value
	// and invalid_length.
	Expected string
	// Len is the first argument of invalid_length.
	Len int
	// Name is the variant or field of unknown_variant, unknown_field,
	// missing_field and duplicate_field.
	Name string
	// Names is the `expected` argument of unknown_variant and unknown_field.
	Names []string
}

// DeValueError is `serde::de::value::Error`.
type DeValueError = Error

// Error returns the message serde's default implementation of the
// constructor writes.
func (e *Error) Error() string {
	switch e.Kind {
	case ErrorKindCustom:
		return e.Msg
	case ErrorKindInvalidType:
		return "invalid type: " + e.Unexpected.String() + ", expected " + e.Expected
	case ErrorKindInvalidValue:
		return "invalid value: " + e.Unexpected.String() + ", expected " + e.Expected
	case ErrorKindInvalidLength:
		return "invalid length " + strconv.Itoa(e.Len) + ", expected " + e.Expected
	case ErrorKindUnknownVariant:
		if len(e.Names) == 0 {
			return "unknown variant `" + e.Name + "`, there are no variants"
		}
		return "unknown variant `" + e.Name + "`, expected " + OneOf(e.Names)
	case ErrorKindUnknownField:
		if len(e.Names) == 0 {
			return "unknown field `" + e.Name + "`, there are no fields"
		}
		return "unknown field `" + e.Name + "`, expected " + OneOf(e.Names)
	case ErrorKindMissingField:
		return "missing field `" + e.Name + "`"
	case ErrorKindDuplicateField:
		return "duplicate field `" + e.Name + "`"
	}
	return e.Msg
}

// String is the error's Display form.
func (e *Error) String() string { return e.Error() }

// AsError returns err as a serde error that no format has taken yet, and
// whether it is one. Formats use it where an error comes back from a visitor
// or a seed.
func AsError(err error) (*Error, bool) {
	e, ok := err.(*Error)
	return e, ok
}

// displayString is `msg.to_string()` for the argument of Error::custom, which
// Rust accepts as any `T: Display`.
func displayString(msg any) string {
	switch value := msg.(type) {
	case string:
		return value
	case error:
		return value.Error()
	case fmt.Stringer:
		return value.String()
	}
	return fmt.Sprint(msg)
}

// DeErrorCustom is `de::Error::custom(msg)`: an error with the message msg,
// which is a string, an error or anything with a String method (Rust takes
// any `T: Display`).
func DeErrorCustom(msg any) error {
	return &Error{Kind: ErrorKindCustom, Msg: displayString(msg)}
}

// SerErrorCustom is `ser::Error::custom(msg)`.
func SerErrorCustom(msg any) error {
	return &Error{Kind: ErrorKindCustom, Msg: displayString(msg)}
}

// DeErrorInvalidType is `de::Error::invalid_type(unexp, exp)`: the input
// holds a type the Deserialize was not expecting.
func DeErrorInvalidType(unexp DeUnexpected, exp DeExpected) error {
	return &Error{Kind: ErrorKindInvalidType, Unexpected: unexp, Expected: exp.Expecting()}
}

// DeErrorInvalidValue is `de::Error::invalid_value(unexp, exp)`: the input
// holds a value of the right type that is wrong for some other reason.
func DeErrorInvalidValue(unexp DeUnexpected, exp DeExpected) error {
	return &Error{Kind: ErrorKindInvalidValue, Unexpected: unexp, Expected: exp.Expecting()}
}

// DeErrorInvalidLength is `de::Error::invalid_length(len, exp)`: a sequence
// or map holds too many or too few elements.
func DeErrorInvalidLength(length int, exp DeExpected) error {
	return &Error{Kind: ErrorKindInvalidLength, Len: length, Expected: exp.Expecting()}
}

// DeErrorUnknownVariant is `de::Error::unknown_variant(variant, expected)`.
func DeErrorUnknownVariant(variant string, expected []string) error {
	return &Error{Kind: ErrorKindUnknownVariant, Name: variant, Names: expected}
}

// DeErrorUnknownField is `de::Error::unknown_field(field, expected)`.
func DeErrorUnknownField(field string, expected []string) error {
	return &Error{Kind: ErrorKindUnknownField, Name: field, Names: expected}
}

// DeErrorMissingField is `de::Error::missing_field(field)`.
func DeErrorMissingField(field string) error {
	return &Error{Kind: ErrorKindMissingField, Name: field}
}

// DeErrorDuplicateField is `de::Error::duplicate_field(field)`.
func DeErrorDuplicateField(field string) error {
	return &Error{Kind: ErrorKindDuplicateField, Name: field}
}

// DeExpected is `de::Expected`: what a visitor was expecting to receive, as a
// noun phrase that completes "This Visitor expects to receive ...". Every
// DeVisitor is one; DeExpectedStr makes one from a string.
//
// Rust's `Expected::fmt` and `Visitor::expecting` write to a formatter; every
// implementation writes one string, so the Go method returns it.
type DeExpected interface {
	Expecting() string
}

// DeExpectedStr is the `&"..."` argument of invalid_type, invalid_value and
// invalid_length: `impl Expected for &str`.
type DeExpectedStr string

// Expecting returns the string.
func (s DeExpectedStr) Expecting() string { return string(s) }

// DeUnexpected is `de::Unexpected`: the type or value the input held where
// something else was expected. A value is a pointer to one of the
// DeUnexpected_* structs.
type DeUnexpected interface {
	isDeUnexpected()
	// String is Unexpected's Display form, as serde's default error messages
	// print it.
	String() string
}

// DeUnexpectedBool is `Unexpected::Bool(bool)`.
type DeUnexpectedBool struct{ F0 bool }

// DeUnexpectedUnsigned is `Unexpected::Unsigned(u64)`.
type DeUnexpectedUnsigned struct{ F0 uint64 }

// DeUnexpectedSigned is `Unexpected::Signed(i64)`.
type DeUnexpectedSigned struct{ F0 int64 }

// DeUnexpectedFloat is `Unexpected::Float(f64)`.
type DeUnexpectedFloat struct{ F0 float64 }

// DeUnexpectedChar is `Unexpected::Char(char)`.
type DeUnexpectedChar struct{ F0 rune }

// DeUnexpectedStr is `Unexpected::Str(&str)`.
type DeUnexpectedStr struct{ F0 string }

// DeUnexpectedBytes is `Unexpected::Bytes(&[u8])`.
type DeUnexpectedBytes struct{ F0 []byte }

// DeUnexpectedUnit is `Unexpected::Unit`.
type DeUnexpectedUnit struct{}

// DeUnexpectedOption is `Unexpected::Option`.
type DeUnexpectedOption struct{}

// DeUnexpectedNewtypeStruct is `Unexpected::NewtypeStruct`.
type DeUnexpectedNewtypeStruct struct{}

// DeUnexpectedSeq is `Unexpected::Seq`.
type DeUnexpectedSeq struct{}

// DeUnexpectedMap is `Unexpected::Map`.
type DeUnexpectedMap struct{}

// DeUnexpectedEnum is `Unexpected::Enum`.
type DeUnexpectedEnum struct{}

// DeUnexpectedUnitVariant is `Unexpected::UnitVariant`.
type DeUnexpectedUnitVariant struct{}

// DeUnexpectedNewtypeVariant is `Unexpected::NewtypeVariant`.
type DeUnexpectedNewtypeVariant struct{}

// DeUnexpectedTupleVariant is `Unexpected::TupleVariant`.
type DeUnexpectedTupleVariant struct{}

// DeUnexpectedStructVariant is `Unexpected::StructVariant`.
type DeUnexpectedStructVariant struct{}

// DeUnexpectedOther is `Unexpected::Other(&str)`: a noun phrase for a type
// the other variants do not cover.
type DeUnexpectedOther struct{ F0 string }

func (*DeUnexpectedBool) isDeUnexpected()           {}
func (*DeUnexpectedUnsigned) isDeUnexpected()       {}
func (*DeUnexpectedSigned) isDeUnexpected()         {}
func (*DeUnexpectedFloat) isDeUnexpected()          {}
func (*DeUnexpectedChar) isDeUnexpected()           {}
func (*DeUnexpectedStr) isDeUnexpected()            {}
func (*DeUnexpectedBytes) isDeUnexpected()          {}
func (*DeUnexpectedUnit) isDeUnexpected()           {}
func (*DeUnexpectedOption) isDeUnexpected()         {}
func (*DeUnexpectedNewtypeStruct) isDeUnexpected()  {}
func (*DeUnexpectedSeq) isDeUnexpected()            {}
func (*DeUnexpectedMap) isDeUnexpected()            {}
func (*DeUnexpectedEnum) isDeUnexpected()           {}
func (*DeUnexpectedUnitVariant) isDeUnexpected()    {}
func (*DeUnexpectedNewtypeVariant) isDeUnexpected() {}
func (*DeUnexpectedTupleVariant) isDeUnexpected()   {}
func (*DeUnexpectedStructVariant) isDeUnexpected()  {}
func (*DeUnexpectedOther) isDeUnexpected()          {}

func (u *DeUnexpectedBool) String() string {
	return "boolean `" + strconv.FormatBool(u.F0) + "`"
}

func (u *DeUnexpectedUnsigned) String() string {
	return "integer `" + strconv.FormatUint(u.F0, 10) + "`"
}

func (u *DeUnexpectedSigned) String() string {
	return "integer `" + strconv.FormatInt(u.F0, 10) + "`"
}

func (u *DeUnexpectedFloat) String() string {
	return "floating point `" + WithDecimalPoint(u.F0) + "`"
}

func (u *DeUnexpectedChar) String() string {
	return "character `" + string(u.F0) + "`"
}

func (u *DeUnexpectedStr) String() string { return "string " + DebugStr(u.F0) }

func (*DeUnexpectedBytes) String() string          { return "byte array" }
func (*DeUnexpectedUnit) String() string           { return "unit value" }
func (*DeUnexpectedOption) String() string         { return "Option value" }
func (*DeUnexpectedNewtypeStruct) String() string  { return "newtype struct" }
func (*DeUnexpectedSeq) String() string            { return "sequence" }
func (*DeUnexpectedMap) String() string            { return "map" }
func (*DeUnexpectedEnum) String() string           { return "enum" }
func (*DeUnexpectedUnitVariant) String() string    { return "unit variant" }
func (*DeUnexpectedNewtypeVariant) String() string { return "newtype variant" }
func (*DeUnexpectedTupleVariant) String() string   { return "tuple variant" }
func (*DeUnexpectedStructVariant) String() string  { return "struct variant" }
func (u *DeUnexpectedOther) String() string        { return u.F0 }

// OneOf formats the expected names of unknown_variant and unknown_field as
// serde does: "`a`", "`a` or `b`", "one of `a`, `b`, `c`". names must not be
// empty.
func OneOf(names []string) string {
	switch len(names) {
	case 0:
		panic("serde: OneOf of no names") // special case elsewhere
	case 1:
		return "`" + names[0] + "`"
	case 2:
		return "`" + names[0] + "` or `" + names[1] + "`"
	}
	var out strings.Builder
	out.WriteString("one of ")
	for index, name := range names {
		if index > 0 {
			out.WriteString(", ")
		}
		out.WriteByte('`')
		out.WriteString(name)
		out.WriteByte('`')
	}
	return out.String()
}

// DisplayF64 is `format!("{}", value)` for an f64: the shortest decimal that
// reads back as value, never in exponent form.
func DisplayF64(value float64) string {
	switch {
	case math.IsNaN(value):
		return "NaN"
	case math.IsInf(value, 1):
		return "inf"
	case math.IsInf(value, -1):
		return "-inf"
	}
	return strconv.FormatFloat(value, 'f', -1, 64)
}

// WithDecimalPoint is how serde's `Unexpected::Float` prints its value: as
// `{}` prints it, with ".0" added when a finite value printed with no decimal
// point.
func WithDecimalPoint(value float64) string {
	text := DisplayF64(value)
	if math.IsNaN(value) || math.IsInf(value, 0) {
		return text
	}
	if !strings.Contains(text, ".") {
		text += ".0"
	}
	return text
}
