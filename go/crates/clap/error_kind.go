// Port of clap_builder 4.6.0 src/error/kind.rs

package clap

import "github.com/BrokkAi/mjolnir/go/rs"

// ErrorErrorKind is clap::error::ErrorKind: command line argument parser
// kind of error.
type ErrorErrorKind uint8

const (
	// ErrorErrorKind_InvalidValue occurs when an Arg has a set of possible
	// values, and the user provides a value which isn't in that set.
	ErrorErrorKind_InvalidValue ErrorErrorKind = iota

	// ErrorErrorKind_UnknownArgument occurs when a user provides a flag,
	// option, argument or subcommand which isn't defined.
	ErrorErrorKind_UnknownArgument

	// ErrorErrorKind_InvalidSubcommand occurs when the user provides an
	// unrecognized Subcommand which meets the threshold for being similar
	// enough to an existing subcommand. If it doesn't meet the threshold, or
	// the 'suggestions' feature is disabled, the more general
	// UnknownArgument error is returned.
	ErrorErrorKind_InvalidSubcommand

	// ErrorErrorKind_NoEquals occurs when the user doesn't use equals for an
	// option that requires equal sign to provide values.
	ErrorErrorKind_NoEquals

	// ErrorErrorKind_ValueValidation occurs when the user provides a value
	// for an argument with a custom validation and the value fails that
	// validation.
	ErrorErrorKind_ValueValidation

	// ErrorErrorKind_TooManyValues occurs when a user provides more values
	// for an argument than were defined by setting Arg::num_args.
	ErrorErrorKind_TooManyValues

	// ErrorErrorKind_TooFewValues occurs when the user provides fewer values
	// for an argument than were defined by setting Arg::num_args.
	ErrorErrorKind_TooFewValues

	// ErrorErrorKind_WrongNumberOfValues occurs when the user provides a
	// different number of values for an argument than what's been defined
	// by setting Arg::num_args or than was implicitly set by
	// Arg::value_names.
	ErrorErrorKind_WrongNumberOfValues

	// ErrorErrorKind_ArgumentConflict occurs when the user provides two
	// values which conflict with each other and can't be used together.
	ErrorErrorKind_ArgumentConflict

	// ErrorErrorKind_MissingRequiredArgument occurs when the user does not
	// provide one or more required arguments.
	ErrorErrorKind_MissingRequiredArgument

	// ErrorErrorKind_MissingSubcommand occurs when a subcommand is required
	// (as defined by Command::subcommand_required), but the user does not
	// provide one.
	ErrorErrorKind_MissingSubcommand

	// ErrorErrorKind_InvalidUtf8 occurs when the user has provided a value
	// containing invalid UTF-8.
	ErrorErrorKind_InvalidUtf8

	// ErrorErrorKind_DisplayHelp is not a true "error" as it means `--help`
	// or similar was used. The help message will be sent to `stdout`.
	//
	// Note: If the help is displayed due to an error (such as missing
	// subcommands) it will be sent to `stderr` instead of `stdout`.
	ErrorErrorKind_DisplayHelp

	// ErrorErrorKind_DisplayHelpOnMissingArgumentOrSubcommand occurs when
	// either an argument or a Subcommand is required, as defined by
	// Command::arg_required_else_help and Command::subcommand_required, but
	// the user did not provide one.
	ErrorErrorKind_DisplayHelpOnMissingArgumentOrSubcommand

	// ErrorErrorKind_DisplayVersion is not a true "error" as it means
	// `--version` or similar was used. The message will be sent to
	// `stdout`.
	ErrorErrorKind_DisplayVersion

	// ErrorErrorKind_Io represents an I/O error. Can occur when writing to
	// `stderr` or `stdout` or reading a configuration file.
	ErrorErrorKind_Io

	// ErrorErrorKind_Format represents a Format error (which is a part of
	// Display). Typically caused by writing to `stderr` or `stdout`.
	ErrorErrorKind_Format
)

var _ = rs.RegisterEnum[ErrorErrorKind]("ErrorKind",
	rs.Const(ErrorErrorKind_InvalidValue, "InvalidValue"),
	rs.Const(ErrorErrorKind_UnknownArgument, "UnknownArgument"),
	rs.Const(ErrorErrorKind_InvalidSubcommand, "InvalidSubcommand"),
	rs.Const(ErrorErrorKind_NoEquals, "NoEquals"),
	rs.Const(ErrorErrorKind_ValueValidation, "ValueValidation"),
	rs.Const(ErrorErrorKind_TooManyValues, "TooManyValues"),
	rs.Const(ErrorErrorKind_TooFewValues, "TooFewValues"),
	rs.Const(ErrorErrorKind_WrongNumberOfValues, "WrongNumberOfValues"),
	rs.Const(ErrorErrorKind_ArgumentConflict, "ArgumentConflict"),
	rs.Const(ErrorErrorKind_MissingRequiredArgument, "MissingRequiredArgument"),
	rs.Const(ErrorErrorKind_MissingSubcommand, "MissingSubcommand"),
	rs.Const(ErrorErrorKind_InvalidUtf8, "InvalidUtf8"),
	rs.Const(ErrorErrorKind_DisplayHelp, "DisplayHelp"),
	rs.Const(ErrorErrorKind_DisplayHelpOnMissingArgumentOrSubcommand, "DisplayHelpOnMissingArgumentOrSubcommand"),
	rs.Const(ErrorErrorKind_DisplayVersion, "DisplayVersion"),
	rs.Const(ErrorErrorKind_Io, "Io"),
	rs.Const(ErrorErrorKind_Format, "Format"),
)

var errorKindNames = [...]string{
	"InvalidValue", "UnknownArgument", "InvalidSubcommand", "NoEquals", "ValueValidation",
	"TooManyValues", "TooFewValues", "WrongNumberOfValues", "ArgumentConflict",
	"MissingRequiredArgument", "MissingSubcommand", "InvalidUtf8", "DisplayHelp",
	"DisplayHelpOnMissingArgumentOrSubcommand", "DisplayVersion", "Io", "Format",
}

// debugString is the derived Debug form: the variant's name.
func (k ErrorErrorKind) debugString() string {
	if int(k) < len(errorKindNames) {
		return errorKindNames[k]
	}
	panic(rs.Unreachable())
}

// FmtDebug is the derived `impl Debug for ErrorKind`.
func (k ErrorErrorKind) FmtDebug(f *rs.Formatter) error { return f.WriteStr(k.debugString()) }

// AsStr is ErrorKind::as_str: end-user description of the error case,
// where relevant.
func (k ErrorErrorKind) AsStr() rs.Option[string] {
	switch k {
	case ErrorErrorKind_InvalidValue:
		return rs.Some("one of the values isn't valid for an argument")
	case ErrorErrorKind_UnknownArgument:
		return rs.Some("unexpected argument found")
	case ErrorErrorKind_InvalidSubcommand:
		return rs.Some("unrecognized subcommand")
	case ErrorErrorKind_NoEquals:
		return rs.Some("equal is needed when assigning values to one of the arguments")
	case ErrorErrorKind_ValueValidation:
		return rs.Some("invalid value for one of the arguments")
	case ErrorErrorKind_TooManyValues:
		return rs.Some("unexpected value for an argument found")
	case ErrorErrorKind_TooFewValues:
		return rs.Some("more values required for an argument")
	case ErrorErrorKind_WrongNumberOfValues:
		return rs.Some("too many or too few values for an argument")
	case ErrorErrorKind_ArgumentConflict:
		return rs.Some("an argument cannot be used with one or more of the other specified arguments")
	case ErrorErrorKind_MissingRequiredArgument:
		return rs.Some("one or more required arguments were not provided")
	case ErrorErrorKind_MissingSubcommand:
		return rs.Some("a subcommand is required but one was not provided")
	case ErrorErrorKind_InvalidUtf8:
		return rs.Some("invalid UTF-8 was detected in one or more arguments")
	case ErrorErrorKind_DisplayHelp:
		return rs.None[string]()
	case ErrorErrorKind_DisplayHelpOnMissingArgumentOrSubcommand:
		return rs.None[string]()
	case ErrorErrorKind_DisplayVersion:
		return rs.None[string]()
	case ErrorErrorKind_Io:
		return rs.None[string]()
	case ErrorErrorKind_Format:
		return rs.None[string]()
	default:
		panic(rs.Unreachable())
	}
}

// String is `impl Display for ErrorKind`.
func (k ErrorErrorKind) String() string {
	return k.AsStr().UnwrapOrDefault()
}

// FmtDisplay is `impl Display for ErrorKind`.
func (k ErrorErrorKind) FmtDisplay(f *rs.Formatter) error { return f.Pad(k.String()) }
