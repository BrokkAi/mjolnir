// Port of clap_builder 4.6.0 src/error/context.rs

package clap

import (
	"strconv"
	"strings"

	"github.com/BrokkAi/mjolnir/go/rs"
)

// ErrorContextKind is clap::error::ContextKind: semantics for a piece of
// error information.
type ErrorContextKind uint8

const (
	// ErrorContextKind_InvalidSubcommand is the cause of the error.
	ErrorContextKind_InvalidSubcommand ErrorContextKind = iota
	// ErrorContextKind_InvalidArg is the cause of the error.
	ErrorContextKind_InvalidArg
	// ErrorContextKind_PriorArg is existing arguments.
	ErrorContextKind_PriorArg
	// ErrorContextKind_ValidSubcommand is accepted subcommands.
	ErrorContextKind_ValidSubcommand
	// ErrorContextKind_ValidValue is accepted values.
	ErrorContextKind_ValidValue
	// ErrorContextKind_InvalidValue is rejected values.
	ErrorContextKind_InvalidValue
	// ErrorContextKind_ActualNumValues is the number of values present.
	ErrorContextKind_ActualNumValues
	// ErrorContextKind_ExpectedNumValues is the number of allowed values.
	ErrorContextKind_ExpectedNumValues
	// ErrorContextKind_MinValues is the minimum number of allowed values.
	ErrorContextKind_MinValues
	// ErrorContextKind_SuggestedCommand is a potential fix for the user.
	ErrorContextKind_SuggestedCommand
	// ErrorContextKind_SuggestedSubcommand is a potential fix for the user.
	ErrorContextKind_SuggestedSubcommand
	// ErrorContextKind_SuggestedArg is a potential fix for the user.
	ErrorContextKind_SuggestedArg
	// ErrorContextKind_SuggestedValue is a potential fix for the user.
	ErrorContextKind_SuggestedValue
	// ErrorContextKind_TrailingArg is the trailing argument.
	ErrorContextKind_TrailingArg
	// ErrorContextKind_Suggested is a general suggestion.
	ErrorContextKind_Suggested
	// ErrorContextKind_Usage is a usage string.
	ErrorContextKind_Usage
	// ErrorContextKind_Custom is an opaque message to the user.
	ErrorContextKind_Custom
)

var _ = rs.RegisterEnum[ErrorContextKind]("ContextKind",
	rs.Const(ErrorContextKind_InvalidSubcommand, "InvalidSubcommand"),
	rs.Const(ErrorContextKind_InvalidArg, "InvalidArg"),
	rs.Const(ErrorContextKind_PriorArg, "PriorArg"),
	rs.Const(ErrorContextKind_ValidSubcommand, "ValidSubcommand"),
	rs.Const(ErrorContextKind_ValidValue, "ValidValue"),
	rs.Const(ErrorContextKind_InvalidValue, "InvalidValue"),
	rs.Const(ErrorContextKind_ActualNumValues, "ActualNumValues"),
	rs.Const(ErrorContextKind_ExpectedNumValues, "ExpectedNumValues"),
	rs.Const(ErrorContextKind_MinValues, "MinValues"),
	rs.Const(ErrorContextKind_SuggestedCommand, "SuggestedCommand"),
	rs.Const(ErrorContextKind_SuggestedSubcommand, "SuggestedSubcommand"),
	rs.Const(ErrorContextKind_SuggestedArg, "SuggestedArg"),
	rs.Const(ErrorContextKind_SuggestedValue, "SuggestedValue"),
	rs.Const(ErrorContextKind_TrailingArg, "TrailingArg"),
	rs.Const(ErrorContextKind_Suggested, "Suggested"),
	rs.Const(ErrorContextKind_Usage, "Usage"),
	rs.Const(ErrorContextKind_Custom, "Custom"),
)

var errorContextKindNames = [...]string{
	"InvalidSubcommand", "InvalidArg", "PriorArg", "ValidSubcommand", "ValidValue",
	"InvalidValue", "ActualNumValues", "ExpectedNumValues", "MinValues", "SuggestedCommand",
	"SuggestedSubcommand", "SuggestedArg", "SuggestedValue", "TrailingArg", "Suggested",
	"Usage", "Custom",
}

// debugString is the derived Debug form: the variant's name.
func (k ErrorContextKind) debugString() string {
	if int(k) < len(errorContextKindNames) {
		return errorContextKindNames[k]
	}
	panic(rs.Unreachable())
}

// FmtDebug is the derived `impl Debug for ContextKind`.
func (k ErrorContextKind) FmtDebug(f *rs.Formatter) error { return f.WriteStr(k.debugString()) }

// AsStr is ContextKind::as_str: end-user description of the error case,
// where relevant.
func (k ErrorContextKind) AsStr() rs.Option[string] {
	switch k {
	case ErrorContextKind_InvalidSubcommand:
		return rs.Some("Invalid Subcommand")
	case ErrorContextKind_InvalidArg:
		return rs.Some("Invalid Argument")
	case ErrorContextKind_PriorArg:
		return rs.Some("Prior Argument")
	case ErrorContextKind_ValidSubcommand:
		return rs.Some("Valid Subcommand")
	case ErrorContextKind_ValidValue:
		return rs.Some("Valid Value")
	case ErrorContextKind_InvalidValue:
		return rs.Some("Invalid Value")
	case ErrorContextKind_ActualNumValues:
		return rs.Some("Actual Number of Values")
	case ErrorContextKind_ExpectedNumValues:
		return rs.Some("Expected Number of Values")
	case ErrorContextKind_MinValues:
		return rs.Some("Minimum Number of Values")
	case ErrorContextKind_SuggestedCommand:
		return rs.Some("Suggested Command")
	case ErrorContextKind_SuggestedSubcommand:
		return rs.Some("Suggested Subcommand")
	case ErrorContextKind_SuggestedArg:
		return rs.Some("Suggested Argument")
	case ErrorContextKind_SuggestedValue:
		return rs.Some("Suggested Value")
	case ErrorContextKind_TrailingArg:
		return rs.Some("Trailing Argument")
	case ErrorContextKind_Suggested:
		return rs.Some("Suggested")
	case ErrorContextKind_Usage:
		return rs.None[string]()
	case ErrorContextKind_Custom:
		return rs.None[string]()
	default:
		panic(rs.Unreachable())
	}
}

// String is `impl Display for ContextKind`.
func (k ErrorContextKind) String() string {
	return k.AsStr().UnwrapOrDefault()
}

// FmtDisplay is `impl Display for ContextKind`.
func (k ErrorContextKind) FmtDisplay(f *rs.Formatter) error { return f.Pad(k.String()) }

// ErrorContextValue is clap::error::ContextValue: a piece of error
// information.
type ErrorContextValue interface {
	isErrorContextValue()
	// String is `impl Display for ContextValue`.
	String() string
}

// ErrorContextValue_None is ContextValue::None: [`ContextKind`] is
// self-sufficient.
type ErrorContextValue_None struct{}

// ErrorContextValue_Bool is ContextValue::Bool: a single value.
type ErrorContextValue_Bool struct{ F0 bool }

// ErrorContextValue_String is ContextValue::String: a single value.
type ErrorContextValue_String struct{ F0 string }

// ErrorContextValue_Strings is ContextValue::Strings: many values.
type ErrorContextValue_Strings struct{ F0 []string }

// ErrorContextValue_StyledStr is ContextValue::StyledStr: a single value.
type ErrorContextValue_StyledStr struct{ F0 BuilderStyledStr }

// ErrorContextValue_StyledStrs is ContextValue::StyledStrs: many values.
type ErrorContextValue_StyledStrs struct{ F0 []BuilderStyledStr }

// ErrorContextValue_Number is ContextValue::Number: a single value.
type ErrorContextValue_Number struct{ F0 int }

func (*ErrorContextValue_None) isErrorContextValue()       {}
func (*ErrorContextValue_Bool) isErrorContextValue()       {}
func (*ErrorContextValue_String) isErrorContextValue()     {}
func (*ErrorContextValue_Strings) isErrorContextValue()    {}
func (*ErrorContextValue_StyledStr) isErrorContextValue()  {}
func (*ErrorContextValue_StyledStrs) isErrorContextValue() {}
func (*ErrorContextValue_Number) isErrorContextValue()     {}

var _ = rs.RegisterEnum[ErrorContextValue]("ContextValue",
	rs.Variant[ErrorContextValue_None]("None", rs.ShapeUnit),
	rs.Variant[ErrorContextValue_Bool]("Bool", rs.ShapeTuple),
	rs.Variant[ErrorContextValue_String]("String", rs.ShapeTuple),
	rs.Variant[ErrorContextValue_Strings]("Strings", rs.ShapeTuple),
	rs.Variant[ErrorContextValue_StyledStr]("StyledStr", rs.ShapeTuple),
	rs.Variant[ErrorContextValue_StyledStrs]("StyledStrs", rs.ShapeTuple),
	rs.Variant[ErrorContextValue_Number]("Number", rs.ShapeTuple),
)

// String is `impl Display for ContextValue`.
func (*ErrorContextValue_None) String() string { return "" }

// String is `impl Display for ContextValue`.
func (v *ErrorContextValue_Bool) String() string { return strconv.FormatBool(v.F0) }

// String is `impl Display for ContextValue`.
func (v *ErrorContextValue_String) String() string { return v.F0 }

// String is `impl Display for ContextValue`.
func (v *ErrorContextValue_Strings) String() string { return strings.Join(v.F0, ", ") }

// String is `impl Display for ContextValue`.
func (v *ErrorContextValue_StyledStr) String() string { return v.F0.String() }

// String is `impl Display for ContextValue`.
func (v *ErrorContextValue_StyledStrs) String() string {
	var out strings.Builder
	for i, v := range v.F0 {
		if i != 0 {
			out.WriteString(", ")
		}
		out.WriteString(v.String())
	}
	return out.String()
}

// String is `impl Display for ContextValue`.
func (v *ErrorContextValue_Number) String() string { return strconv.Itoa(v.F0) }

// FmtDisplay is `impl Display for ContextValue`: each part is padded as
// Rust pads it.
func errorContextValueFmtDisplay(v ErrorContextValue, f *rs.Formatter) error {
	switch v := v.(type) {
	case *ErrorContextValue_StyledStrs:
		for i, v := range v.F0 {
			if i != 0 {
				if err := f.Pad(", "); err != nil {
					return err
				}
			}
			if err := v.FmtDisplay(f); err != nil {
				return err
			}
		}
		return nil
	case *ErrorContextValue_StyledStr:
		return v.F0.FmtDisplay(f)
	default:
		return f.Pad(v.String())
	}
}

// FmtDisplay is `impl Display for ContextValue`.
func (v *ErrorContextValue_None) FmtDisplay(f *rs.Formatter) error {
	return errorContextValueFmtDisplay(v, f)
}

// FmtDisplay is `impl Display for ContextValue`.
func (v *ErrorContextValue_Bool) FmtDisplay(f *rs.Formatter) error {
	return errorContextValueFmtDisplay(v, f)
}

// FmtDisplay is `impl Display for ContextValue`.
func (v *ErrorContextValue_String) FmtDisplay(f *rs.Formatter) error {
	return errorContextValueFmtDisplay(v, f)
}

// FmtDisplay is `impl Display for ContextValue`.
func (v *ErrorContextValue_Strings) FmtDisplay(f *rs.Formatter) error {
	return errorContextValueFmtDisplay(v, f)
}

// FmtDisplay is `impl Display for ContextValue`.
func (v *ErrorContextValue_StyledStr) FmtDisplay(f *rs.Formatter) error {
	return errorContextValueFmtDisplay(v, f)
}

// FmtDisplay is `impl Display for ContextValue`.
func (v *ErrorContextValue_StyledStrs) FmtDisplay(f *rs.Formatter) error {
	return errorContextValueFmtDisplay(v, f)
}

// FmtDisplay is `impl Display for ContextValue`.
func (v *ErrorContextValue_Number) FmtDisplay(f *rs.Formatter) error {
	return errorContextValueFmtDisplay(v, f)
}

// errorContextValueEq is the derived `PartialEq for ContextValue`.
func errorContextValueEq(a, b ErrorContextValue) bool {
	switch a := a.(type) {
	case *ErrorContextValue_None:
		_, ok := b.(*ErrorContextValue_None)
		return ok
	case *ErrorContextValue_Bool:
		b, ok := b.(*ErrorContextValue_Bool)
		return ok && a.F0 == b.F0
	case *ErrorContextValue_String:
		b, ok := b.(*ErrorContextValue_String)
		return ok && a.F0 == b.F0
	case *ErrorContextValue_Strings:
		b, ok := b.(*ErrorContextValue_Strings)
		if !ok || len(a.F0) != len(b.F0) {
			return false
		}
		for i := range a.F0 {
			if a.F0[i] != b.F0[i] {
				return false
			}
		}
		return true
	case *ErrorContextValue_StyledStr:
		b, ok := b.(*ErrorContextValue_StyledStr)
		return ok && a.F0 == b.F0
	case *ErrorContextValue_StyledStrs:
		b, ok := b.(*ErrorContextValue_StyledStrs)
		if !ok || len(a.F0) != len(b.F0) {
			return false
		}
		for i := range a.F0 {
			if a.F0[i] != b.F0[i] {
				return false
			}
		}
		return true
	case *ErrorContextValue_Number:
		b, ok := b.(*ErrorContextValue_Number)
		return ok && a.F0 == b.F0
	default:
		panic(rs.Unreachable(a))
	}
}
