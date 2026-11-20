// Port of clap_builder 4.6.0 src/error/mod.rs

package clap

import (
	"os"

	"github.com/BrokkAi/mjolnir/go/rs"
)

// Error is clap::Error (clap::error::Error with the default formatter,
// RichFormatter): command Line Argument Parser Error.
//
// See Command::error to create an error.
//
// It implements Go's error: Error() is the Display text, and Unwrap()
// gives the source.
type Error struct {
	inner *errorInner
}

type errorInner struct {
	kind    ErrorErrorKind
	context utilFlatMap[ErrorContextKind, ErrorContextValue]
	message rs.Option[errorMessage]
	source  rs.Option[error]
	// helpFlag is the help flag to suggest.
	helpFlag      rs.Option[string]
	styles        BuilderStyles
	colorWhen     ColorChoice
	colorHelpWhen ColorChoice
	// backtrace: the crate's `debug` feature is off, so there is none.
}

// ErrorRaw is Error::raw: create an unformatted error.
//
// This is for you need to pass the error up to a place that has access to
// the Command at which point you can call Error::format.
//
// Prefer Command::error for generating errors.
func ErrorRaw(kind ErrorErrorKind, message string) *Error {
	return ErrorNew(kind).setMessage(&errorMessage_Raw{F0: message})
}

// Format is Error::format: format the existing message with the Command's
// context.
func (e *Error) Format(cmd *Command) *Error {
	cmd.buildSelf(false)
	usage := cmd.renderUsage_()
	if message, ok := e.inner.message.Get(); ok {
		e.inner.message = rs.Some(message.format(cmd, usage))
	}
	return e.WithCmd(cmd)
}

// ErrorNew is Error::new: create an error with a pre-defined message.
//
// See also
//   - Error::insert
//   - Error::with_cmd
func ErrorNew(kind ErrorErrorKind) *Error {
	return &Error{
		inner: &errorInner{
			kind:          kind,
			message:       rs.None[errorMessage](),
			source:        rs.None[error](),
			helpFlag:      rs.None[string](),
			styles:        BuilderStylesPlain(),
			colorWhen:     ColorChoice_Never,
			colorHelpWhen: ColorChoice_Never,
		},
	}
}

// WithCmd is Error::with_cmd: apply Command's formatting to the error.
//
// Generally, this is used with Error::new.
func (e *Error) WithCmd(cmd *Command) *Error {
	return e.setStyles(cmd.GetStyles().Clone()).
		setColor(cmd.GetColor()).
		setColoredHelp(cmd.colorHelp()).
		setHelpFlag(getHelpFlag(cmd))
}

// Apply is Error::apply: apply an alternative formatter to the error.
//
// The port keeps one formatter, RichFormatter; Rust's
// `Error::<KindFormatter>` has no Go spelling.
func (e *Error) Apply() *Error {
	panic(rs.Todo("clap::Error::apply: an Error formatted by another ErrorFormatter; Mjolnir uses only RichFormatter"))
}

// Kind is Error::kind: type of error for programmatic processing.
func (e *Error) Kind() ErrorErrorKind {
	return e.inner.kind
}

// ErrorContextEntry is one `(ContextKind, &ContextValue)` of
// Error::context.
type ErrorContextEntry struct {
	Kind  ErrorContextKind
	Value ErrorContextValue
}

// Context is Error::context: additional information to further qualify
// the error.
func (e *Error) Context() []ErrorContextEntry {
	entries := make([]ErrorContextEntry, 0, e.inner.context.len())
	for i, k := range e.inner.context.keys {
		entries = append(entries, ErrorContextEntry{Kind: k, Value: e.inner.context.values[i]})
	}
	return entries
}

// Get is Error::get: lookup a piece of context.
func (e *Error) Get(kind ErrorContextKind) rs.Option[ErrorContextValue] {
	if value := e.inner.context.get(kind); value != nil {
		return rs.Some(*value)
	}
	return rs.None[ErrorContextValue]()
}

// Insert is Error::insert: insert a piece of context.
//
// If this `ContextKind` is already present, its value is replaced and the
// old value is returned.
func (e *Error) Insert(kind ErrorContextKind, value ErrorContextValue) rs.Option[ErrorContextValue] {
	return rs.OptionOf(e.inner.context.insert(kind, value))
}

// Remove is Error::remove: remove a piece of context, return the old value
// if any.
//
// The context is currently implemented in a vector, so `remove` takes
// linear time.
func (e *Error) Remove(kind ErrorContextKind) rs.Option[ErrorContextValue] {
	return rs.OptionOf(e.inner.context.remove(kind))
}

// UseStderr is Error::use_stderr: should the message be written to
// `stdout` or not?
func (e *Error) UseStderr() bool {
	return e.stream() == outputFmtStream_Stderr
}

func (e *Error) stream() outputFmtStream {
	switch e.Kind() {
	case ErrorErrorKind_DisplayHelp, ErrorErrorKind_DisplayVersion:
		return outputFmtStream_Stdout
	default:
		return outputFmtStream_Stderr
	}
}

// ExitCode is Error::exit_code: returns the exit code that `.exit` will
// exit the process with.
//
// When the error's kind would print to `stderr` this returns `2`, else it
// returns `0`.
func (e *Error) ExitCode() int {
	if e.UseStderr() {
		return utilUsageCode
	}
	return utilSuccessCode
}

// Exit is Error::exit: prints the error and exits.
//
// Depending on the error kind, this either prints to `stderr` and exits
// with a status of `2` or prints to `stdout` and exits with a status of
// `0`.
func (e *Error) Exit() {
	// Swallow broken pipe errors
	_ = e.Print()
	os.Exit(e.ExitCode())
}

// Print is Error::print: prints formatted and colored error to `stdout` or
// `stderr` according to its error kind.
func (e *Error) Print() error {
	style := e.formatted()
	var colorWhen ColorChoice
	switch e.Kind() {
	case ErrorErrorKind_DisplayHelp, ErrorErrorKind_DisplayHelpOnMissingArgumentOrSubcommand:
		colorWhen = e.inner.colorHelpWhen
	default:
		colorWhen = e.inner.colorWhen
	}
	c := outputFmtColorizerNew(e.stream(), colorWhen).withContent(style)
	return c.print()
}

// Render is Error::render: render the error message to a StyledStr.
func (e *Error) Render() BuilderStyledStr {
	return e.formatted()
}

func errorForApp(kind ErrorErrorKind, cmd *Command, styled BuilderStyledStr) *Error {
	return ErrorNew(kind).setMessage(&errorMessage_Formatted{F0: styled}).WithCmd(cmd)
}

func (e *Error) setMessage(message errorMessage) *Error {
	e.inner.message = rs.Some(message)
	return e
}

func (e *Error) setSource(source error) *Error {
	e.inner.source = rs.Some(source)
	return e
}

func (e *Error) setStyles(styles BuilderStyles) *Error {
	e.inner.styles = styles
	return e
}

func (e *Error) setColor(colorWhen ColorChoice) *Error {
	e.inner.colorWhen = colorWhen
	return e
}

func (e *Error) setColoredHelp(colorHelpWhen ColorChoice) *Error {
	e.inner.colorHelpWhen = colorHelpWhen
	return e
}

func (e *Error) setHelpFlag(helpFlag rs.Option[string]) *Error {
	e.inner.helpFlag = helpFlag
	return e
}

// insertContextUnchecked does not verify if `ContextKind` is already
// present.
func (e *Error) insertContextUnchecked(kind ErrorContextKind, value ErrorContextValue) *Error {
	e.inner.context.insertUnchecked(kind, value)
	return e
}

// extendContextUnchecked does not verify if `ContextKind` is already
// present.
func (e *Error) extendContextUnchecked(context ...ErrorContextEntry) *Error {
	for _, entry := range context {
		e.inner.context.insertUnchecked(entry.Kind, entry.Value)
	}
	return e
}

func errorDisplayHelp(cmd *Command, styled BuilderStyledStr) *Error {
	return errorForApp(ErrorErrorKind_DisplayHelp, cmd, styled)
}

func errorDisplayHelpError(cmd *Command, styled BuilderStyledStr) *Error {
	return errorForApp(ErrorErrorKind_DisplayHelpOnMissingArgumentOrSubcommand, cmd, styled)
}

func errorDisplayVersion(cmd *Command, styled BuilderStyledStr) *Error {
	return errorForApp(ErrorErrorKind_DisplayVersion, cmd, styled)
}

// errorOthers is the `match others.len()` of argument_conflict and
// subcommand_conflict.
func errorOthers(others []string) ErrorContextValue {
	switch len(others) {
	case 0:
		return &ErrorContextValue_None{}
	case 1:
		return &ErrorContextValue_String{F0: others[0]}
	default:
		return &ErrorContextValue_Strings{F0: others}
	}
}

func errorArgumentConflict(cmd *Command, arg string, others []string, usage rs.Option[BuilderStyledStr]) *Error {
	err := ErrorNew(ErrorErrorKind_ArgumentConflict).WithCmd(cmd)

	err = err.extendContextUnchecked(
		ErrorContextEntry{ErrorContextKind_InvalidArg, &ErrorContextValue_String{F0: arg}},
		ErrorContextEntry{ErrorContextKind_PriorArg, errorOthers(others)},
	)
	if usage, ok := usage.Get(); ok {
		err = err.insertContextUnchecked(ErrorContextKind_Usage, &ErrorContextValue_StyledStr{F0: usage})
	}

	return err
}

func errorSubcommandConflict(cmd *Command, sub string, others []string, usage rs.Option[BuilderStyledStr]) *Error {
	err := ErrorNew(ErrorErrorKind_ArgumentConflict).WithCmd(cmd)

	err = err.extendContextUnchecked(
		ErrorContextEntry{ErrorContextKind_InvalidSubcommand, &ErrorContextValue_String{F0: sub}},
		ErrorContextEntry{ErrorContextKind_PriorArg, errorOthers(others)},
	)
	if usage, ok := usage.Get(); ok {
		err = err.insertContextUnchecked(ErrorContextKind_Usage, &ErrorContextValue_StyledStr{F0: usage})
	}

	return err
}

func errorEmptyValue(cmd *Command, goodVals []string, arg string) *Error {
	return errorInvalidValue(cmd, "", goodVals, arg)
}

func errorNoEquals(cmd *Command, arg string, usage rs.Option[BuilderStyledStr]) *Error {
	err := ErrorNew(ErrorErrorKind_NoEquals).WithCmd(cmd)

	err = err.extendContextUnchecked(
		ErrorContextEntry{ErrorContextKind_InvalidArg, &ErrorContextValue_String{F0: arg}},
	)
	if usage, ok := usage.Get(); ok {
		err = err.insertContextUnchecked(ErrorContextKind_Usage, &ErrorContextValue_StyledStr{F0: usage})
	}

	return err
}

func errorInvalidValue(cmd *Command, badVal string, goodVals []string, arg string) *Error {
	suggestion := rs.None[string]()
	if found := suggestionsDidYouMean(badVal, goodVals); len(found) > 0 {
		suggestion = rs.Some(found[len(found)-1])
	}
	err := ErrorNew(ErrorErrorKind_InvalidValue).WithCmd(cmd)

	err = err.extendContextUnchecked(
		ErrorContextEntry{ErrorContextKind_InvalidArg, &ErrorContextValue_String{F0: arg}},
		ErrorContextEntry{ErrorContextKind_InvalidValue, &ErrorContextValue_String{F0: badVal}},
		ErrorContextEntry{ErrorContextKind_ValidValue, &ErrorContextValue_Strings{F0: append([]string{}, goodVals...)}},
	)
	if suggestion, ok := suggestion.Get(); ok {
		err = err.insertContextUnchecked(ErrorContextKind_SuggestedValue, &ErrorContextValue_String{F0: suggestion})
	}

	return err
}

func errorInvalidSubcommand(cmd *Command, subcmd string, didYouMean []string, name string, suggestedTrailingArg bool, usage rs.Option[BuilderStyledStr]) *Error {
	styles := cmd.GetStyles()
	invalid := styles.GetInvalid()
	valid := styles.GetValid()
	err := ErrorNew(ErrorErrorKind_InvalidSubcommand).WithCmd(cmd)

	suggestions := []BuilderStyledStr{}
	if suggestedTrailingArg {
		styledSuggestion := BuilderStyledStrNew()
		styledSuggestion.PushStr("to pass '" + invalid.Render() + subcmd + invalid.RenderReset() + "' as a value, use '" + valid.Render() + name + " -- " + subcmd + valid.RenderReset() + "'")
		suggestions = append(suggestions, styledSuggestion)
	}

	err = err.extendContextUnchecked(
		ErrorContextEntry{ErrorContextKind_InvalidSubcommand, &ErrorContextValue_String{F0: subcmd}},
		ErrorContextEntry{ErrorContextKind_SuggestedSubcommand, &ErrorContextValue_Strings{F0: didYouMean}},
		ErrorContextEntry{ErrorContextKind_Suggested, &ErrorContextValue_StyledStrs{F0: suggestions}},
	)
	if usage, ok := usage.Get(); ok {
		err = err.insertContextUnchecked(ErrorContextKind_Usage, &ErrorContextValue_StyledStr{F0: usage})
	}

	return err
}

func errorUnrecognizedSubcommand(cmd *Command, subcmd string, usage rs.Option[BuilderStyledStr]) *Error {
	err := ErrorNew(ErrorErrorKind_InvalidSubcommand).WithCmd(cmd)

	err = err.extendContextUnchecked(
		ErrorContextEntry{ErrorContextKind_InvalidSubcommand, &ErrorContextValue_String{F0: subcmd}},
	)
	if usage, ok := usage.Get(); ok {
		err = err.insertContextUnchecked(ErrorContextKind_Usage, &ErrorContextValue_StyledStr{F0: usage})
	}

	return err
}

func errorMissingRequiredArgument(cmd *Command, required []string, usage rs.Option[BuilderStyledStr]) *Error {
	err := ErrorNew(ErrorErrorKind_MissingRequiredArgument).WithCmd(cmd)

	err = err.extendContextUnchecked(
		ErrorContextEntry{ErrorContextKind_InvalidArg, &ErrorContextValue_Strings{F0: required}},
	)
	if usage, ok := usage.Get(); ok {
		err = err.insertContextUnchecked(ErrorContextKind_Usage, &ErrorContextValue_StyledStr{F0: usage})
	}

	return err
}

func errorMissingSubcommand(cmd *Command, parent string, available []string, usage rs.Option[BuilderStyledStr]) *Error {
	err := ErrorNew(ErrorErrorKind_MissingSubcommand).WithCmd(cmd)

	err = err.extendContextUnchecked(
		ErrorContextEntry{ErrorContextKind_InvalidSubcommand, &ErrorContextValue_String{F0: parent}},
		ErrorContextEntry{ErrorContextKind_ValidSubcommand, &ErrorContextValue_Strings{F0: available}},
	)
	if usage, ok := usage.Get(); ok {
		err = err.insertContextUnchecked(ErrorContextKind_Usage, &ErrorContextValue_StyledStr{F0: usage})
	}

	return err
}

func errorInvalidUtf8(cmd *Command, usage rs.Option[BuilderStyledStr]) *Error {
	err := ErrorNew(ErrorErrorKind_InvalidUtf8).WithCmd(cmd)

	if usage, ok := usage.Get(); ok {
		err = err.insertContextUnchecked(ErrorContextKind_Usage, &ErrorContextValue_StyledStr{F0: usage})
	}

	return err
}

func errorTooManyValues(cmd *Command, val string, arg string, usage rs.Option[BuilderStyledStr]) *Error {
	err := ErrorNew(ErrorErrorKind_TooManyValues).WithCmd(cmd)

	err = err.extendContextUnchecked(
		ErrorContextEntry{ErrorContextKind_InvalidArg, &ErrorContextValue_String{F0: arg}},
		ErrorContextEntry{ErrorContextKind_InvalidValue, &ErrorContextValue_String{F0: val}},
	)
	if usage, ok := usage.Get(); ok {
		err = err.insertContextUnchecked(ErrorContextKind_Usage, &ErrorContextValue_StyledStr{F0: usage})
	}

	return err
}

func errorTooFewValues(cmd *Command, arg string, minVals int, currVals int, usage rs.Option[BuilderStyledStr]) *Error {
	err := ErrorNew(ErrorErrorKind_TooFewValues).WithCmd(cmd)

	err = err.extendContextUnchecked(
		ErrorContextEntry{ErrorContextKind_InvalidArg, &ErrorContextValue_String{F0: arg}},
		ErrorContextEntry{ErrorContextKind_MinValues, &ErrorContextValue_Number{F0: minVals}},
		ErrorContextEntry{ErrorContextKind_ActualNumValues, &ErrorContextValue_Number{F0: currVals}},
	)
	if usage, ok := usage.Get(); ok {
		err = err.insertContextUnchecked(ErrorContextKind_Usage, &ErrorContextValue_StyledStr{F0: usage})
	}

	return err
}

func errorValueValidation(arg string, val string, source error) *Error {
	err := ErrorNew(ErrorErrorKind_ValueValidation).setSource(source)

	err = err.extendContextUnchecked(
		ErrorContextEntry{ErrorContextKind_InvalidArg, &ErrorContextValue_String{F0: arg}},
		ErrorContextEntry{ErrorContextKind_InvalidValue, &ErrorContextValue_String{F0: val}},
	)

	return err
}

func errorWrongNumberOfValues(cmd *Command, arg string, numVals int, currVals int, usage rs.Option[BuilderStyledStr]) *Error {
	err := ErrorNew(ErrorErrorKind_WrongNumberOfValues).WithCmd(cmd)

	err = err.extendContextUnchecked(
		ErrorContextEntry{ErrorContextKind_InvalidArg, &ErrorContextValue_String{F0: arg}},
		ErrorContextEntry{ErrorContextKind_ExpectedNumValues, &ErrorContextValue_Number{F0: numVals}},
		ErrorContextEntry{ErrorContextKind_ActualNumValues, &ErrorContextValue_Number{F0: currVals}},
	)
	if usage, ok := usage.Get(); ok {
		err = err.insertContextUnchecked(ErrorContextKind_Usage, &ErrorContextValue_StyledStr{F0: usage})
	}

	return err
}

// errorDidYouMean is the `(String, Option<String>)` unknown_argument takes:
// the suggested flag, and the subcommand that has it.
type errorDidYouMean struct {
	flag string
	sub  rs.Option[string]
}

func errorUnknownArgument(cmd *Command, arg string, didYouMean rs.Option[errorDidYouMean], suggestedTrailingArg bool, usage rs.Option[BuilderStyledStr]) *Error {
	styles := cmd.GetStyles()
	invalid := styles.GetInvalid()
	valid := styles.GetValid()
	err := ErrorNew(ErrorErrorKind_UnknownArgument).WithCmd(cmd)

	suggestions := []BuilderStyledStr{}
	if suggestedTrailingArg {
		styledSuggestion := BuilderStyledStrNew()
		styledSuggestion.PushStr("to pass '" + invalid.Render() + arg + invalid.RenderReset() + "' as a value, use '" + valid.Render() + "-- " + arg + valid.RenderReset() + "'")
		suggestions = append(suggestions, styledSuggestion)
	}

	err = err.extendContextUnchecked(
		ErrorContextEntry{ErrorContextKind_InvalidArg, &ErrorContextValue_String{F0: arg}},
	)
	if usage, ok := usage.Get(); ok {
		err = err.insertContextUnchecked(ErrorContextKind_Usage, &ErrorContextValue_StyledStr{F0: usage})
	}
	if didYouMean, ok := didYouMean.Get(); ok {
		if sub, ok := didYouMean.sub.Get(); ok {
			styledSuggestion := BuilderStyledStrNew()
			styledSuggestion.PushStr("'" + valid.Render() + sub + " " + didYouMean.flag + valid.RenderReset() + "' exists")
			suggestions = append(suggestions, styledSuggestion)
		} else {
			err = err.insertContextUnchecked(ErrorContextKind_SuggestedArg, &ErrorContextValue_String{F0: didYouMean.flag})
		}
	}
	if len(suggestions) != 0 {
		err = err.insertContextUnchecked(ErrorContextKind_Suggested, &ErrorContextValue_StyledStrs{F0: suggestions})
	}

	return err
}

func errorUnnecessaryDoubleDash(cmd *Command, arg string, usage rs.Option[BuilderStyledStr]) *Error {
	styles := cmd.GetStyles()
	invalid := styles.GetInvalid()
	valid := styles.GetValid()
	err := ErrorNew(ErrorErrorKind_UnknownArgument).WithCmd(cmd)

	styledSuggestion := BuilderStyledStrNew()
	styledSuggestion.PushStr("subcommand '" + valid.Render() + arg + valid.RenderReset() + "' exists; to use it, remove the '" + invalid.Render() + "--" + invalid.RenderReset() + "' before it")

	err = err.extendContextUnchecked(
		ErrorContextEntry{ErrorContextKind_InvalidArg, &ErrorContextValue_String{F0: arg}},
		ErrorContextEntry{ErrorContextKind_Suggested, &ErrorContextValue_StyledStrs{F0: []BuilderStyledStr{styledSuggestion}}},
	)
	if usage, ok := usage.Get(); ok {
		err = err.insertContextUnchecked(ErrorContextKind_Usage, &ErrorContextValue_StyledStr{F0: usage})
	}

	return err
}

func (e *Error) formatted() BuilderStyledStr {
	if message, ok := e.inner.message.Get(); ok {
		return message.formatted(&e.inner.styles)
	}
	return ErrorRichFormatter{}.FormatError(e)
}

// ErrorFromIoError is `impl From<io::Error> for Error`.
func ErrorFromIoError(e error) *Error {
	return ErrorRaw(ErrorErrorKind_Io, e.Error())
}

// ErrorFromFmtError is `impl From<fmt::Error> for Error`.
func ErrorFromFmtError(e error) *Error {
	return ErrorRaw(ErrorErrorKind_Format, e.Error())
}

// Unwrap is `impl std::error::Error for Error`'s source.
func (e *Error) Unwrap() error {
	if source, ok := e.inner.source.Get(); ok {
		return source
	}
	return nil
}

// String is `impl Display for Error`.
func (e *Error) String() string {
	// Assuming `self.message` already has a trailing newline, from `try_help` or similar
	return e.formatted().String()
}

// Error is `impl Display for Error`, so that an Error is a Go error.
func (e *Error) Error() string { return e.String() }

// FmtDisplay is `impl Display for Error`.
func (e *Error) FmtDisplay(f *rs.Formatter) error {
	return e.formatted().FmtDisplay(f)
}

// FmtDebug is `impl Debug for Error`: the derived Debug of its inner state.
func (e *Error) FmtDebug(f *rs.Formatter) error {
	panic(rs.Todo("clap::Error: Debug: the derived Debug of ErrorInner (FlatMap, Styles, anstyle) is not ported"))
}

// errorMessage is Message: the error's text, raw or formatted.
type errorMessage interface {
	isErrorMessage()
	format(cmd *Command, usage rs.Option[BuilderStyledStr]) errorMessage
	formatted(styles *BuilderStyles) BuilderStyledStr
}

type errorMessage_Raw struct{ F0 string }

type errorMessage_Formatted struct{ F0 BuilderStyledStr }

func (*errorMessage_Raw) isErrorMessage()       {}
func (*errorMessage_Formatted) isErrorMessage() {}

var _ = rs.RegisterEnum[errorMessage]("Message",
	rs.Variant[errorMessage_Raw]("Raw", rs.ShapeTuple),
	rs.Variant[errorMessage_Formatted]("Formatted", rs.ShapeTuple),
)

func (m *errorMessage_Raw) format(cmd *Command, usage rs.Option[BuilderStyledStr]) errorMessage {
	styled := formatErrorMessage(m.F0, cmd.GetStyles(), cmd, usage)
	return &errorMessage_Formatted{F0: styled}
}

func (m *errorMessage_Formatted) format(*Command, rs.Option[BuilderStyledStr]) errorMessage {
	return m
}

func (m *errorMessage_Raw) formatted(styles *BuilderStyles) BuilderStyledStr {
	return formatErrorMessage(m.F0, styles, nil, rs.None[BuilderStyledStr]())
}

func (m *errorMessage_Formatted) formatted(*BuilderStyles) BuilderStyledStr {
	return m.F0
}

// errorFromAny turns the error a TypedValueParser returned into the
// clap::Error Rust's type system guarantees it is.
func errorFromAny(err error) *Error {
	if clapErr, ok := err.(*Error); ok {
		return clapErr
	}
	rs.PanicAt(1, "a TypedValueParser returned an error that is not a *clap.Error: "+err.Error())
	return nil
}
