// Port of clap_builder 4.6.0 src/parser/error.rs

package clap

import "github.com/BrokkAi/mjolnir/go/rs"

// ParserMatchesError is clap::parser::MatchesError: violation of
// ArgMatches assumptions.
type ParserMatchesError interface {
	error
	isParserMatchesError()
}

// ParserMatchesError_Downcast is MatchesError::Downcast: failed to downcast
// `AnyValue` to the specified type.
type ParserMatchesError_Downcast struct {
	Actual   utilAnyValueId
	Expected utilAnyValueId
}

// ParserMatchesError_UnknownArgument is MatchesError::UnknownArgument:
// argument not defined in Command.
type ParserMatchesError_UnknownArgument struct {
	// Missing `id` but blocked on a public id type which will hopefully come with `unstable-v4`
}

func (*ParserMatchesError_Downcast) isParserMatchesError()        {}
func (*ParserMatchesError_UnknownArgument) isParserMatchesError() {}

var _ = rs.RegisterEnum[ParserMatchesError]("MatchesError",
	rs.Variant[ParserMatchesError_Downcast]("Downcast", rs.ShapeStruct),
	rs.Variant[ParserMatchesError_UnknownArgument]("UnknownArgument", rs.ShapeStruct),
)

// Error is `impl Display for MatchesError`.
//
// The two types print by their Go names where Rust prints
// `std::any::type_name`.
func (e *ParserMatchesError_Downcast) Error() string {
	return "Could not downcast to " + e.Expected.debugString() + ", need to downcast to " + e.Actual.debugString() + "\n"
}

// Error is `impl Display for MatchesError`.
func (e *ParserMatchesError_UnknownArgument) Error() string {
	return "Unknown argument or group id.  Make sure you are using the argument id and not the short or long flags\n"
}

// FmtDisplay is `impl Display for MatchesError`.
func (e *ParserMatchesError_Downcast) FmtDisplay(f *rs.Formatter) error {
	return f.WriteStr(e.Error())
}

// FmtDisplay is `impl Display for MatchesError`.
func (e *ParserMatchesError_UnknownArgument) FmtDisplay(f *rs.Formatter) error {
	return f.WriteStr(e.Error())
}

// parserMatchesErrorUnwrap is MatchesError::unwrap: the value, or a panic
// that names the id.
func parserMatchesErrorUnwrap[T any](id string, value T, err ParserMatchesError) T {
	if err == nil {
		return value
	}
	rs.PanicAt(2, "Mismatch between definition and access of `"+id+"`. "+err.Error())
	panic(rs.Unreachable())
}
