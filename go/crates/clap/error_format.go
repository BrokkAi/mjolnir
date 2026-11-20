// Port of clap_builder 4.6.0 src/error/format.rs

package clap

import (
	"strconv"

	"github.com/BrokkAi/mjolnir/go/rs"
)

// ErrorErrorFormatter is clap::error::ErrorFormatter: defines how to format
// an error for displaying to the user.
//
// Mjolnir uses only the default, RichFormatter, so Error is always
// formatted with it; KindFormatter is here for the API.
type ErrorErrorFormatter interface {
	// FormatError is ErrorFormatter::format_error: stylize the error for
	// the terminal.
	FormatError(err *Error) BuilderStyledStr
}

// ErrorKindFormatter is clap::error::KindFormatter: report ErrorKind.
//
// No context is included.
//
// NOTE: Consider removing the `error-context` default feature if using
// this to remove all overhead for RichFormatter.
type ErrorKindFormatter struct{}

// FormatError is `impl ErrorFormatter for KindFormatter`.
func (ErrorKindFormatter) FormatError(err *Error) BuilderStyledStr {
	styles := &err.inner.styles

	styled := BuilderStyledStrNew()
	startError(&styled, styles)
	if msg, ok := err.Kind().AsStr().Get(); ok {
		styled.PushStr(msg)
	} else if source, ok := err.inner.source.Get(); ok {
		styled.PushStr(source.Error())
	} else {
		styled.PushStr("unknown cause")
	}
	styled.PushStr("\n")
	return styled
}

// ErrorRichFormatter is clap::error::RichFormatter: richly formatted error
// context.
//
// This follows the rustc diagnostic style guide.
type ErrorRichFormatter struct{}

// FormatError is `impl ErrorFormatter for RichFormatter`.
func (ErrorRichFormatter) FormatError(err *Error) BuilderStyledStr {
	styles := &err.inner.styles
	valid := styles.GetValid()

	styled := BuilderStyledStrNew()
	startError(&styled, styles)

	if !writeDynamicContext(err, &styled, styles) {
		if msg, ok := err.Kind().AsStr().Get(); ok {
			styled.PushStr(msg)
		} else if source, ok := err.inner.source.Get(); ok {
			styled.PushStr(source.Error())
		} else {
			styled.PushStr("unknown cause")
		}
	}

	suggested := false
	if valid, ok := err.Get(ErrorContextKind_SuggestedSubcommand).Get(); ok {
		styled.PushStr("\n")
		if !suggested {
			styled.PushStr("\n")
			suggested = true
		}
		didYouMean(&styled, styles, "subcommand", valid)
	}
	if valid, ok := err.Get(ErrorContextKind_SuggestedArg).Get(); ok {
		styled.PushStr("\n")
		if !suggested {
			styled.PushStr("\n")
			suggested = true
		}
		didYouMean(&styled, styles, "argument", valid)
	}
	if valid, ok := err.Get(ErrorContextKind_SuggestedValue).Get(); ok {
		styled.PushStr("\n")
		if !suggested {
			styled.PushStr("\n")
			suggested = true
		}
		didYouMean(&styled, styles, "value", valid)
	}
	suggestions := err.Get(ErrorContextKind_Suggested)
	if value, ok := suggestions.Get(); ok {
		if suggestions, ok := value.(*ErrorContextValue_StyledStrs); ok {
			if !suggested {
				styled.PushStr("\n")
			}
			for _, suggestion := range suggestions.F0 {
				styled.PushStr("\n" + outputTAB + valid.Render() + "tip:" + valid.RenderReset() + " ")
				styled.pushStyled(suggestion)
			}
		}
	}

	usage := err.Get(ErrorContextKind_Usage)
	if value, ok := usage.Get(); ok {
		if usage, ok := value.(*ErrorContextValue_StyledStr); ok {
			putUsage(&styled, usage.F0)
		}
	}

	tryHelp(&styled, styles, err.inner.helpFlag)

	return styled
}

func startError(styled *BuilderStyledStr, styles *BuilderStyles) {
	error_ := styles.GetError()
	styled.PushStr(error_.Render() + "error:" + error_.RenderReset() + " ")
}

// contextString is `if let Some(ContextValue::String(v)) = context`.
func contextString(value rs.Option[ErrorContextValue]) (string, bool) {
	if v, ok := value.Get(); ok {
		if s, ok := v.(*ErrorContextValue_String); ok {
			return s.F0, true
		}
	}
	return "", false
}

// contextStrings is `if let Some(ContextValue::Strings(v)) = context`.
func contextStrings(value rs.Option[ErrorContextValue]) ([]string, bool) {
	if v, ok := value.Get(); ok {
		if s, ok := v.(*ErrorContextValue_Strings); ok {
			return s.F0, true
		}
	}
	return nil, false
}

// contextNumber is `if let Some(ContextValue::Number(v)) = context`.
func contextNumber(value rs.Option[ErrorContextValue]) (int, bool) {
	if v, ok := value.Get(); ok {
		if n, ok := v.(*ErrorContextValue_Number); ok {
			return n.F0, true
		}
	}
	return 0, false
}

func writeDynamicContext(err *Error, styled *BuilderStyledStr, styles *BuilderStyles) bool {
	valid := styles.GetValid()
	invalid := styles.GetInvalid()
	literal := styles.GetLiteral()

	switch err.Kind() {
	case ErrorErrorKind_ArgumentConflict:
		priorArg := err.Get(ErrorContextKind_PriorArg)
		if invalidArg, ok := contextString(err.Get(ErrorContextKind_InvalidArg)); ok {
			if prior, ok := priorArg.Get(); ok && errorContextValueEq(prior, &ErrorContextValue_String{F0: invalidArg}) {
				priorArg = rs.None[ErrorContextValue]()
				styled.PushStr("the argument '" + invalid.Render() + invalidArg + invalid.RenderReset() + "' cannot be used multiple times")
			} else {
				styled.PushStr("the argument '" + invalid.Render() + invalidArg + invalid.RenderReset() + "' cannot be used with")
			}
		} else if invalidArg, ok := contextString(err.Get(ErrorContextKind_InvalidSubcommand)); ok {
			styled.PushStr("the subcommand '" + invalid.Render() + invalidArg + invalid.RenderReset() + "' cannot be used with")
		} else {
			styled.PushStr(err.Kind().AsStr().Unwrap())
		}

		if priorArg, ok := priorArg.Get(); ok {
			switch priorArg := priorArg.(type) {
			case *ErrorContextValue_Strings:
				styled.PushStr(":")
				for _, v := range priorArg.F0 {
					styled.PushStr("\n" + outputTAB + invalid.Render() + v + invalid.RenderReset())
				}
			case *ErrorContextValue_String:
				styled.PushStr(" '" + invalid.Render() + priorArg.F0 + invalid.RenderReset() + "'")
			default:
				styled.PushStr(" one or more of the other specified arguments")
			}
		}
		return true
	case ErrorErrorKind_NoEquals:
		if invalidArg, ok := contextString(err.Get(ErrorContextKind_InvalidArg)); ok {
			styled.PushStr("equal sign is needed when assigning values to '" + invalid.Render() + invalidArg + invalid.RenderReset() + "'")
			return true
		}
		return false
	case ErrorErrorKind_InvalidValue:
		invalidArg, okArg := contextString(err.Get(ErrorContextKind_InvalidArg))
		invalidValue, okValue := contextString(err.Get(ErrorContextKind_InvalidValue))
		if okArg && okValue {
			if invalidValue == "" {
				styled.PushStr("a value is required for '" + invalid.Render() + invalidArg + invalid.RenderReset() + "' but none was supplied")
			} else {
				styled.PushStr("invalid value '" + invalid.Render() + invalidValue + invalid.RenderReset() + "' for '" + literal.Render() + invalidArg + literal.RenderReset() + "'")
			}

			values := err.Get(ErrorContextKind_ValidValue)
			writeValuesList("possible values", styled, valid, values)

			return true
		}
		return false
	case ErrorErrorKind_InvalidSubcommand:
		if invalidSub, ok := contextString(err.Get(ErrorContextKind_InvalidSubcommand)); ok {
			styled.PushStr("unrecognized subcommand '" + invalid.Render() + invalidSub + invalid.RenderReset() + "'")
			return true
		}
		return false
	case ErrorErrorKind_MissingRequiredArgument:
		if invalidArg, ok := contextStrings(err.Get(ErrorContextKind_InvalidArg)); ok {
			styled.PushStr("the following required arguments were not provided:")
			for _, v := range invalidArg {
				styled.PushStr("\n" + outputTAB + valid.Render() + v + valid.RenderReset())
			}
			return true
		}
		return false
	case ErrorErrorKind_MissingSubcommand:
		if invalidSub, ok := contextString(err.Get(ErrorContextKind_InvalidSubcommand)); ok {
			styled.PushStr("'" + invalid.Render() + invalidSub + invalid.RenderReset() + "' requires a subcommand but one was not provided")
			values := err.Get(ErrorContextKind_ValidSubcommand)
			writeValuesList("subcommands", styled, valid, values)

			return true
		}
		return false
	case ErrorErrorKind_InvalidUtf8:
		return false
	case ErrorErrorKind_TooManyValues:
		invalidArg, okArg := contextString(err.Get(ErrorContextKind_InvalidArg))
		invalidValue, okValue := contextString(err.Get(ErrorContextKind_InvalidValue))
		if okArg && okValue {
			styled.PushStr("unexpected value '" + invalid.Render() + invalidValue + invalid.RenderReset() + "' for '" + literal.Render() + invalidArg + literal.RenderReset() + "' found; no more were expected")
			return true
		}
		return false
	case ErrorErrorKind_TooFewValues:
		invalidArg, okArg := contextString(err.Get(ErrorContextKind_InvalidArg))
		actualNumValues, okActual := contextNumber(err.Get(ErrorContextKind_ActualNumValues))
		minValues, okMin := contextNumber(err.Get(ErrorContextKind_MinValues))
		if okArg && okActual && okMin {
			wereProvided := singularOrPlural(actualNumValues)
			styled.PushStr(valid.Render() + strconv.Itoa(minValues) + valid.RenderReset() + " values required by '" + literal.Render() + invalidArg + literal.RenderReset() + "'; only " + invalid.Render() + strconv.Itoa(actualNumValues) + invalid.RenderReset() + wereProvided)
			return true
		}
		return false
	case ErrorErrorKind_ValueValidation:
		invalidArg, okArg := contextString(err.Get(ErrorContextKind_InvalidArg))
		invalidValue, okValue := contextString(err.Get(ErrorContextKind_InvalidValue))
		if okArg && okValue {
			styled.PushStr("invalid value '" + invalid.Render() + invalidValue + invalid.RenderReset() + "' for '" + literal.Render() + invalidArg + literal.RenderReset() + "'")
			if source, ok := err.inner.source.Get(); ok {
				styled.PushStr(": " + source.Error())
			}
			return true
		}
		return false
	case ErrorErrorKind_WrongNumberOfValues:
		invalidArg, okArg := contextString(err.Get(ErrorContextKind_InvalidArg))
		actualNumValues, okActual := contextNumber(err.Get(ErrorContextKind_ActualNumValues))
		numValues, okNum := contextNumber(err.Get(ErrorContextKind_ExpectedNumValues))
		if okArg && okActual && okNum {
			wereProvided := singularOrPlural(actualNumValues)
			styled.PushStr(valid.Render() + strconv.Itoa(numValues) + valid.RenderReset() + " values required for '" + literal.Render() + invalidArg + literal.RenderReset() + "' but " + invalid.Render() + strconv.Itoa(actualNumValues) + invalid.RenderReset() + wereProvided)
			return true
		}
		return false
	case ErrorErrorKind_UnknownArgument:
		if invalidArg, ok := contextString(err.Get(ErrorContextKind_InvalidArg)); ok {
			styled.PushStr("unexpected argument '" + invalid.Render() + invalidArg + invalid.RenderReset() + "' found")
			return true
		}
		return false
	case ErrorErrorKind_DisplayHelp,
		ErrorErrorKind_DisplayHelpOnMissingArgumentOrSubcommand,
		ErrorErrorKind_DisplayVersion,
		ErrorErrorKind_Io,
		ErrorErrorKind_Format:
		return false
	default:
		panic(rs.Unreachable(err.Kind()))
	}
}

func writeValuesList(listName string, styled *BuilderStyledStr, valid BuilderStylingStyle, possibleValues rs.Option[ErrorContextValue]) {
	if possibleValues, ok := contextStrings(possibleValues); ok {
		if len(possibleValues) != 0 {
			styled.PushStr("\n" + outputTAB + "[" + listName + ": ")

			for idx, val := range possibleValues {
				if idx > 0 {
					styled.PushStr(", ")
				}
				styled.PushStr(valid.Render() + utilEscape(val) + valid.RenderReset())
			}

			styled.PushStr("]")
		}
	}
}

func formatErrorMessage(message string, styles *BuilderStyles, cmd *Command, usage rs.Option[BuilderStyledStr]) BuilderStyledStr {
	styled := BuilderStyledStrNew()
	startError(&styled, styles)
	styled.PushStr(message)
	if usage, ok := usage.Get(); ok {
		putUsage(&styled, usage)
	}
	if cmd != nil {
		tryHelp(&styled, styles, getHelpFlag(cmd))
	}
	return styled
}

// singularOrPlural returns the singular or plural form on the verb to be
// based on the argument's value.
func singularOrPlural(n int) string {
	if n > 1 {
		return " were provided"
	}
	return " was provided"
}

func putUsage(styled *BuilderStyledStr, usage BuilderStyledStr) {
	styled.PushStr("\n\n")
	styled.pushStyled(usage)
}

func getHelpFlag(cmd *Command) rs.Option[string] {
	if !cmd.IsDisableHelpFlagSet() {
		return rs.Some("--help")
	} else if flag, ok := getUserHelpFlag(cmd).Get(); ok {
		return rs.Some(flag)
	} else if cmd.HasSubcommands() && !cmd.IsDisableHelpSubcommandSet() {
		return rs.Some("help")
	}
	return rs.None[string]()
}

func getUserHelpFlag(cmd *Command) rs.Option[string] {
	var found *Arg
	for _, arg := range cmd.GetArguments() {
		switch arg.GetAction() {
		case ArgAction_Help, ArgAction_HelpShort, ArgAction_HelpLong:
			found = arg
		case ArgAction_Append,
			ArgAction_Count,
			ArgAction_SetTrue,
			ArgAction_SetFalse,
			ArgAction_Set,
			ArgAction_Version:
		default:
			panic(rs.Unreachable(arg.GetAction()))
		}
		if found != nil {
			break
		}
	}
	if found == nil {
		return rs.None[string]()
	}

	if long, ok := found.GetLong().Get(); ok {
		return rs.Some("--" + long)
	}
	if short, ok := found.GetShort().Get(); ok {
		return rs.Some("-" + string(short))
	}
	return rs.None[string]()
}

func tryHelp(styled *BuilderStyledStr, styles *BuilderStyles, help rs.Option[string]) {
	if help, ok := help.Get(); ok {
		literal := styles.GetLiteral()
		styled.PushStr("\n\nFor more information, try '" + literal.Render() + help + literal.RenderReset() + "'.\n")
	} else {
		styled.PushStr("\n")
	}
}

func didYouMean(styled *BuilderStyledStr, styles *BuilderStyles, context string, possibles ErrorContextValue) {
	valid := styles.GetValid()
	styled.PushStr(outputTAB + valid.Render() + "tip:" + valid.RenderReset())
	if possible, ok := possibles.(*ErrorContextValue_String); ok {
		styled.PushStr(" a similar " + context + " exists: '" + valid.Render() + possible.F0 + valid.RenderReset() + "'")
	} else if possibles, ok := possibles.(*ErrorContextValue_Strings); ok {
		if len(possibles.F0) == 1 {
			styled.PushStr(" a similar " + context + " exists: ")
		} else {
			styled.PushStr(" some similar " + context + "s exist: ")
		}
		for i, possible := range possibles.F0 {
			if i != 0 {
				styled.PushStr(", ")
			}
			styled.PushStr("'" + valid.Render() + possible + valid.RenderReset() + "'")
		}
	}
}
